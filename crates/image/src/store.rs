//! On-disk layout:  <root>/images/<key>.json   one file per image
//!                  <root>/layers/<sha256>/     one unpacked dir per layer, shared by every image
//!                  <root>/blobs/<sha256>       compressed layer, deleted once unpacked
use crate::{error::{err, Result}, layer, reference::Reference};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, HashSet}, fs, path::{Path, PathBuf}};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayerRef {
    pub digest: String,
    pub media_type: String,
    pub size: u64,
}

/// A container health check: run `test` (an argv) every `interval_s`; `retries` failures in a row = unhealthy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Health {
    pub test: Vec<String>,
    pub interval_s: u64,
    pub timeout_s: u64,
    pub retries: u32,
    /// Failures during this initial period do not count.
    pub start_period_s: u64,
}

impl Health {
    pub fn new(test: Vec<String>) -> Self {
        Health { test, interval_s: 30, timeout_s: 30, retries: 3, start_period_s: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ImageMeta {
    pub name: String,
    /// Bottom layer first.
    pub layers: Vec<LayerRef>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: String,
    /// `user`, `uid`, `uid:gid` or `user:group`; empty means root.
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub exposed: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub healthcheck: Option<Health>,
    /// `ONBUILD` triggers (instruction text), run when this image is the `FROM` of another build.
    #[serde(default)]
    pub onbuild: Vec<String>,
    /// `VOLUME` paths: each gets an anonymous volume at run time unless `-v` covers it.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// `STOPSIGNAL` (name or number); empty means SIGTERM.
    #[serde(default)]
    pub stop_signal: String,
    /// `SHELL` (empty means `/bin/sh -c`).
    #[serde(default)]
    pub shell: Vec<String>,
}

/// Total size of regular files under `p` (does not follow symlinks).
pub fn dir_size(p: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(p) else { return 0 };
    if md.is_dir() {
        fs::read_dir(p).map_or(0, |rd| rd.flatten().map(|e| dir_size(&e.path())).sum())
    } else {
        md.len()
    }
}

impl ImageMeta {
    pub fn size(&self) -> u64 {
        self.layers.iter().map(|l| l.size).sum()
    }
}

pub struct Store {
    root: PathBuf,
}

/// A digest becomes a path component: accept only `sha256:<64 hex>`.
fn hex(digest: &str) -> Result<&str> {
    match digest.strip_prefix("sha256:") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(h),
        _ => Err(err(format!("bad digest {digest:?}"))),
    }
}

/// Released (the flock with it) when dropped.
pub struct StoreLock(#[allow(dead_code)] Option<fs::File>);

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A pull holds this shared while it downloads (layers it fetched are not recorded until the end), and
    /// `remove`/`prune` hold it exclusively while they delete: so an `rmi` in another process can never pull
    /// a layer out from under a pull in progress. Linux only (elsewhere nothing deletes concurrently).
    pub fn lock(&self, exclusive: bool) -> StoreLock {
        self.lock_file(".lock", exclusive)
    }

    /// Same, on a named lock file inside the store (one per layer while it is unpacked).
    fn lock_file(&self, name: &str, exclusive: bool) -> StoreLock {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let path = self.root.join(name);
            if path.parent().is_some_and(|d| fs::create_dir_all(d).is_ok())
                && let Ok(f) = fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)
            {
                // SAFETY: flock on a descriptor we own; it is released when the file is dropped.
                unsafe { libc::flock(f.as_raw_fd(), if exclusive { libc::LOCK_EX } else { libc::LOCK_SH }) };
                return StoreLock(Some(f));
            }
        }
        let _ = (name, exclusive);
        StoreLock(None)
    }

    /// Give an existing image another name (shares all layers).
    pub fn tag(&self, src: &Reference, dst: &Reference) -> Result<()> {
        let mut meta = self.get(src)?.ok_or_else(|| err(format!("no such image: {}", src.display())))?;
        meta.name = dst.display();
        self.save(dst, &meta)
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Where a layer built from scratch is staged before it gets its final name.
    pub fn new_staging_dir(&self, tag: &str) -> Result<PathBuf> {
        let d = self.tmp_dir().join(format!("{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d)?;
        Ok(d)
    }

    /// Install a finished layer directory under its digest (atomic rename).
    pub fn adopt_layer(&self, digest: &str, dir: &Path) -> Result<PathBuf> {
        let dest = self.layer_path(digest)?;
        fs::create_dir_all(dest.parent().expect("has parent"))?;
        let _ = fs::remove_dir_all(&dest);
        fs::rename(dir, &dest)?;
        Ok(dest)
    }

    /// Delete layers, blobs and staging data that no image refers to. Returns bytes freed.
    pub fn prune(&self) -> Result<u64> {
        let _lock = self.lock(true);
        let used: HashSet<String> = self
            .list()
            .into_iter()
            .flat_map(|m| m.layers.into_iter().filter_map(|l| hex(&l.digest).ok().map(String::from)))
            .collect();
        let mut freed = 0;
        for sub in ["layers", "blobs"] {
            let Ok(rd) = fs::read_dir(self.root.join(sub)) else { continue };
            for e in rd.flatten() {
                if used.contains(e.file_name().to_string_lossy().as_ref()) {
                    continue;
                }
                freed += dir_size(&e.path());
                let _ = if e.path().is_dir() { fs::remove_dir_all(e.path()) } else { fs::remove_file(e.path()) };
            }
        }
        freed += dir_size(&self.tmp_dir());
        let _ = fs::remove_dir_all(self.tmp_dir());
        Ok(freed)
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.root.join("blobs").join(hex(digest)?))
    }

    pub fn layer_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.root.join("layers").join(hex(digest)?))
    }

    fn image_file(&self, r: &Reference) -> PathBuf {
        self.root.join("images").join(format!("{}.json", r.key()))
    }

    pub fn save(&self, r: &Reference, meta: &ImageMeta) -> Result<()> {
        let f = self.image_file(r);
        fs::create_dir_all(f.parent().expect("has parent"))?;
        fs::write(f, serde_json::to_vec_pretty(meta)?)?;
        Ok(())
    }

    pub fn get(&self, r: &Reference) -> Result<Option<ImageMeta>> {
        match fs::read(self.image_file(r)) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// All images, sorted by name.
    pub fn list(&self) -> Vec<ImageMeta> {
        let Ok(rd) = fs::read_dir(self.root.join("images")) else { return vec![] };
        let mut v: Vec<ImageMeta> =
            rd.flatten().filter_map(|f| serde_json::from_slice(&fs::read(f.path()).ok()?).ok()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Delete an image and every layer no other image uses. Returns bytes freed (compressed size).
    pub fn remove(&self, r: &Reference) -> Result<u64> {
        let _lock = self.lock(true);
        let meta = self.get(r)?.ok_or_else(|| err(format!("no such image: {}", r.display())))?;
        fs::remove_file(self.image_file(r))?;
        crate::info::forget(self, r);
        let still_used: HashSet<String> =
            self.list().into_iter().flat_map(|m| m.layers.into_iter().map(|l| l.digest)).collect();
        let mut freed = 0;
        for l in meta.layers.iter().filter(|l| !still_used.contains(&l.digest)) {
            let _ = fs::remove_dir_all(self.layer_path(&l.digest)?);
            let _ = fs::remove_file(self.blob_path(&l.digest)?);
            freed += l.size;
        }
        Ok(freed)
    }

    pub fn has_layer(&self, l: &LayerRef) -> bool {
        self.layer_path(&l.digest).is_ok_and(|p| p.is_dir())
    }

    /// Path of the unpacked layer, unpacking the downloaded blob first if needed (Linux only).
    pub fn ensure_layer(&self, l: &LayerRef) -> Result<PathBuf> {
        let dir = self.layer_path(&l.digest)?;
        if dir.is_dir() {
            return Ok(dir);
        }
        // One process unpacks a given layer at a time: two pulls that share a layer (every Debian-based image
        // does) would otherwise wipe each other's half-unpacked staging directory.
        let _unpacking = self.lock_file(&format!(".locks/{}", hex(&l.digest)?), true);
        if dir.is_dir() {
            return Ok(dir); // the other process finished it while we waited
        }
        let blob = self.blob_path(&l.digest)?;
        let staging = Path::new(&self.root).join("tmp").join(format!("unpack-{}", hex(&l.digest)?));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;
        layer::unpack(&blob, &l.media_type, &staging)?;
        fs::create_dir_all(dir.parent().expect("has parent"))?;
        fs::rename(&staging, &dir)?; // atomic: a half-unpacked layer is never visible
        let _ = fs::remove_file(blob); // keep only the unpacked copy
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: char) -> String {
        format!("sha256:{}", n.to_string().repeat(64))
    }

    fn meta(name: &str, layers: &[char]) -> ImageMeta {
        ImageMeta {
            name: name.into(),
            layers: layers.iter().map(|&c| LayerRef { digest: d(c), media_type: String::new(), size: 10 }).collect(),
            entrypoint: vec![],
            cmd: vec!["sh".into()],
            env: vec![],
            working_dir: String::new(),
            user: String::new(),
            exposed: vec![],
            labels: BTreeMap::new(),
            healthcheck: None,
            ..Default::default()
        }
    }

    #[test]
    fn remove_keeps_shared_layers() {
        let root = std::env::temp_dir().join(format!("rd-store-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let s = Store::open(&root);
        let (a, b) = (Reference::parse("a").unwrap(), Reference::parse("b").unwrap());
        s.save(&a, &meta("a:latest", &['1', '2'])).unwrap();
        s.save(&b, &meta("b:latest", &['2', '3'])).unwrap();
        for c in ['1', '2', '3'] {
            fs::create_dir_all(s.layer_path(&d(c)).unwrap()).unwrap();
        }
        assert_eq!(s.list().len(), 2);
        assert_eq!(s.remove(&a).unwrap(), 10); // only layer 1 was unique to `a`
        assert!(!s.layer_path(&d('1')).unwrap().exists());
        assert!(s.layer_path(&d('2')).unwrap().exists());
        assert!(s.get(&a).unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_removes_only_unreferenced_layers() {
        let root = std::env::temp_dir().join(format!("rd-prune-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let s = Store::open(&root);
        s.save(&Reference::parse("a").unwrap(), &meta("a:latest", &['1'])).unwrap();
        for c in ['1', '2'] {
            let l = s.layer_path(&d(c)).unwrap();
            fs::create_dir_all(&l).unwrap();
            fs::write(l.join("f"), "12345").unwrap();
        }
        fs::create_dir_all(s.tmp_dir()).unwrap();
        fs::write(s.tmp_dir().join("junk"), "xx").unwrap();
        assert_eq!(s.prune().unwrap(), 5 + 2);
        assert!(s.layer_path(&d('1')).unwrap().exists());
        assert!(!s.layer_path(&d('2')).unwrap().exists());
        assert!(!s.tmp_dir().exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tag_shares_layers_under_a_new_name() {
        let root = std::env::temp_dir().join(format!("rd-tag-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let s = Store::open(&root);
        let (a, b) = (Reference::parse("a").unwrap(), Reference::parse("localhost:5000/team/a:v1").unwrap());
        s.save(&a, &meta("a:latest", &['1'])).unwrap();
        s.tag(&a, &b).unwrap();
        assert_eq!(s.get(&b).unwrap().unwrap().name, "localhost:5000/team/a:v1");
        assert_eq!(s.get(&b).unwrap().unwrap().layers, s.get(&a).unwrap().unwrap().layers);
        assert!(s.tag(&Reference::parse("nope").unwrap(), &b).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn adopt_layer_moves_a_staged_dir() {
        let root = std::env::temp_dir().join(format!("rd-adopt-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let s = Store::open(&root);
        let staged = s.new_staging_dir("t").unwrap();
        fs::write(staged.join("x"), "y").unwrap();
        let dest = s.adopt_layer(&d('3'), &staged).unwrap();
        assert!(dest.join("x").is_file() && !staged.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_path_traversal_digests() {
        let s = Store::open("x");
        assert!(s.layer_path("sha256:../../etc").is_err());
        assert!(s.layer_path("md5:abc").is_err());
    }
}
