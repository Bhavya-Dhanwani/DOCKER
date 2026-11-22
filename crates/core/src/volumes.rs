//! Named volumes: plain directories under `<data>/volumes/<name>/_data`, outliving any container.
use crate::{
    cli::is_volume_name,
    error::{Error, Result},
    registry,
};
use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    registry::data_dir().join("volumes")
}

pub fn path(name: &str) -> PathBuf {
    root().join(name).join("_data")
}

fn check(name: &str) -> Result<()> {
    if is_volume_name(name) { Ok(()) } else { Err(Error::Usage(format!("bad volume name {name:?}"))) }
}

/// The volume's directory, created on first use (as `docker run -v name:/path` does).
pub fn ensure(name: &str) -> Result<PathBuf> {
    check(name)?;
    let p = path(name);
    fs::create_dir_all(&p).map_err(|e| Error::io(format!("create volume {name}"), e))?;
    Ok(p)
}

/// A volume made for `docker run -v /path` (no name given): `anon-` plus 16 hex digits.
pub fn is_anonymous(name: &str) -> bool {
    name.strip_prefix("anon-").is_some_and(|h| h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

pub fn exists(name: &str) -> bool {
    path(name).is_dir()
}

pub struct Info {
    pub name: String,
    pub size: u64,
}

pub fn list() -> Vec<Info> {
    let Ok(rd) = fs::read_dir(root()) else { return vec![] };
    let mut v: Vec<Info> = rd
        .flatten()
        .filter(|e| e.path().join("_data").is_dir())
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            Info { size: docker_image::dir_size(&e.path().join("_data")), name }
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Labels of the running containers that mount this volume.
pub fn used_by(name: &str) -> Vec<String> {
    registry::list()
        .into_iter()
        .filter(|e| e.running())
        .filter(|e| e.config.as_ref().is_some_and(|c| c.volumes.iter().any(|v| v.is_named() && v.host == name)))
        .map(|e| e.label().to_string())
        .collect()
}

pub fn create(name: &str) -> Result<()> {
    ensure(name).map(drop)
}

pub fn remove(name: &str) -> Result<()> {
    check(name)?;
    if !exists(name) {
        return Err(Error::Usage(format!("no such volume: {name}")));
    }
    let users = used_by(name);
    if !users.is_empty() {
        return Err(Error::Usage(format!("volume {name} is in use by {}", users.join(", "))));
    }
    fs::remove_dir_all(root().join(name)).map_err(|e| Error::io(format!("remove volume {name}"), e))
}

/// Remove every volume no running container uses. Returns (names, bytes freed).
pub fn prune() -> (Vec<String>, u64) {
    let (mut names, mut freed) = (vec![], 0);
    for v in list() {
        if used_by(&v.name).is_empty() && fs::remove_dir_all(root().join(&v.name)).is_ok() {
            freed += v.size;
            names.push(v.name);
        }
    }
    (names, freed)
}
