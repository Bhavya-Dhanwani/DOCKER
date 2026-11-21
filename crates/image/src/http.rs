//! HTTP through the system `curl` (preinstalled on Windows 10+, macOS and nearly every Linux).
//! ponytail: avoids needing a C toolchain for a TLS library; swap this one function for a Rust
//! HTTP client (ureq + rustls) if you want zero external binaries.
use crate::error::{err, Result};
use std::{fs, path::Path, process::Command};

pub struct Req<'a> {
    /// GET (follows redirects), HEAD, POST, PUT, ...
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, String)],
    /// File to send as the request body (PUT).
    pub upload: Option<&'a Path>,
}

pub struct Reply {
    pub status: u16,
    /// Value of the last `WWW-Authenticate` header seen (for the token handshake).
    pub www_authenticate: Option<String>,
    pub location: Option<String>,
    pub content_digest: Option<String>,
}

pub fn get(url: &str, headers: &[(&str, String)], dest: &Path) -> Result<Reply> {
    send(&Req { method: "GET", url, headers, upload: None }, dest)
}

fn last_header(dump: &str, name: &str) -> Option<String> {
    dump.lines().rev().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    })
}

/// Send a request, writing the response body to `dest`. Headers go through a file so tokens never show in `ps`.
pub fn send(req: &Req, dest: &Path) -> Result<Reply> {
    let hdr_in = dest.with_extension("req");
    let hdr_out = dest.with_extension("resp");
    fs::write(&hdr_in, req.headers.iter().map(|(k, v)| format!("{k}: {v}\n")).collect::<String>())?;

    let mut c = Command::new("curl");
    c.args(["-sS", "--retry", "2"]);
    match req.method {
        "GET" => {
            c.arg("-L");
        }
        "HEAD" => {
            c.arg("-I");
        }
        m => {
            c.args(["-X", m]);
        }
    }
    if let Some(f) = req.upload {
        c.arg("-T").arg(f);
    }
    c.arg("-o").arg(dest).arg("-D").arg(&hdr_out).arg("-H").arg(format!("@{}", hdr_in.display()));
    c.args(["-w", "%{http_code}"]).arg(req.url);
    let out = c.output();
    let _ = fs::remove_file(&hdr_in);
    let out = out.map_err(|e| err(format!("cannot run curl ({e}); install curl")))?;
    let dump = fs::read_to_string(&hdr_out).unwrap_or_default();
    let _ = fs::remove_file(&hdr_out);
    if !out.status.success() {
        return Err(err(format!("curl failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(Reply {
        status: String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0),
        www_authenticate: last_header(&dump, "www-authenticate"),
        location: last_header(&dump, "location"),
        content_digest: last_header(&dump, "docker-content-digest"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_last_matching_header() {
        let dump = "HTTP/1.1 307\r\nLocation: /a\r\n\r\nHTTP/1.1 202\r\nlocation: /b\r\nDocker-Content-Digest: sha256:x\r\n";
        assert_eq!(last_header(dump, "Location").as_deref(), Some("/b"));
        assert_eq!(last_header(dump, "docker-content-digest").as_deref(), Some("sha256:x"));
        assert_eq!(last_header(dump, "etag"), None);
    }
}
