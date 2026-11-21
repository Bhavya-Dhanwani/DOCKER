//! Copy-up fallback: when the kernel refuses an unprivileged overlayfs mount, the image layers are merged
//! by copying into a (RAM-backed) directory. Honours overlay whiteouts (0/0 char devices) and opaque dirs.
use std::{
    ffi::CString,
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::Path,
};

fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("no NUL in path")
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

fn is_opaque(dir: &Path) -> bool {
    let mut buf = [0u8; 4];
    let n = unsafe { libc::lgetxattr(cpath(dir).as_ptr(), c"user.overlay.opaque".as_ptr(), buf.as_mut_ptr().cast(), 4) };
    n == 1 && buf[0] == b'y'
}

/// Merge `layers` (bottom first) into the existing directory `dest`.
pub fn merge(layers: &[std::path::PathBuf], dest: &Path) -> io::Result<()> {
    layers.iter().try_for_each(|l| apply(l, dest))
}

fn apply(src: &Path, dst: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (sp, dp) = (entry.path(), dst.join(entry.file_name()));
        let md = fs::symlink_metadata(&sp)?;
        let ft = md.file_type();
        if ft.is_char_device() {
            if md.rdev() == 0 {
                remove_any(&dp)?; // whiteout: deleted in this layer
            }
            continue; // other devices: the runtime provides its own /dev
        }
        if ft.is_dir() {
            if fs::symlink_metadata(&dp).is_ok_and(|m| !m.is_dir()) {
                remove_any(&dp)?;
            }
            if is_opaque(&sp) && dp.is_dir() {
                for e in fs::read_dir(&dp)? {
                    remove_any(&e?.path())?;
                }
            }
            if !dp.is_dir() {
                fs::create_dir(&dp)?;
            }
            apply(&sp, &dp)?;
            fs::set_permissions(&dp, fs::Permissions::from_mode(md.mode() & 0o7777))?;
        } else {
            remove_any(&dp)?;
            if ft.is_symlink() {
                symlink(fs::read_link(&sp)?, &dp)?;
            } else if ft.is_file() {
                fs::copy(&sp, &dp)?;
                fs::set_permissions(&dp, fs::Permissions::from_mode(md.mode() & 0o7777))?;
            } else {
                continue; // fifos and sockets are not part of images
            }
        }
        unsafe { libc::lchown(cpath(&dp).as_ptr(), md.uid(), md.gid()) }; // best effort, like unpacking
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("copyfs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn upper_layer_overrides_and_keeps_symlinks() {
        let (a, b, out) = (tmp("a"), tmp("b"), tmp("out"));
        fs::write(a.join("f"), "old").unwrap();
        fs::write(a.join("keep"), "k").unwrap();
        fs::create_dir(a.join("d")).unwrap();
        fs::write(a.join("d/x"), "x").unwrap();
        fs::write(b.join("f"), "new").unwrap();
        fs::write(b.join("d"), "now a file").unwrap();
        symlink("keep", b.join("link")).unwrap();
        merge(&[a.clone(), b.clone()], &out).unwrap();
        assert_eq!(fs::read_to_string(out.join("f")).unwrap(), "new");
        assert_eq!(fs::read_to_string(out.join("keep")).unwrap(), "k");
        assert_eq!(fs::read_to_string(out.join("d")).unwrap(), "now a file");
        assert_eq!(fs::read_link(out.join("link")).unwrap(), Path::new("keep"));
        for d in [a, b, out] {
            let _ = fs::remove_dir_all(d);
        }
    }
}
