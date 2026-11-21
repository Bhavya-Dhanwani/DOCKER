//! `docker cp` for running containers, through `/proc/<pid>/root` (the container's root as the host sees it).
use crate::{
    error::{Error, Result},
    registry::Entry,
    stats::CpDir,
};
use std::{
    fs, io,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

fn io_err(ctx: impl Into<String>) -> impl FnOnce(io::Error) -> Error {
    let ctx = ctx.into();
    move |e| Error::io(ctx, e)
}

/// Resolve `path` inside `root` the way chroot would: symlinks, even absolute ones, never lead outside.
/// (Plain `root.join(path)` would follow `/etc/passwd -> /host/file` out of the container.)
pub fn secure_join(root: &Path, path: &str) -> Result<PathBuf> {
    let mut resolved: Vec<String> = vec![];
    let mut pending: Vec<String> = path.split('/').filter(|c| !c.is_empty()).map(String::from).rev().collect();
    let mut links = 0;
    while let Some(c) = pending.pop() {
        match c.as_str() {
            "." => {}
            ".." => {
                resolved.pop();
            }
            _ => {
                let candidate = root.join(resolved.join("/")).join(&c);
                match fs::symlink_metadata(&candidate) {
                    Ok(md) if md.file_type().is_symlink() => {
                        links += 1;
                        if links > 40 {
                            return Err(Error::Usage(format!("too many symbolic links in {path}")));
                        }
                        let target = fs::read_link(&candidate).map_err(io_err("readlink"))?.to_string_lossy().into_owned();
                        if target.starts_with('/') {
                            resolved.clear();
                        }
                        pending.extend(target.split('/').filter(|x| !x.is_empty()).rev().map(String::from));
                    }
                    _ => resolved.push(c),
                }
            }
        }
    }
    Ok(root.join(resolved.join("/")))
}

fn copy_entry(src: &Path, dst: &Path) -> Result<()> {
    let md = fs::symlink_metadata(src).map_err(io_err(format!("stat {}", src.display())))?;
    if md.file_type().is_symlink() {
        let _ = fs::remove_file(dst);
        symlink(fs::read_link(src).map_err(io_err("readlink"))?, dst).map_err(io_err(format!("symlink {}", dst.display())))
    } else if md.is_dir() {
        fs::create_dir_all(dst).map_err(io_err(format!("mkdir {}", dst.display())))?;
        let _ = fs::set_permissions(dst, md.permissions());
        for e in fs::read_dir(src).map_err(io_err(format!("read {}", src.display())))?.flatten() {
            copy_entry(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(src, dst).map_err(io_err(format!("copy {} -> {}", src.display(), dst.display()))).map(drop)
    }
}

/// `cp -a` semantics: into `dst` if it is an existing directory, otherwise as `dst`.
fn copy_into(src: &Path, dst: &Path) -> Result<()> {
    let target = if dst.is_dir() {
        dst.join(src.file_name().ok_or_else(|| Error::Usage(format!("cannot copy {}", src.display())))?)
    } else {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p).map_err(io_err(format!("mkdir {}", p.display())))?;
        }
        dst.to_path_buf()
    };
    copy_entry(src, &target)
}

pub fn cp(e: &Entry, dir: CpDir, container_path: &str, host_path: &str) -> Result<()> {
    if !e.running() {
        return Err(Error::Usage(format!("container {} is not running (docker cp reads a live container)", e.label())));
    }
    let root = PathBuf::from(format!("/proc/{}/root", e.pid));
    let inside = secure_join(&root, container_path)?;
    match dir {
        CpDir::FromContainer => {
            if fs::symlink_metadata(&inside).is_err() {
                return Err(Error::Usage(format!("no such file or directory in the container: {container_path}")));
            }
            copy_into(&inside, Path::new(host_path))
        }
        CpDir::ToContainer => copy_into(Path::new(host_path), &inside),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rd-cp-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn symlinks_cannot_escape_the_container_root() {
        let host = tmp("secret");
        fs::write(host.join("passwd"), "host secret").unwrap();
        let root = tmp("root");
        fs::create_dir_all(root.join("etc")).unwrap();
        symlink(host.join("passwd"), root.join("etc/evil")).unwrap(); // absolute link out of the container
        symlink("../etc", root.join("rel")).unwrap();
        symlink("/etc", root.join("abs")).unwrap();

        // the absolute target is re-rooted: it resolves under `root`, not on the host
        assert_eq!(secure_join(&root, "/etc/evil").unwrap(), root.join(host.strip_prefix("/").unwrap_or(&host)).join("passwd"));
        assert_eq!(secure_join(&root, "/rel/x").unwrap(), root.join("etc/x"));
        assert_eq!(secure_join(&root, "/abs/x").unwrap(), root.join("etc/x"));
        assert_eq!(secure_join(&root, "/../../etc").unwrap(), root.join("etc"));
        assert_eq!(secure_join(&root, "a/./b/../c").unwrap(), root.join("a/c"));
        symlink("loop", root.join("loop")).unwrap();
        assert!(secure_join(&root, "/loop").is_err());
        for d in [host, root] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn copies_like_cp_a() {
        let (src, dst) = (tmp("src"), tmp("dst"));
        fs::create_dir_all(src.join("d/sub")).unwrap();
        fs::write(src.join("d/sub/f"), "x").unwrap();
        fs::write(src.join("file"), "y").unwrap();
        symlink("file", src.join("link")).unwrap();

        copy_into(&src.join("file"), &dst).unwrap(); // existing dir -> inside
        assert!(dst.join("file").is_file());
        copy_into(&src.join("file"), &dst.join("renamed")).unwrap(); // new name
        assert!(dst.join("renamed").is_file());
        copy_into(&src.join("d"), &dst).unwrap();
        assert!(dst.join("d/sub/f").is_file());
        copy_into(&src.join("link"), &dst.join("l2")).unwrap();
        assert_eq!(fs::read_link(dst.join("l2")).unwrap().to_str(), Some("file"));
        for d in [src, dst] {
            let _ = fs::remove_dir_all(d);
        }
    }
}
