use crate::{
    error::{err, Result},
    reference::Reference,
    remote::{host_arch, pick_platform, Client},
    store::{Health, ImageMeta, LayerRef, Store},
};
use serde::Deserialize;
use std::fs;

#[derive(Deserialize)]
struct ConfigFile {
    config: Option<RunConfig>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct RunConfig {
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    env: Option<Vec<String>>,
    working_dir: Option<String>,
    user: Option<String>,
    exposed_ports: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    labels: Option<std::collections::BTreeMap<String, String>>,
    healthcheck: Option<HealthConfig>,
}

/// Durations in image configs are nanoseconds.
#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct HealthConfig {
    test: Option<Vec<String>>,
    interval: Option<u64>,
    timeout: Option<u64>,
    start_period: Option<u64>,
    retries: Option<u32>,
}

fn health_from(h: HealthConfig) -> Option<Health> {
    let t = h.test?;
    let test = match t.first().map(String::as_str) {
        Some("CMD-SHELL") => vec!["/bin/sh".to_string(), "-c".to_string(), t[1..].join(" ")],
        Some("CMD") => t[1..].to_vec(),
        _ => return None, // NONE or unknown
    };
    let secs = |ns: Option<u64>, default: u64| ns.filter(|n| *n > 0).map_or(default, |n| (n / 1_000_000_000).max(1));
    Some(Health {
        test,
        interval_s: secs(h.interval, 30),
        timeout_s: secs(h.timeout, 30),
        retries: h.retries.filter(|r| *r > 0).unwrap_or(3),
        start_period_s: h.start_period.map_or(0, |n| n / 1_000_000_000),
    })
}

/// Download an image (config + layer blobs, digest-verified) into the store.
/// Layers already present are skipped, which is where the disk savings come from.
pub fn pull(store: &Store, r: &Reference, say: &mut dyn FnMut(String)) -> Result<ImageMeta> {
    let creds = crate::auth::load(store.root(), &r.registry);
    let client = Client::connect(r, &store.tmp_dir(), creds.as_ref(), false)?;
    let mut m = client.manifest(&r.tag)?;
    if !m.manifests.is_empty() {
        let arch = host_arch();
        let digest = pick_platform(&m, arch)
            .ok_or_else(|| err(format!("{} has no linux/{arch} build", r.display())))?
            .to_string();
        m = client.manifest(&digest)?;
    }
    let cfg = m.config.as_ref().ok_or_else(|| err("manifest has no config"))?;
    let cfg_path = store.tmp_dir().join("config.json");
    client.blob(&cfg.digest, &cfg_path)?;
    let cfg_bytes = fs::read(&cfg_path)?;
    let _ = fs::remove_file(&cfg_path);

    let mut layers = vec![];
    for d in &m.layers {
        let l = LayerRef { digest: d.digest.clone(), media_type: d.media_type.clone(), size: d.size };
        let short = &l.digest[7..19];
        if store.has_layer(&l) || store.blob_path(&l.digest)?.is_file() {
            say(format!("{short}: Already exists"));
        } else {
            let blob = store.blob_path(&l.digest)?;
            fs::create_dir_all(blob.parent().expect("has parent"))?;
            say(format!("{short}: Pulling"));
            client.blob(&l.digest, &blob)?;
            say(format!("{short}: Pull complete"));
        }
        layers.push(l);
    }

    let meta = meta_from_config(&r.display(), layers, &cfg_bytes)?;
    store.save(r, &meta)?;
    Ok(meta)
}

/// Build the stored image record from an image config JSON (as found in registries and archives).
pub(crate) fn meta_from_config(name: &str, layers: Vec<LayerRef>, config: &[u8]) -> Result<ImageMeta> {
    let rc = serde_json::from_slice::<ConfigFile>(config)?.config.unwrap_or_default();
    Ok(ImageMeta {
        name: name.to_string(),
        layers,
        entrypoint: rc.entrypoint.unwrap_or_default(),
        cmd: rc.cmd.unwrap_or_default(),
        env: rc.env.unwrap_or_default(),
        working_dir: rc.working_dir.unwrap_or_default(),
        user: rc.user.unwrap_or_default(),
        exposed: rc.exposed_ports.unwrap_or_default().into_keys().collect(),
        labels: rc.labels.unwrap_or_default(),
        healthcheck: rc.healthcheck.and_then(health_from),
    })
}
