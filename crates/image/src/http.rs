//! HTTP through the system `curl` (preinstalled on Windows 10+, macOS and nearly every Linux).
//! ponytail: avoids needing a C toolchain for a TLS library; swap this one function for a Rust
//! HTTP client (ureq + rustls) if you want zero external binaries.
use crate::error::{err, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

pub struct Req<'a> {
    /// GET (follows redirects), HEAD, POST, PUT, PATCH, ...
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, String)],
    /// File to send as the request body (PUT/PATCH).
    pub upload: Option<&'a Path>,
}

pub struct Reply {
    pub status: u16,
    /// Value of the last `WWW-Authenticate` header seen (for the token handshake).
    pub www_authenticate: Option<String>,
    pub location: Option<String>,
    pub content_digest: Option<String>,
    /// `Range: 0-<last byte>` of an upload session (resumable uploads).
    pub range: Option<String>,
    /// `Retry-After` in seconds, when the server sent one.
    pub retry_after: Option<u64>,
}

/// A scratch file name that no other thread is using (`send` derives its header files from it).
pub fn scratch(dir: &Path, what: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    dir.join(format!("{what}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

/// Statuses worth another try: the server is overloaded or flaky, not "no".
pub fn transient(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Seconds to wait before retry number `attempt` (0-based): 1, 2, 4 ... capped at 30, or what the server asked.
pub fn backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
    Duration::from_secs(retry_after.unwrap_or(1u64 << attempt.min(5)).min(30))
}

/// `send`, retried with backoff on 429/5xx and on curl failures (connection resets), `tries` attempts in all.
/// Only for requests that are safe to repeat: GET, HEAD, POST that opens a session, PUT of a whole body.
pub fn send_retry(req: &Req, dest: &Path, tries: u32) -> Result<Reply> {
    for attempt in 0.. {
        let last = attempt + 1 >= tries;
        match send(req, dest) {
            Ok(r) if transient(r.status) && !last => thread::sleep(backoff(attempt, r.retry_after)),
            Err(_) if !last => thread::sleep(backoff(attempt, None)),
            other => return other,
        }
    }
    unreachable!()
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
    c.arg("-sS");
    // never hang forever on a dead connection: give up when nothing arrives for a minute (the caller retries)
    c.args(["--connect-timeout", "30", "--speed-time", "60", "--speed-limit", "1024"]);
    match req.method {
        "GET" => {
            c.args(["-L", "--retry", "2"]);
        }
        "HEAD" => {
            c.args(["-I", "--retry", "2"]);
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
        range: last_header(&dump, "range"),
        retry_after: last_header(&dump, "retry-after").and_then(|v| v.parse().ok()),
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

    #[test]
    fn retry_policy() {
        assert!(transient(429) && transient(503) && transient(500));
        assert!(!transient(404) && !transient(401) && !transient(201));
        assert_eq!(backoff(0, None), Duration::from_secs(1));
        assert_eq!(backoff(2, None), Duration::from_secs(4));
        assert_eq!(backoff(20, None), Duration::from_secs(30));
        assert_eq!(backoff(0, Some(7)), Duration::from_secs(7));
        assert_eq!(backoff(0, Some(9999)), Duration::from_secs(30));
    }

    #[test]
    fn scratch_names_are_unique() {
        assert_ne!(scratch(Path::new("/t"), "x"), scratch(Path::new("/t"), "x"));
    }
}
