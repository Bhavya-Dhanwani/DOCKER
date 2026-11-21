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

/// Wrap a layer blob in the right decompressor. The magic bytes decide (zstd `28 B5 2F FD`, gzip `1F 8B`),
/// because `docker save` archives carry no media type; the media type is only the tie-breaker.
#[cfg(any(target_os = "linux", test))]
pub fn decompress<R: std::io::Read + 'static>(r: R, media_type: &str) -> Result<Box<dyn std::io::Read>> {
    use std::io::{Cursor, Read};
    let mut r = r;
    let mut magic = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut magic[got..])? {
            0 => break,
            n => got += n,
        }
    }
    let r = Cursor::new(magic[..got].to_vec()).chain(r);
    let zstd = magic == [0x28, 0xB5, 0x2F, 0xFD];
    let gzip = got >= 2 && magic[..2] == [0x1F, 0x8B];
    Ok(if zstd || (!gzip && media_type.contains("zstd")) {
        Box::new(ruzstd::decoding::StreamingDecoder::new(r).map_err(|e| crate::error::err(format!("bad zstd layer: {e}")))?)
    } else if gzip || media_type.contains("gzip") {
        Box::new(flate2::read::GzDecoder::new(r))
    } else {
        Box::new(r)
    })
}

#[cfg(target_os = "linux")]
pub fn unpack(blob: &Path, media_type: &str, dest: &Path) -> Result<()> {
    unpack_as(blob, media_type, dest, false)
}

/// Unpack for rootless use. Must run inside a user namespace (where whiteout `mknod 0/0` is allowed):
/// uses `user.overlay.*` xattrs (overlayfs `userxattr`) and tolerates chown/device failures
/// (an unmapped uid cannot be owned) instead of aborting.
#[cfg(target_os = "linux")]
pub fn unpack_rootless(blob: &Path, media_type: &str, dest: &Path) -> Result<()> {
    unpack_as(blob, media_type, dest, true)
}

#[cfg(target_os = "linux")]
fn unpack_as(blob: &Path, media_type: &str, dest: &Path, rootless: bool) -> Result<()> {
    use crate::error::err;
    use std::{ffi::CString, fs, os::unix::ffi::OsStrExt};

    let file = fs::File::open(blob).map_err(|e| err(format!("layer blob missing ({e}); run `docker pull` again")))?;
    let reader = decompress(file, media_type)?;

    let cpath = |p: &Path| CString::new(p.as_os_str().as_bytes()).expect("no NUL in path");
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_preserve_ownerships(!rootless);
    ar.set_unpack_xattrs(!rootless);
    let opaque_attr = if rootless { c"user.overlay.opaque" } else { c"trusted.overlay.opaque" };
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parent = dest.join(path.parent().unwrap_or(Path::new("")));
        match classify(&name) {
            Whiteout::No => {
                let kind = entry.header().entry_type();
                if rootless && (kind.is_character_special() || kind.is_block_special()) {
                    continue; // devices need real root; the runtime binds host ones instead
                }
                let (uid, gid) = (entry.header().uid().unwrap_or(0) as u32, entry.header().gid().unwrap_or(0) as u32);
                if entry.unpack_in(dest)? && rootless && (uid != 0 || gid != 0) {
                    // best effort: works under a full uid/gid map, EINVAL under a single-id map (stays root-owned)
                    unsafe { libc::lchown(cpath(&dest.join(&path)).as_ptr(), uid, gid) };
                }
            }
            Whiteout::Opaque => {
                fs::create_dir_all(&parent)?;
                let r = unsafe {
                    libc::lsetxattr(cpath(&parent).as_ptr(), opaque_attr.as_ptr(), b"y".as_ptr().cast(), 1, 0)
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

    /// A valid zstd frame made of one raw (stored) block: magic, header (window descriptor only), block header, data.
    fn zstd_raw(data: &[u8]) -> Vec<u8> {
        let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x50];
        let bh = 1u32 | ((data.len() as u32) << 3); // last block, type raw
        f.extend(&bh.to_le_bytes()[..3]);
        f.extend(data);
        f
    }

    #[test]
    fn decompresses_zstd_gzip_and_plain() {
        use std::io::{Cursor, Read, Write};
        let mut out = vec![];
        decompress(Cursor::new(zstd_raw(b"hello zstd")), "application/vnd.oci.image.layer.v1.tar+zstd").unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello zstd");
        // detected by magic even when the media type says nothing
        out.clear();
        decompress(Cursor::new(zstd_raw(b"x")), "").unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, b"x");
        let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
        gz.write_all(b"hello gzip").unwrap();
        out.clear();
        decompress(Cursor::new(gz.finish().unwrap()), "").unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello gzip");
        out.clear();
        decompress(Cursor::new(b"plain".to_vec()), "application/vnd.oci.image.layer.v1.tar").unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, b"plain");
    }

    #[test]
    fn classifies_whiteouts() {
        assert_eq!(classify(".wh..wh..opq"), Whiteout::Opaque);
        assert_eq!(classify(".wh.gone"), Whiteout::Remove("gone"));
        assert_eq!(classify("regular"), Whiteout::No);
        assert_eq!(classify(".whatever"), Whiteout::No);
    }
}
