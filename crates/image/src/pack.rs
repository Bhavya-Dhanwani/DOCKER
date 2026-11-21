//! Turn an unpacked layer directory back into an OCI layer tarball (gzip): the reverse of `layer::unpack`.
//! Overlay whiteouts (char device 0/0) become `.wh.<name>` files, opaque directories get `.wh..wh..opq`.
use crate::error::Result;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::{io, io::Write};
use std::path::Path;

pub struct Packed {
    /// sha256 of the compressed tarball (what the registry stores).
    pub digest: String,
    /// sha256 of the uncompressed tar (goes in the image config's `rootfs.diff_ids`).
    pub diff_id: String,
    pub size: u64,
}

/// Pass-through writer that hashes and counts what goes through it.
#[cfg(target_os = "linux")]
struct Hashing<W> {
    inner: W,
    h: Sha256,
    n: u64,
}

#[cfg(target_os = "linux")]
impl<W> Hashing<W> {
    fn new(inner: W) -> Self {
        Hashing { inner, h: Sha256::new(), n: 0 }
    }

    fn digest(&self) -> String {
        format!("sha256:{:x}", self.h.clone().finalize())
    }
}

#[cfg(target_os = "linux")]
impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.h.update(&buf[..n]);
        self.n += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(not(target_os = "linux"))]
pub fn pack(_dir: &Path, _out: &Path) -> Result<Packed> {
    Err(crate::error::err("packing layers needs a Linux host"))
}

#[cfg(target_os = "linux")]
pub fn pack(dir: &Path, out: &Path) -> Result<Packed> {
    use flate2::{write::GzEncoder, Compression};
    let file = std::fs::File::create(out)?;
    let mut tar = tar::Builder::new(Hashing::new(GzEncoder::new(Hashing::new(file), Compression::default())));
    add_dir(&mut tar, dir, Path::new(""))?;
    let uncompressed = tar.into_inner()?; // writes the end-of-archive blocks
    let diff_id = uncompressed.digest();
    let compressed = uncompressed.inner.finish()?;
    compressed.inner.sync_all()?;
    Ok(Packed { digest: compressed.digest(), diff_id, size: compressed.n })
}

#[cfg(target_os = "linux")]
fn marker<W: Write>(tar: &mut tar::Builder<W>, path: &Path) -> io::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(0);
    h.set_mode(0);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    tar.append_data(&mut h, path, io::empty())
}

#[cfg(target_os = "linux")]
fn is_opaque(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).expect("no NUL in path");
    let mut b = [0u8; 1];
    let n = unsafe { libc::lgetxattr(c.as_ptr(), c"trusted.overlay.opaque".as_ptr(), b.as_mut_ptr().cast(), 1) };
    n == 1 && b[0] == b'y'
}

#[cfg(target_os = "linux")]
fn add_dir<W: Write>(tar: &mut tar::Builder<W>, root: &Path, rel: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut names: Vec<_> = std::fs::read_dir(root.join(rel))?.map(|e| e.map(|e| e.file_name())).collect::<io::Result<_>>()?;
    names.sort(); // stable output
    for name in names {
        let rel_path = rel.join(&name);
        let full = root.join(&rel_path);
        let md = std::fs::symlink_metadata(&full)?;
        let ft = md.file_type();
        if ft.is_char_device() && md.rdev() == 0 {
            marker(tar, &rel.join(format!(".wh.{}", name.to_string_lossy())))?;
        } else if ft.is_dir() {
            tar.append_dir(&rel_path, &full)?;
            if is_opaque(&full) {
                marker(tar, &rel_path.join(".wh..wh..opq"))?;
            }
            add_dir(tar, root, &rel_path)?;
        } else if ft.is_symlink() {
            let mut h = tar::Header::new_gnu();
            h.set_metadata(&md);
            tar.append_link(&mut h, &rel_path, std::fs::read_link(&full)?)?;
        } else if ft.is_file() {
            tar.append_file(&rel_path, &mut std::fs::File::open(&full)?)?;
        } // devices, fifos and sockets are not part of image layers
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::{symlink, FileTypeExt}};

    #[test]
    fn pack_then_unpack_round_trips_files_links_and_whiteouts() {
        if unsafe { libc::geteuid() } != 0 {
            return; // unpacking preserves ownership, which needs root
        }
        let tmp = std::env::temp_dir().join(format!("rd-pack-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let (src, dst, blob) = (tmp.join("src"), tmp.join("dst"), tmp.join("layer.tgz"));
        fs::create_dir_all(src.join("etc/sub")).unwrap();
        fs::write(src.join("etc/file"), "hello").unwrap();
        symlink("file", src.join("etc/link")).unwrap();
        let c = std::ffi::CString::new(src.join("etc/gone").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR, 0) }, 0); // overlay whiteout
        fs::create_dir_all(&dst).unwrap();

        let p = pack(&src, &blob).unwrap();
        assert_eq!(p.size, fs::metadata(&blob).unwrap().len());
        assert!(p.digest.starts_with("sha256:") && p.diff_id.starts_with("sha256:") && p.digest != p.diff_id);
        crate::layer::unpack(&blob, "application/vnd.oci.image.layer.v1.tar+gzip", &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("etc/file")).unwrap(), "hello");
        assert_eq!(fs::read_link(dst.join("etc/link")).unwrap().to_str(), Some("file"));
        assert!(dst.join("etc/sub").is_dir());
        let wh = fs::symlink_metadata(dst.join("etc/gone")).unwrap();
        assert!(wh.file_type().is_char_device(), "whiteout must come back as a whiteout");
        assert!(!dst.join("etc/.wh.gone").exists());
        let _ = fs::remove_dir_all(&tmp);
    }
}
