//! Extra per-image facts the run-time record (`ImageMeta`) does not carry: config digest (the image ID),
//! manifest digest, creation time and build history. Kept beside the store in `imageinfo/<key>.json`,
//! so `docker image inspect` and `docker history` work. Images with no record (built locally) still work, with less detail.
use crate::{
    error::{err, Result},
    reference::Reference,
    store::{ImageMeta, Store},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub empty_layer: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageInfo {
    /// Digest of the image config (Docker's image ID).
    pub id: Option<String>,
    /// Digest of the manifest as the registry knows it.
    pub digest: Option<String>,
    pub created: Option<String>,
    pub architecture: Option<String>,
    pub os: Option<String>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

fn file(store: &Store, r: &Reference) -> PathBuf {
    store.root().join("imageinfo").join(format!("{}.json", r.key()))
}

pub fn load(store: &Store, r: &Reference) -> ImageInfo {
    fs::read(file(store, r)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(store: &Store, r: &Reference, info: &ImageInfo) -> Result<()> {
    let f = file(store, r);
    fs::create_dir_all(f.parent().expect("has parent"))?;
    fs::write(f, serde_json::to_vec_pretty(info)?)?;
    Ok(())
}

pub fn forget(store: &Store, r: &Reference) {
    let _ = fs::remove_file(file(store, r));
}

/// Read what we keep from an image config JSON.
pub fn from_config(id: Option<String>, digest: Option<String>, config: &[u8]) -> ImageInfo {
    let v: Value = serde_json::from_slice(config).unwrap_or(Value::Null);
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    ImageInfo {
        id,
        digest,
        created: s("created"),
        architecture: s("architecture"),
        os: s("os"),
        history: v.get("history").and_then(|h| serde_json::from_value(h.clone()).ok()).unwrap_or_default(),
    }
}

/// Remember the facts about an image that was just pulled or loaded (best effort: never fails the pull).
pub fn record(store: &Store, r: &Reference, id: Option<String>, digest: Option<String>, config: &[u8]) {
    let _ = save(store, r, &from_config(id, digest, config));
}

/// After a push: the registry now knows the image under this manifest digest.
pub fn set_digest(store: &Store, r: &Reference, digest: &str) {
    let mut i = load(store, r);
    i.digest = Some(digest.to_string());
    let _ = save(store, r, &i);
}

/// Docker-style `inspect` document for an image.
pub fn inspect(store: &Store, r: &Reference) -> Result<Value> {
    let meta = store.get(r)?.ok_or_else(|| err(format!("no such image: {}", r.display())))?;
    let i = load(store, r);
    Ok(inspect_json(&meta, &i, &r.registry, &r.repo))
}

fn inspect_json(meta: &ImageMeta, i: &ImageInfo, registry: &str, repo: &str) -> Value {
    let name = if registry == crate::reference::HUB { repo.to_string() } else { format!("{registry}/{repo}") };
    json!({
        "Id": i.id,
        "RepoTags": [meta.name],
        "RepoDigests": i.digest.iter().map(|d| format!("{name}@{d}")).collect::<Vec<_>>(),
        "Created": i.created,
        "Architecture": i.architecture,
        "Os": i.os,
        "Size": meta.size(),
        "Config": {
            "Entrypoint": meta.entrypoint, "Cmd": meta.cmd, "Env": meta.env, "WorkingDir": meta.working_dir,
            "User": meta.user, "ExposedPorts": meta.exposed, "Labels": meta.labels,
        },
        // note: digests of the layer blobs as stored (compressed), not the config's uncompressed diff_ids
        "RootFS": { "Type": "layers", "Layers": meta.layers.iter().map(|l| l.digest.clone()).collect::<Vec<_>>() },
    })
}

pub struct HistoryRow {
    pub created: String,
    pub created_by: String,
    pub size: u64,
}

/// Build history, newest first. Entries that made a layer get that layer's size; the rest are 0.
pub fn history(store: &Store, r: &Reference) -> Result<Vec<HistoryRow>> {
    let meta = store.get(r)?.ok_or_else(|| err(format!("no such image: {}", r.display())))?;
    Ok(history_rows(&meta, &load(store, r)))
}

fn history_rows(meta: &ImageMeta, i: &ImageInfo) -> Vec<HistoryRow> {
    let mut layers = meta.layers.iter();
    let mut rows: Vec<HistoryRow> = i
        .history
        .iter()
        .map(|h| HistoryRow {
            created: h.created.clone(),
            created_by: h.created_by.clone(),
            size: if h.empty_layer { 0 } else { layers.next().map_or(0, |l| l.size) },
        })
        .collect();
    if rows.is_empty() {
        // no record (locally built / old store): one row per layer
        rows = meta.layers.iter().map(|l| HistoryRow { created: String::new(), created_by: "<missing>".into(), size: l.size }).collect();
    }
    rows.reverse();
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LayerRef;
    use std::collections::BTreeMap;

    fn meta(sizes: &[u64]) -> ImageMeta {
        ImageMeta {
            name: "a:1".into(),
            layers: sizes.iter().map(|s| LayerRef { digest: format!("sha256:{s}"), media_type: String::new(), size: *s }).collect(),
            entrypoint: vec![],
            cmd: vec![],
            env: vec![],
            working_dir: String::new(),
            user: String::new(),
            exposed: vec![],
            labels: BTreeMap::new(),
            healthcheck: None,
        }
    }

    #[test]
    fn parses_config_history() {
        let cfg = br#"{"created":"2024-01-02T03:04:05Z","architecture":"arm64","os":"linux","history":[
            {"created":"t1","created_by":"ADD rootfs"},{"created":"t2","created_by":"ENV A=1","empty_layer":true},{"created":"t3","created_by":"RUN x"}]}"#;
        let i = from_config(Some("sha256:cfg".into()), Some("sha256:m".into()), cfg);
        assert_eq!(i.created.as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(i.history.len(), 3);
        assert!(i.history[1].empty_layer);
        // 2 layers for the 2 non-empty entries; newest first
        let rows = history_rows(&meta(&[100, 200]), &i);
        assert_eq!(rows.iter().map(|r| (r.created_by.as_str(), r.size)).collect::<Vec<_>>(), [("RUN x", 200), ("ENV A=1", 0), ("ADD rootfs", 100)]);
        let v = inspect_json(&meta(&[100, 200]), &i, "ghcr.io", "o/a");
        assert_eq!(v["Id"], "sha256:cfg");
        assert_eq!(v["RepoDigests"][0], "ghcr.io/o/a@sha256:m");
        assert_eq!(v["Size"], 300);
        assert_eq!(v["RootFS"]["Layers"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn history_without_a_record_lists_layers() {
        let rows = history_rows(&meta(&[5, 6]), &ImageInfo::default());
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].size, rows[0].created_by.as_str()), (6, "<missing>"));
    }
}
