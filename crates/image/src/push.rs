//! `docker push`: pack each layer directory into a tarball, upload missing blobs, publish the manifest.
use crate::{
    auth,
    error::{err, Result},
    pack::pack,
    reference::Reference,
    remote::{host_arch, Client},
    store::{ImageMeta, Store},
};
use serde_json::{json, Map, Value};
use std::fs;

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// The image config JSON (the same shape `pull` reads back).
pub fn config_json(meta: &ImageMeta, diff_ids: &[String]) -> Value {
    let mut c = Map::new();
    for (key, list) in [("Entrypoint", &meta.entrypoint), ("Cmd", &meta.cmd), ("Env", &meta.env)] {
        if !list.is_empty() {
            c.insert(key.into(), json!(list));
        }
    }
    if !meta.working_dir.is_empty() {
        c.insert("WorkingDir".into(), json!(meta.working_dir));
    }
    if !meta.user.is_empty() {
        c.insert("User".into(), json!(meta.user));
    }
    if !meta.exposed.is_empty() {
        c.insert("ExposedPorts".into(), meta.exposed.iter().map(|p| (p.clone(), json!({}))).collect::<Map<_, _>>().into());
    }
    if !meta.labels.is_empty() {
        c.insert("Labels".into(), json!(meta.labels));
    }
    if let Some(h) = &meta.healthcheck {
        let ns = |s: u64| s * 1_000_000_000;
        let test: Vec<String> = std::iter::once("CMD".to_string()).chain(h.test.iter().cloned()).collect();
        c.insert(
            "Healthcheck".into(),
            json!({
                "Test": test,
                "Interval": ns(h.interval_s), "Timeout": ns(h.timeout_s), "StartPeriod": ns(h.start_period_s), "Retries": h.retries,
            }),
        );
    }
    json!({
        "architecture": host_arch(),
        "os": "linux",
        "config": c,
        "rootfs": { "type": "layers", "diff_ids": diff_ids },
    })
}

pub fn manifest_json(config: (&str, u64), layers: &[(String, u64)]) -> Value {
    json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": { "mediaType": OCI_CONFIG, "digest": config.0, "size": config.1 },
        "layers": layers.iter().map(|(d, s)| json!({ "mediaType": OCI_LAYER, "digest": d, "size": s })).collect::<Vec<_>>(),
    })
}

/// Push `r` (which must already be in the store, e.g. via `docker tag`). Returns the manifest digest.
pub fn push(store: &Store, r: &Reference, say: &mut dyn FnMut(String)) -> Result<String> {
    let meta = store.get(r)?.ok_or_else(|| err(format!("no such image: {}", r.display())))?;
    let creds = auth::load(store.root(), &r.registry);
    let client = Client::connect(r, &store.tmp_dir(), creds.as_ref(), true)?;
    let work = store.new_staging_dir("push")?;
    let result = push_layers(store, &client, r, &meta, &work, say);
    let _ = fs::remove_dir_all(&work);
    result
}

fn push_layers(store: &Store, client: &Client, r: &Reference, meta: &ImageMeta, work: &std::path::Path, say: &mut dyn FnMut(String)) -> Result<String> {
    let (mut layers, mut diff_ids) = (vec![], vec![]);
    let blob = work.join("blob");
    for l in &meta.layers {
        let dir = store.layer_path(&l.digest)?;
        if !dir.is_dir() {
            return Err(err(format!("layer {} is missing from the store; pull or rebuild the image", &l.digest[7..19])));
        }
        let short = l.digest[7..19].to_string();
        let p = pack(&dir, &blob)?;
        if client.blob_exists(&p.digest)? {
            say(format!("{short}: Layer already exists"));
        } else {
            say(format!("{short}: Pushing"));
            client.upload_blob(&p.digest, &blob)?;
            say(format!("{short}: Pushed"));
        }
        let _ = fs::remove_file(&blob);
        layers.push((p.digest, p.size));
        diff_ids.push(p.diff_id);
    }

    let config = serde_json::to_vec(&config_json(meta, &diff_ids))?;
    let config_digest = format!("sha256:{}", hex(&config));
    fs::write(&blob, &config)?;
    if !client.blob_exists(&config_digest)? {
        client.upload_blob(&config_digest, &blob)?;
    }
    let manifest = serde_json::to_vec(&manifest_json((&config_digest, config.len() as u64), &layers))?;
    fs::write(&blob, &manifest)?;
    let digest = client.put_manifest(&r.tag, &blob, OCI_MANIFEST)?;
    say(format!("{}: digest: {digest} size: {}", r.tag, manifest.len()));
    Ok(digest)
}

fn hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn config_round_trips_through_the_shape_pull_reads() {
        let meta = ImageMeta {
            name: "app:1".into(),
            layers: vec![],
            entrypoint: vec!["/ep".into()],
            cmd: vec!["run".into()],
            env: vec!["A=1".into()],
            working_dir: "/app".into(),
            user: "app".into(),
            exposed: vec!["80/tcp".into()],
            labels: BTreeMap::from([("k".to_string(), "v".to_string())]),
            healthcheck: Some(crate::store::Health::new(vec!["/bin/check".into()])),
        };
        let v = config_json(&meta, &["sha256:d1".into()]);
        assert_eq!(v["os"], "linux");
        assert_eq!(v["config"]["Entrypoint"][0], "/ep");
        assert_eq!(v["config"]["WorkingDir"], "/app");
        assert!(v["config"]["ExposedPorts"]["80/tcp"].is_object());
        assert_eq!(v["config"]["Labels"]["k"], "v");
        assert_eq!(v["rootfs"]["diff_ids"][0], "sha256:d1");
        assert_eq!(v["config"]["Healthcheck"]["Test"][1], "/bin/check");
        assert_eq!(v["config"]["Healthcheck"]["Interval"], 30_000_000_000u64);
    }

    #[test]
    fn empty_fields_are_omitted() {
        let meta = ImageMeta {
            name: "x".into(),
            layers: vec![],
            entrypoint: vec![],
            cmd: vec![],
            env: vec![],
            working_dir: String::new(),
            user: String::new(),
            exposed: vec![],
            labels: BTreeMap::new(),
            healthcheck: None,
        };
        assert!(config_json(&meta, &[])["config"].as_object().unwrap().is_empty());
    }

    #[test]
    fn manifest_lists_config_and_layers() {
        let m = manifest_json(("sha256:c", 10), &[("sha256:l1".into(), 5), ("sha256:l2".into(), 6)]);
        assert_eq!(m["schemaVersion"], 2);
        assert_eq!(m["config"]["digest"], "sha256:c");
        assert_eq!(m["layers"][1]["size"], 6);
        assert_eq!(m["layers"][0]["mediaType"], OCI_LAYER);
    }
}
