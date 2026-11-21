//! What `ADD` does beyond `COPY`: download a URL, and unpack a local tar archive (plain, gzip, bzip2, xz).
use crate::{
    error::{err, Result},
    http,
};
use std::{
    fs,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

/// Is this an `ADD` source that must be downloaded?
pub fn is_url(src: &str) -> bool {
    src.starts_with("http://") || src.starts_with("https://")
}

/// Last path segment of a URL (query and fragment dropped): the file name `ADD <url> /dir/` creates.
pub fn url_filename(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let after_scheme = path.split_once("://").map_or(path, |(_, r)| r);
    match after_scheme.split_once('/') {
        Some((_, p)) => p.rsplit('/').next().unwrap_or("").to_string(),
        None => String::new(), // no path at all
    }
}

/// Download `url` into the file `dest` (redirects are followed; anything but 200 is an error).
pub fn fetch_url(url: &str, dest: &Path) -> Result<()> {
    let r = http::get(url, &[], dest)?;
    if r.status != 200 {
        let _ = fs::remove_file(dest);
        return Err(err(format!("ADD {url}: server answered HTTP {}", r.status)));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Tar,
    Gzip,
    Bzip2,
    Xz,
    Other,
}

fn sniff(head: &[u8]) -> Kind {
    if head.starts_with(&[0x1f, 0x8b]) {
        Kind::Gzip
    } else if head.starts_with(b"BZh") {
        Kind::Bzip2
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Kind::Xz
    } else if head.len() >= 262 && &head[257..262] == b"ustar" {
        Kind::Tar
    } else {
        Kind::Other
    }
}

fn head(f: &mut fs::File) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 512];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    f.seek(SeekFrom::Start(0))?;
    Ok(buf)
}

/// If `archive` is a tar (optionally gzip/bzip2/xz compressed, detected by content, not by name), unpack it
/// into `dest` and return true; otherwise return false and leave `dest` alone. `scratch` is a file path
/// that may be used for the decompressed copy.
pub fn extract_archive(archive: &Path, dest: &Path, scratch: &Path) -> Result<bool> {
    let mut f = fs::File::open(archive)?;
    let kind = sniff(&head(&mut f)?);
    let result = match kind {
        Kind::Other => return Ok(false),
        Kind::Tar => unpack(f, dest).map(|()| true),
        k => {
            // decompress to a scratch file first: only a compressed *tar* is extracted, other files stay as they are
            let mut out = fs::File::create(scratch)?;
            match k {
                Kind::Gzip => drop(std::io::copy(&mut flate2::read::GzDecoder::new(f), &mut out)?),
                Kind::Bzip2 => drop(std::io::copy(&mut bzip2_rs::DecoderReader::new(f), &mut out)?),
                _ => lzma_rs::xz_decompress(&mut BufReader::new(f), &mut out).map_err(|e| err(format!("xz: {e}")))?,
            }
            let mut inner = fs::File::open(scratch)?;
            if sniff(&head(&mut inner)?) == Kind::Tar {
                unpack(inner, dest).map(|()| true)
            } else {
                Ok(false)
            }
        }
    };
    let _ = fs::remove_file(scratch);
    result
}

fn unpack(f: fs::File, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let mut ar = tar::Archive::new(f);
    ar.set_preserve_permissions(true);
    #[cfg(unix)]
    ar.set_preserve_ownerships(true);
    ar.unpack(dest)?; // refuses entries that would escape `dest`
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_names() {
        assert!(is_url("https://x/y") && is_url("http://x") && !is_url("ftp://x") && !is_url("a/b"));
        assert_eq!(url_filename("https://h.example/dir/app.tar.gz?x=1#f"), "app.tar.gz");
        assert_eq!(url_filename("http://127.0.0.1:8000/a.txt"), "a.txt");
        assert_eq!(url_filename("http://host"), "");
        assert_eq!(url_filename("http://host/dir/"), "");
    }

    fn tmp(n: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rd-add-{n}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn tar_bytes() -> Vec<u8> {
        let mut b = tar::Builder::new(vec![]);
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o640);
        b.append_data(&mut h, "d/hello.txt", &b"hello"[..]).unwrap();
        b.into_inner().unwrap()
    }

    #[test]
    fn extracts_plain_and_gzip_tars_and_leaves_other_files() {
        use std::io::Write;
        let d = tmp("x");
        let plain = d.join("a.tar");
        fs::write(&plain, tar_bytes()).unwrap();
        let gz = d.join("b.bin"); // the name does not matter, the content does
        let mut e = flate2::write::GzEncoder::new(fs::File::create(&gz).unwrap(), flate2::Compression::fast());
        e.write_all(&tar_bytes()).unwrap();
        e.finish().unwrap();
        let notar = d.join("c.gz");
        let mut e = flate2::write::GzEncoder::new(fs::File::create(&notar).unwrap(), flate2::Compression::fast());
        e.write_all(b"just text").unwrap();
        e.finish().unwrap();
        let text = d.join("t.txt");
        fs::write(&text, "plain").unwrap();

        for (src, n) in [(&plain, "o1"), (&gz, "o2")] {
            let out = d.join(n);
            assert!(extract_archive(src, &out, &d.join("scratch")).unwrap());
            assert_eq!(fs::read_to_string(out.join("d/hello.txt")).unwrap(), "hello");
        }
        assert!(!extract_archive(&notar, &d.join("o3"), &d.join("scratch")).unwrap(), "a gzip that is not a tar is copied as is");
        assert!(!extract_archive(&text, &d.join("o4"), &d.join("scratch")).unwrap());
        assert!(!d.join("o3").exists() && !d.join("o4").exists());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn sniffs_formats() {
        assert_eq!(sniff(&[0x1f, 0x8b, 8]), Kind::Gzip);
        assert_eq!(sniff(b"BZh91AY"), Kind::Bzip2);
        assert_eq!(sniff(&[0xfd, b'7', b'z', b'X', b'Z', 0, 0]), Kind::Xz);
        assert_eq!(sniff(b"hello"), Kind::Other);
    }
}
