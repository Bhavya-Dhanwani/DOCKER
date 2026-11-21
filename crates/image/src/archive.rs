//! `docker save` / `docker load`: OCI image-layout tarballs (`index.json` + `blobs/sha256/*`).
//! `load` also understands the older archive format Docker's own `docker save` writes (`manifest.json`).
use crate::{
    error::{err, Result},
    pack::pack,
    pull::meta_from_config,
    push::{config_json, manifest_json},
    reference::Reference,
    remote::Manifest,
    store::{LayerRef, Store},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, io, path::Path};

const REF_NAME: &str = "org.opencontainers.image.ref.name";
const CONTAINERD_NAME: &str = "io.containerd.image.name";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

fn hex_of(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn append_bytes<W: io::Write>(tar: &mut tar::Builder<W>, path: &str, data: &[u8]) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    tar.append_data(&mut h, path, data)?;
    Ok(())
}

/// Write the images (and their layers) into one tar file.
pub fn save(store: &Store, refs: &[Reference], out: &Path, say: &mut dyn FnMut(String)) -> Result<()> {
    let work = store.new_staging_dir("save")?;
    let result = save_into(store, refs, out, &work, say);
    let _ = fs::remove_dir_all(&work);
    if result.is_err() {
        let _ = fs::remove_file(out); // never leave a half-written archive behind
    }
    result
}

fn save_into(store: &Store, refs: &[Reference], out: &Path, work: &Path, say: &mut dyn FnMut(String)) -> Result<()> {
    let mut tar = tar::Builder::new(fs::File::create(out)?);
    let mut written: BTreeSet<String> = BTreeSet::new();
    let mut index = vec![];
    for r in refs {
        let meta = store.get(r)?.ok_or_else(|| err(format!("no such image: {}", r.display())))?;
        let (mut layers, mut diff_ids) = (vec![], vec![]);
        for l in &meta.layers {
            let dir = store.layer_path(&l.digest)?;
            if !dir.is_dir() {
                return Err(err(format!("layer {} of {} is missing from the store", &l.digest[7..19], r.display())));
            }
            let blob = work.join("layer.tgz");
            let p = pack(&dir, &blob)?;
            if written.insert(p.digest.clone()) {
                tar.append_path_with_name(&blob, format!("blobs/sha256/{}", &p.digest[7..]))?;
            }
            let _ = fs::remove_file(&blob);
            layers.push((p.digest, p.size));
            diff_ids.push(p.diff_id);
        }
        let config = serde_json::to_vec(&config_json(&meta, &diff_ids))?;
        let config_digest = format!("sha256:{}", hex_of(&config));
        if written.insert(config_digest.clone()) {
            append_bytes(&mut tar, &format!("blobs/sha256/{}", &config_digest[7..]), &config)?;
        }
        let manifest = serde_json::to_vec(&manifest_json((&config_digest, config.len() as u64), &layers))?;
        let manifest_digest = format!("sha256:{}", hex_of(&manifest));
        if written.insert(manifest_digest.clone()) {
            append_bytes(&mut tar, &format!("blobs/sha256/{}", &manifest_digest[7..]), &manifest)?;
        }
        index.push(json!({
            "mediaType": OCI_MANIFEST,
            "digest": manifest_digest,
            "size": manifest.len(),
            "annotations": { REF_NAME: r.display() },
        }));
        say(format!("Saved {}", r.display()));
    }
    append_bytes(&mut tar, "index.json", &serde_json::to_vec(&json!({ "schemaVersion": 2, "manifests": index }))?)?;
    append_bytes(&mut tar, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    tar.finish()?;
    Ok(())
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    digest: String,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

/// Docker's pre-OCI `docker save` format.
#[derive(Deserialize)]
struct DockerEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags", default)]
    repo_tags: Vec<String>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Read an archive into the store. Returns the image names it contained.
pub fn load(store: &Store, archive: &Path, say: &mut dyn FnMut(String)) -> Result<Vec<String>> {
    let stage = store.new_staging_dir("load")?;
    let result = (|| {
        tar::Archive::new(fs::File::open(archive)?).unpack(&stage)?; // rejects paths that escape the directory
        if stage.join("index.json").is_file() {
            load_oci(store, &stage, say)
        } else if stage.join("manifest.json").is_file() {
            load_docker(store, &stage, say)
        } else {
            Err(err("not an image archive (neither index.json nor manifest.json)"))
        }
    })();
    let _ = fs::remove_dir_all(&stage);
    result
}

fn digest_file(p: &Path) -> Result<String> {
    let mut h = Sha256::new();
    io::copy(&mut fs::File::open(p)?, &mut h)?;
    Ok(format!("sha256:{:x}", h.finalize()))
}

/// Move a layer file into the store's blob area (verifying it) and unpack it.
/// `size` is the layer file's size (the file itself may already be gone if another image in the same
/// archive shared the layer and moved it into the store first).
fn adopt_layer(store: &Store, file: &Path, digest: &str, media_type: &str, size: u64, say: &mut dyn FnMut(String)) -> Result<LayerRef> {
    let l = LayerRef { digest: digest.into(), media_type: media_type.into(), size };
    if store.has_layer(&l) {
        say(format!("{}: Already exists", &digest[7..19]));
        return Ok(l);
    }
    if digest_file(file)? != digest {
        return Err(err(format!("layer {} is corrupt (digest mismatch)", &digest[7..19])));
    }
    let dest = store.blob_path(digest)?;
    fs::create_dir_all(dest.parent().expect("has parent"))?;
    fs::rename(file, &dest).or_else(|_| fs::copy(file, &dest).map(drop))?;
    store.ensure_layer(&l)?;
    say(format!("{}: Loaded", &digest[7..19]));
    Ok(l)
}

fn load_oci(store: &Store, stage: &Path, say: &mut dyn FnMut(String)) -> Result<Vec<String>> {
    let index: Index = serde_json::from_slice(&fs::read(stage.join("index.json"))?)?;
    let blob = |digest: &str| -> Result<std::path::PathBuf> {
        let hex = digest.strip_prefix("sha256:").ok_or_else(|| err(format!("unsupported digest {digest}")))?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err(format!("bad digest {digest}")));
        }
        Ok(stage.join("blobs/sha256").join(hex))
    };
    let mut names = vec![];
    for e in &index.manifests {
        let name = e
            .annotations
            .get(REF_NAME)
            .or_else(|| e.annotations.get(CONTAINERD_NAME))
            .ok_or_else(|| err("an image in the archive has no name (org.opencontainers.image.ref.name)"))?;
        let r = Reference::parse(name)?;
        let m: Manifest = serde_json::from_slice(&fs::read(blob(&e.digest)?)?)?;
        if !m.manifests.is_empty() {
            return Err(err("multi-platform indexes inside an archive are not supported"));
        }
        let cfg = m.config.as_ref().ok_or_else(|| err("manifest has no config"))?;
        let config_bytes = fs::read(blob(&cfg.digest)?)?;
        let mut layers = vec![];
        for d in &m.layers {
            layers.push(adopt_layer(store, &blob(&d.digest)?, &d.digest, &d.media_type, d.size, say)?);
        }
        let meta = meta_from_config(&r.display(), layers, &config_bytes)?;
        store.save(&r, &meta)?;
        names.push(r.display());
    }
    Ok(names)
}

fn load_docker(store: &Store, stage: &Path, say: &mut dyn FnMut(String)) -> Result<Vec<String>> {
    let entries: Vec<DockerEntry> = serde_json::from_slice(&fs::read(stage.join("manifest.json"))?)?;
    let inside = |rel: &str| -> Result<std::path::PathBuf> {
        if rel.starts_with('/') || rel.split('/').any(|c| c == "..") {
            return Err(err(format!("unsafe path {rel:?} in the archive")));
        }
        Ok(stage.join(rel))
    };
    let mut names = vec![];
    // path -> (digest, size), computed once: images in one archive may share a layer file
    let mut seen: std::collections::HashMap<String, (String, u64)> = std::collections::HashMap::new();
    for e in &entries {
        let config_bytes = fs::read(inside(&e.config)?)?;
        let mut layers = vec![];
        for rel in &e.layers {
            let file = inside(rel)?;
            if !seen.contains_key(rel) {
                seen.insert(rel.clone(), (digest_file(&file)?, fs::metadata(&file)?.len()));
            }
            let (digest, size) = seen[rel].clone();
            layers.push(adopt_layer(store, &file, &digest, "application/vnd.docker.image.rootfs.diff.tar", size, say)?);
        }
        if e.repo_tags.is_empty() {
            return Err(err("an image in the archive has no RepoTags (untagged images cannot be loaded)"));
        }
        for tag in &e.repo_tags {
            let r = Reference::parse(tag)?;
            let meta = meta_from_config(&r.display(), layers.clone(), &config_bytes)?;
            store.save(&r, &meta)?;
            names.push(r.display());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_docker_manifest_formats_parse() {
        let i: Index = serde_json::from_str(r#"{"schemaVersion":2,"manifests":[{"digest":"sha256:a","size":1,"annotations":{"org.opencontainers.image.ref.name":"app:1"}}]}"#).unwrap();
        assert_eq!(i.manifests[0].annotations[REF_NAME], "app:1");
        let d: Vec<DockerEntry> = serde_json::from_str(r#"[{"Config":"c.json","RepoTags":["a:1","a:latest"],"Layers":["x/layer.tar"]}]"#).unwrap();
        assert_eq!((d[0].repo_tags.len(), d[0].layers[0].as_str()), (2, "x/layer.tar"));
    }

    #[test]
    fn unsafe_paths_and_bad_digests_are_rejected() {
        let root = std::env::temp_dir().join(format!("rd-arch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = Store::open(&root);
        let stage = store.new_staging_dir("t").unwrap();
        fs::write(stage.join("manifest.json"), r#"[{"Config":"../../etc/passwd","RepoTags":["a:1"],"Layers":[]}]"#).unwrap();
        assert!(load_docker(&store, &stage, &mut |_| {}).is_err());
        fs::write(stage.join("index.json"), r#"{"manifests":[{"digest":"sha256:../../x","annotations":{"org.opencontainers.image.ref.name":"a"}}]}"#).unwrap();
        assert!(load_oci(&store, &stage, &mut |_| {}).is_err());
        assert!(load(&store, &stage.join("nope.tar"), &mut |_| {}).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
