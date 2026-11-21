//! Unpack one layer tarball into a directory that overlayfs can use as a `lowerdir`.
use crate::error::Result;
use std::path::Path;

#[cfg(not(target_os = "linux"))]
pub fn unpack(_blob: &Path, _media_type: &str, _dest: &Path) -> Result<()> {
    Err(crate::error::err("unpacking layers needs a Linux host (VM backend is on the roadmap)"))
}

/// What a tar entry name means to overlayfs.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
enum Whiteout<'a> {
    /// `.wh..wh..opq`: hide everything lower in this directory.
    Opaque,
    /// `.wh.<name>`: delete `<name>` from lower layers.
    Remove(&'a str),
    No,
}

#[cfg(any(target_os = "linux", test))]
fn classify(file_name: &str) -> Whiteout<'_> {
    if file_name == ".wh..wh..opq" {
        Whiteout::Opaque
    } else if let Some(n) = file_name.strip_prefix(".wh.") {
        Whiteout::Remove(n)
    } else {
        Whiteout::No
    }
}

#[cfg(target_os = "linux")]
pub fn unpack(blob: &Path, media_type: &str, dest: &Path) -> Result<()> {
    use crate::error::err;
    use std::{ffi::CString, fs, io::Read, os::unix::ffi::OsStrExt};

    let file = fs::File::open(blob).map_err(|e| err(format!("layer blob missing ({e}); run `docker pull` again")))?;
    let reader: Box<dyn Read> = if media_type.contains("zstd") {
        return Err(err("zstd layers are not supported yet"));
    } else if media_type.contains("gzip") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };

    let cpath = |p: &Path| CString::new(p.as_os_str().as_bytes()).expect("no NUL in path");
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_preserve_ownerships(true);
    ar.set_unpack_xattrs(true);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parent = dest.join(path.parent().unwrap_or(Path::new("")));
        match classify(&name) {
            Whiteout::No => {
                entry.unpack_in(dest)?;
            }
            Whiteout::Opaque => {
                fs::create_dir_all(&parent)?;
                let r = unsafe {
                    libc::lsetxattr(cpath(&parent).as_ptr(), c"trusted.overlay.opaque".as_ptr(), b"y".as_ptr().cast(), 1, 0)
                };
                if r < 0 {
                    return Err(err(format!("set opaque dir: {}", std::io::Error::last_os_error())));
                }
            }
            Whiteout::Remove(target) => {
                fs::create_dir_all(&parent)?;
                // overlayfs represents "deleted" as a 0/0 character device
                let r = unsafe { libc::mknod(cpath(&parent.join(target)).as_ptr(), libc::S_IFCHR, 0) };
                if r < 0 {
                    return Err(err(format!("create whiteout {target}: {}", std::io::Error::last_os_error())));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_whiteouts() {
        assert_eq!(classify(".wh..wh..opq"), Whiteout::Opaque);
        assert_eq!(classify(".wh.gone"), Whiteout::Remove("gone"));
        assert_eq!(classify("regular"), Whiteout::No);
        assert_eq!(classify(".whatever"), Whiteout::No);
    }
}
