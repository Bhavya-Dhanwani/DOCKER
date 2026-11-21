//! `docker push`: pack each layer directory into a tarball, upload missing blobs, publish the manifest.
use crate::{
    auth,
    error::{err, Result},
    pack::pack,
    reference::Reference,
    remote::{host_arch, Client, Upload},
    store::{ImageMeta, Store},
};
use serde_json::{json, Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Mutex},
    thread,
};

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
    if !meta.volumes.is_empty() {
        c.insert("Volumes".into(), meta.volumes.iter().map(|v| (v.clone(), json!({}))).collect::<Map<_, _>>().into());
    }
    for (key, list) in [("OnBuild", &meta.onbuild), ("Shell", &meta.shell)] {
        if !list.is_empty() {
            c.insert(key.into(), json!(list));
        }
    }
    if !meta.stop_signal.is_empty() {
        c.insert("StopSignal".into(), json!(meta.stop_signal));
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
    if r.is_digest() {
        return Err(err("cannot push a digest reference; push a tag"));
    }
    let meta = store.get(r)?.ok_or_else(|| err(format!("no such image: {}", r.display())))?;
    let mounts = mount_sources(store, r, &meta);
    let repos: Vec<String> = mounts.iter().map(|(repo, _)| repo.clone()).collect();
    let creds = auth::load(store.root(), &r.registry);
    let client = Client::connect_mounting(r, &store.tmp_dir(), creds.as_ref(), true, &repos)?;
    let work = store.new_staging_dir("push")?;
    let result = push_layers(store, &client, r, &meta, &mounts, &work, say);
    let _ = fs::remove_dir_all(&work);
    if let Ok(d) = &result {
        crate::info::set_digest(store, r, d);
    }
    result
}

/// Other repos of the same registry whose local images share layers with `meta`: (repo, layer digests).
/// Pushed earlier, those layers are probably in the registry already, so a cross-repo mount can skip the upload.
fn mount_sources(store: &Store, r: &Reference, meta: &ImageMeta) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = vec![];
    for other in store.list() {
        let Ok(o) = Reference::parse(&other.name) else { continue };
        if o.registry != r.registry || o.repo == r.repo {
            continue;
        }
        let shared: Vec<String> = other.layers.iter().filter(|l| meta.layers.iter().any(|m| m.digest == l.digest)).map(|l| l.digest.clone()).collect();
        if shared.is_empty() {
            continue;
        }
        match out.iter_mut().find(|(repo, _)| *repo == o.repo) {
            Some((_, v)) => v.extend(shared),
            None => out.push((o.repo, shared)),
        }
    }
    out.truncate(3);
    out
}

/// Layers uploaded at once. ponytail: fixed; each upload is its own curl process, so this is cheap to raise.
const WORKERS: usize = 3;

struct Job {
    short: String,
    digest: String,
    blob: PathBuf,
    mounts: Vec<String>,
}

/// Skip the upload if the registry has the blob, else mount it or send it. Reports through `say`.
fn upload_one(client: &Client, j: &Job, say: &dyn Fn(String)) -> Result<()> {
    if client.blob_exists(&j.digest)? {
        say(format!("{}: Layer already exists", j.short));
        return Ok(());
    }
    say(format!("{}: Pushing", j.short));
    match client.upload_blob(&j.digest, &j.blob, &j.mounts)? {
        Upload::Mounted(from) => say(format!("{}: Mounted from {from}", j.short)),
        Upload::Pushed => say(format!("{}: Pushed", j.short)),
    }
    Ok(())
}

fn push_layers(store: &Store, client: &Client, r: &Reference, meta: &ImageMeta, mounts: &[(String, Vec<String>)], work: &Path, say: &mut dyn FnMut(String)) -> Result<String> {
    let (mut layers, mut diff_ids) = (vec![], vec![]);
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Job>(1); // bounded: at most a few packed layers sit on disk
    let jobs_rx = Mutex::new(jobs_rx);
    let (msg_tx, msg_rx) = mpsc::channel::<String>();
    let failure: Mutex<Option<crate::error::Error>> = Mutex::new(None);

    // Packing (CPU) in this thread overlaps with WORKERS uploading threads (network).
    let packed: Result<()> = thread::scope(|s| {
        for _ in 0..WORKERS {
            let (rx, tx, failure) = (&jobs_rx, msg_tx.clone(), &failure);
            s.spawn(move || {
                loop {
                    // take the guard only for the receive, or the workers would run one at a time
                    let next = rx.lock().expect("lock").recv();
                    let Ok(job) = next else { break };
                    let say = |m: String| {
                        let _ = tx.send(m);
                    };
                    if let Err(e) = upload_one(client, &job, &say) {
                        failure.lock().expect("lock").get_or_insert(e);
                    }
                    let _ = fs::remove_file(&job.blob);
                }
            });
        }
        drop(msg_tx);
        let mut feed = || -> Result<()> {
            for (i, l) in meta.layers.iter().enumerate() {
                let dir = store.layer_path(&l.digest)?;
                if !dir.is_dir() {
                    return Err(err(format!("layer {} is missing from the store; pull or rebuild the image", &l.digest[7..19])));
                }
                if failure.lock().expect("lock").is_some() {
                    break;
                }
                let blob = work.join(format!("layer-{i}"));
                let p = pack(&dir, &blob)?;
                let mounts = mounts.iter().filter(|(_, ds)| ds.contains(&l.digest)).map(|(repo, _)| repo.clone()).collect();
                let job = Job { short: l.digest[7..19].to_string(), digest: p.digest.clone(), blob, mounts };
                jobs_tx.send(job).map_err(|_| err("upload threads stopped"))?;
                layers.push((p.digest, p.size));
                diff_ids.push(p.diff_id);
                for m in msg_rx.try_iter() {
                    say(m);
                }
            }
            Ok(())
        };
        let res = feed();
        drop(jobs_tx); // lets the workers finish and exit
        for m in msg_rx.iter() {
            say(m); // ends when every worker has dropped its sender
        }
        res
    });
    packed?;
    if let Some(e) = failure.into_inner().expect("lock") {
        return Err(e);
    }

    let blob = work.join("blob");
    let config = serde_json::to_vec(&config_json(meta, &diff_ids))?;
    let config_digest = format!("sha256:{}", hex(&config));
    fs::write(&blob, &config)?;
    if !client.blob_exists(&config_digest)? {
        client.upload_blob(&config_digest, &blob, &[])?;
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
            onbuild: vec!["RUN x".into()],
            volumes: vec!["/data".into()],
            stop_signal: "SIGINT".into(),
            shell: vec!["/bin/bash".into(), "-c".into()],
        };
        let v = config_json(&meta, &["sha256:d1".into()]);
        assert_eq!(v["os"], "linux");
        assert_eq!(v["config"]["Entrypoint"][0], "/ep");
        assert_eq!(v["config"]["WorkingDir"], "/app");
        assert!(v["config"]["ExposedPorts"]["80/tcp"].is_object());
        assert_eq!(v["config"]["Labels"]["k"], "v");
        assert_eq!(v["config"]["Volumes"]["/data"], json!({}));
        assert_eq!(v["config"]["OnBuild"][0], "RUN x");
        assert_eq!(v["config"]["StopSignal"], "SIGINT");
        assert_eq!(v["config"]["Shell"][0], "/bin/bash");
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
            ..Default::default()
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
