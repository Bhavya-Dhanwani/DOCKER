//! Registry v2 client: auth handshake (bearer token or basic), manifests, blobs, uploads.
use crate::{
    auth::Creds,
    error::{err, Result},
    http::{self, Req},
    reference::Reference,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    thread,
};

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

/// Attempts for requests that are safe to repeat (1 try + 3 retries, backing off 1s, 2s, 4s).
const TRIES: u32 = 4;

/// Blobs bigger than this are uploaded in PATCH chunks (resumable); smaller ones in one PUT.
/// 8 MiB is above the 5 MiB minimum part size of S3-backed registries. `RUST_DOCKER_PUSH_CHUNK`
/// (bytes) overrides it, which is how the tests exercise chunking with tiny layers.
const DEFAULT_CHUNK: u64 = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
}

#[derive(Debug, Deserialize)]
pub struct Desc {
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default, rename = "mediaType")]
    pub media_type: String,
    pub platform: Option<Platform>,
}

/// Either an image manifest (`config` + `layers`) or an index (`manifests`).
#[derive(Debug, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub manifests: Vec<Desc>,
    pub config: Option<Desc>,
    #[serde(default)]
    pub layers: Vec<Desc>,
}

/// OS/arch names the way registries spell them.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// `linux/arm64`, `linux/arm64/v8`, `amd64` (os defaults to linux) -> (os, arch). The variant is ignored.
pub fn parse_platform(s: &str) -> Result<(String, String)> {
    let mut it = s.split('/');
    let (a, b) = (it.next().unwrap_or(""), it.next());
    let (os, arch) = match b {
        Some(arch) => (a, arch),
        None => ("linux", a),
    };
    let arch = match arch {
        "x86_64" | "x86-64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    if os.is_empty() || arch.is_empty() || it.count() > 1 || !format!("{os}{arch}").chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(err(format!("invalid platform {s:?} (want os/arch, e.g. linux/amd64)")));
    }
    Ok((os.to_string(), arch.to_string()))
}

/// Digest of the `os`/`arch` image inside an index (attestation entries have no platform and never match).
pub fn pick_platform<'a>(m: &'a Manifest, os: &str, arch: &str) -> Option<&'a str> {
    m.manifests
        .iter()
        .find(|d| d.platform.as_ref().is_some_and(|p| p.os == os && p.architecture == arch))
        .map(|d| d.digest.as_str())
}

fn param(challenge: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let rest = &challenge[challenge.find(&pat)? + pat.len()..];
    Some(rest[..rest.find('"')?].to_string())
}

/// `Bearer realm="…",service="…"` -> (realm, service)
pub fn parse_challenge(h: &str) -> Option<(String, Option<String>)> {
    let h = h.trim();
    h.get(..7).filter(|p| p.eq_ignore_ascii_case("bearer "))?;
    Some((param(h, "realm")?, param(h, "service")))
}

/// Loopback registries are spoken to over plain HTTP (as Docker does); everything else needs TLS.
pub fn scheme(registry: &str) -> &'static str {
    let host = match registry.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => registry,
    };
    if host == "localhost" || host.starts_with("127.") || host == "[::1]" { "http" } else { "https" }
}

/// `Range: 0-1023` from an upload-status reply -> next offset to send (1024). No range = nothing received.
fn next_offset(range: Option<&str>) -> u64 {
    range.and_then(|r| r.rsplit('-').next()).and_then(|n| n.trim().parse::<u64>().ok()).map_or(0, |last| last + 1)
}

/// How a blob got to the registry.
#[derive(Debug, PartialEq, Eq)]
pub enum Upload {
    /// Linked from another repository of the same registry; nothing was sent. Holds that repo.
    Mounted(String),
    Pushed,
}

pub struct Client {
    base: String,
    repo: String,
    /// Full `Authorization` header value. Behind a lock because a token that expires in the middle of a long
    /// pull is renewed (the client is shared between upload threads).
    auth: std::sync::Mutex<Option<String>>,
    /// What was used to log in, kept to renew the token.
    creds: Option<Creds>,
    scopes: Vec<String>,
    tmp: PathBuf,
}

#[derive(Deserialize)]
struct Token {
    token: Option<String>,
    access_token: Option<String>,
}

impl Client {
    /// Connect for `repo`. `push` asks the token endpoint for push rights as well.
    pub fn connect(r: &Reference, tmp: &Path, creds: Option<&Creds>, push: bool) -> Result<Self> {
        Self::connect_mounting(r, tmp, creds, push, &[])
    }

    /// Like `connect`, plus pull rights on `mount_from` repos so blobs can be mounted from them.
    pub fn connect_mounting(r: &Reference, tmp: &Path, creds: Option<&Creds>, push: bool, mount_from: &[String]) -> Result<Self> {
        let mut scopes = vec![format!("repository:{}:{}", r.repo, if push { "pull,push" } else { "pull" })];
        scopes.extend(mount_from.iter().map(|m| format!("repository:{m}:pull")));
        Self::open(&r.registry, &r.repo, tmp, creds, &scopes)
    }

    /// Authenticate only (for `login`).
    pub fn probe(registry: &str, tmp: &Path, creds: &Creds) -> Result<Self> {
        Self::open(registry, "", tmp, Some(creds), &[])
    }

    fn open(registry: &str, repo: &str, tmp: &Path, creds: Option<&Creds>, scopes: &[String]) -> Result<Self> {
        fs::create_dir_all(tmp)?;
        let c = Client {
            base: format!("{}://{registry}", scheme(registry)),
            repo: repo.into(),
            auth: std::sync::Mutex::new(None),
            creds: creds.cloned(),
            scopes: scopes.to_vec(),
            tmp: tmp.into(),
        };
        c.authenticate(creds, scopes)?;
        Ok(c)
    }

    /// A fresh scratch path (the client is shared between upload threads).
    fn scratch(&self, what: &str) -> PathBuf {
        http::scratch(&self.tmp, what)
    }

    fn authenticate(&self, creds: Option<&Creds>, scopes: &[String]) -> Result<()> {
        let probe = self.scratch("probe");
        let reply = http::send_retry(&Req { method: "GET", url: &format!("{}/v2/", self.base), headers: &[], upload: None }, &probe, TRIES)?;
        let _ = fs::remove_file(&probe);
        match reply.status {
            200 => Ok(()),
            401 => {
                let challenge = reply.www_authenticate.unwrap_or_default();
                if let Some((realm, service)) = parse_challenge(&challenge) {
                    let mut url = format!("{realm}?");
                    for s in scopes {
                        url.push_str(&format!("scope={s}&"));
                    }
                    if let Some(s) = service {
                        url.push_str(&format!("service={s}"));
                    }
                    let headers: Vec<(&str, String)> = creds.map(|c| ("Authorization", c.basic())).into_iter().collect();
                    let body = self.scratch("token");
                    let r = http::send_retry(&Req { method: "GET", url: url.trim_end_matches(['?', '&']), headers: &headers, upload: None }, &body, TRIES)?;
                    let bytes = fs::read(&body).unwrap_or_default();
                    let _ = fs::remove_file(&body);
                    match r.status {
                        200 => {
                            let t: Token = serde_json::from_slice(&bytes)?;
                            let t = t.token.or(t.access_token).ok_or_else(|| err("no token in response"))?;
                            *self.auth.lock().unwrap() = Some(format!("Bearer {t}"));
                            Ok(())
                        }
                        401 | 403 if creds.is_some() => Err(err("incorrect username or password")),
                        401 | 403 => Err(err("this registry requires `docker login`")),
                        s => Err(err(format!("token request failed: HTTP {s}"))),
                    }
                } else if challenge.trim_start().to_ascii_lowercase().starts_with("basic") {
                    let c = creds.ok_or_else(|| err("this registry requires `docker login`"))?;
                    *self.auth.lock().unwrap() = Some(c.basic());
                    let r = http::get(&format!("{}/v2/", self.base), &self.headers(None), &probe)?;
                    let _ = fs::remove_file(&probe);
                    if r.status == 200 { Ok(()) } else { Err(err("incorrect username or password")) }
                } else {
                    Err(err("registry requires an unsupported kind of login"))
                }
            }
            s => Err(err(format!("registry probe failed: HTTP {s}"))),
        }
    }

    fn headers(&self, extra: Option<(&'static str, String)>) -> Vec<(&'static str, String)> {
        let mut h = vec![];
        if let Some(a) = self.auth.lock().unwrap().as_ref() {
            h.push(("Authorization", a.clone()));
        }
        h.extend(extra);
        h
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v2/{}/{path}", self.base, self.repo)
    }

    /// An upload `Location`: absolute, or a path on the registry's own host.
    fn location(&self, loc: &str) -> String {
        if loc.starts_with('/') { format!("{}{loc}", self.base) } else { loc.to_string() }
    }

    fn fetch(&self, path: &str, accept: Option<&str>, dest: &Path) -> Result<()> {
        let h = self.headers(accept.map(|a| ("Accept", a.to_string())));
        let mut r = http::send_retry(&Req { method: "GET", url: &self.url(path), headers: &h, upload: None }, dest, TRIES)?;
        if r.status == 401 {
            // the bearer token expired during a long pull: get a new one and try once more
            self.authenticate(self.creds.as_ref(), &self.scopes)?;
            let h = self.headers(accept.map(|a| ("Accept", a.to_string())));
            r = http::send_retry(&Req { method: "GET", url: &self.url(path), headers: &h, upload: None }, dest, TRIES)?;
        }
        if r.status != 200 {
            let _ = fs::remove_file(dest);
            return Err(err(format!("{path}: HTTP {}", r.status)));
        }
        Ok(())
    }

    /// Fetch a manifest or index by tag or digest. Also returns the sha256 of the exact bytes served,
    /// which is checked against `tag_or_digest` when that is a digest.
    pub fn manifest(&self, tag_or_digest: &str) -> Result<(Manifest, String)> {
        let dest = self.scratch("manifest");
        self.fetch(&format!("manifests/{tag_or_digest}"), Some(MANIFEST_TYPES), &dest)?;
        let bytes = fs::read(&dest)?;
        let _ = fs::remove_file(&dest);
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        if tag_or_digest.starts_with("sha256:") && digest != tag_or_digest {
            return Err(err(format!("manifest digest mismatch: asked for {tag_or_digest}, got {digest}")));
        }
        Ok((serde_json::from_slice(&bytes)?, digest))
    }

    /// The manifest (or manifest list) exactly as the registry serves it, for `docker manifest inspect`.
    pub fn raw_manifest(&self, tag_or_digest: &str) -> Result<serde_json::Value> {
        let dest = self.scratch("manifest-raw");
        self.fetch(&format!("manifests/{tag_or_digest}"), Some(MANIFEST_TYPES), &dest)?;
        let bytes = fs::read(&dest)?;
        let _ = fs::remove_file(&dest);
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Download a blob to `dest` and verify its sha256.
    pub fn blob(&self, digest: &str, dest: &Path) -> Result<()> {
        self.fetch(&format!("blobs/{digest}"), None, dest)?;
        let mut f = fs::File::open(dest)?;
        let mut h = Sha256::new();
        io::copy(&mut f, &mut h)?;
        let got = format!("sha256:{:x}", h.finalize());
        if got != digest {
            let _ = fs::remove_file(dest);
            return Err(err(format!("digest mismatch for {digest}: got {got}")));
        }
        Ok(())
    }

    pub fn blob_exists(&self, digest: &str) -> Result<bool> {
        let dest = self.scratch("head");
        let r = http::send_retry(&Req { method: "HEAD", url: &self.url(&format!("blobs/{digest}")), headers: &self.headers(None), upload: None }, &dest, TRIES)?;
        let _ = fs::remove_file(&dest);
        match r.status {
            200 => Ok(true),
            404 => Ok(false),
            s => Err(err(format!("checking blob {digest}: HTTP {s}"))),
        }
    }

    /// Open an upload session, optionally asking the registry to mount `digest` from `from`.
    /// Returns the status (201 = mounted, 202 = session opened) and the session URL.
    fn start_upload(&self, mount: Option<(&str, &str)>) -> Result<(u16, Option<String>)> {
        let dest = self.scratch("upload");
        let path = match mount {
            Some((digest, from)) => format!("blobs/uploads/?mount={digest}&from={from}"),
            None => "blobs/uploads/".to_string(),
        };
        let h = self.headers(Some(("Content-Length", "0".into())));
        let r = http::send_retry(&Req { method: "POST", url: &self.url(&path), headers: &h, upload: None }, &dest, TRIES)?;
        let _ = fs::remove_file(&dest);
        match r.status {
            201 | 202 => Ok((r.status, r.location.map(|l| self.location(&l)))),
            s => Err(err(format!("starting upload: HTTP {s}"))),
        }
    }

    /// Put `file` (whose digest is `digest`) into the repo.
    /// First tries to mount it from each of `mount_from` (other repos of this registry known to hold it),
    /// then uploads in chunks if it is big, falling back to one PUT if the registry will not take chunks.
    pub fn upload_blob(&self, digest: &str, file: &Path, mount_from: &[String]) -> Result<Upload> {
        let mut session = None;
        for from in mount_from {
            match self.start_upload(Some((digest, from))) {
                Ok((201, _)) => return Ok(Upload::Mounted(from.clone())),
                Ok((_, loc)) => session = loc, // mount refused, but the reply is a usable session
                Err(_) => {} // a mount that errors is not worth failing the push for
            }
        }
        let size = fs::metadata(file)?.len();
        let chunk = std::env::var("RUST_DOCKER_PUSH_CHUNK").ok().and_then(|v| v.parse().ok()).filter(|c| *c > 0).unwrap_or(DEFAULT_CHUNK);
        if size > chunk {
            let loc = match session.take() {
                Some(l) => l,
                None => self.start_upload(None)?.1.ok_or_else(|| err("registry sent no upload location"))?,
            };
            if self.upload_chunked(loc, digest, file, size, chunk)? {
                return Ok(Upload::Pushed);
            }
            // registry does not do chunks: fall through to a fresh monolithic upload
        }
        let loc = match session {
            Some(l) => l,
            None => self.start_upload(None)?.1.ok_or_else(|| err("registry sent no upload location"))?,
        };
        self.finish_upload(&loc, digest, Some(file))?;
        Ok(Upload::Pushed)
    }

    /// Closing PUT: carries `file` as the whole body (monolithic) or nothing (after chunks).
    fn finish_upload(&self, loc: &str, digest: &str, file: Option<&Path>) -> Result<()> {
        let mut url = loc.to_string();
        url.push_str(if url.contains('?') { "&" } else { "?" });
        url.push_str(&format!("digest={digest}"));
        let dest = self.scratch("upload");
        let h = self.headers(Some(match file {
            Some(_) => ("Content-Type", "application/octet-stream".into()),
            None => ("Content-Length", "0".into()),
        }));
        let r = http::send_retry(&Req { method: "PUT", url: &url, headers: &h, upload: file }, &dest, TRIES)?;
        let body = fs::read_to_string(&dest).unwrap_or_default();
        let _ = fs::remove_file(&dest);
        if r.status != 201 {
            return Err(err(format!("uploading {digest}: HTTP {} {}", r.status, body.trim())));
        }
        Ok(())
    }

    /// Ask the registry how much of an upload it has (`Range` header of a GET on the session).
    fn upload_status(&self, loc: &str) -> Result<(u64, Option<String>)> {
        let dest = self.scratch("status");
        let r = http::send_retry(&Req { method: "GET", url: loc, headers: &self.headers(None), upload: None }, &dest, TRIES)?;
        let _ = fs::remove_file(&dest);
        if r.status != 204 && r.status != 200 {
            return Err(err(format!("upload status: HTTP {}", r.status)));
        }
        Ok((next_offset(r.range.as_deref()), r.location))
    }

    /// PATCH the file up in `chunk`-sized pieces, resuming from whatever the registry says it has after a
    /// failure, then close with a PUT. Ok(false) means "this registry does not support chunked upload".
    fn upload_chunked(&self, mut loc: String, digest: &str, file: &Path, size: u64, chunk: u64) -> Result<bool> {
        let mut src = fs::File::open(file)?;
        let (mut offset, mut failures) = (0u64, 0u32);
        let piece = self.scratch("chunk");
        let result = (|| {
            while offset < size {
                let n = chunk.min(size - offset);
                src.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; n as usize];
                src.read_exact(&mut buf)?;
                fs::File::create(&piece)?.write_all(&buf)?;
                let h = self.headers(Some(("Content-Type", "application/octet-stream".into())));
                let h: Vec<(&str, String)> = h
                    .into_iter()
                    .chain([("Content-Length", n.to_string()), ("Content-Range", format!("{offset}-{}", offset + n - 1))])
                    .collect();
                let dest = self.scratch("patch");
                let sent = http::send(&Req { method: "PATCH", url: &loc, headers: &h, upload: Some(&piece) }, &dest);
                let _ = fs::remove_file(&dest);
                match sent {
                    Ok(r) if r.status == 202 => {
                        offset += n;
                        failures = 0;
                        if let Some(l) = r.location {
                            loc = self.location(&l);
                        }
                    }
                    Ok(r) if offset == 0 && matches!(r.status, 404 | 405 | 501) => return Ok(false),
                    // transient failure or out-of-sync offset: ask where the registry is and carry on from there
                    other => {
                        let retryable = match &other {
                            Ok(r) => http::transient(r.status) || r.status == 416,
                            Err(_) => true,
                        };
                        failures += 1;
                        if !retryable || failures > 3 {
                            return Err(match other {
                                Ok(r) => err(format!("uploading {digest}: HTTP {}", r.status)),
                                Err(e) => e,
                            });
                        }
                        thread::sleep(http::backoff(failures - 1, other.ok().and_then(|r| r.retry_after)));
                        let (at, l) = self.upload_status(&loc)?;
                        offset = at.min(size);
                        if let Some(l) = l {
                            loc = self.location(&l);
                        }
                    }
                }
            }
            self.finish_upload(&loc, digest, None)?;
            Ok(true)
        })();
        let _ = fs::remove_file(&piece);
        result
    }

    /// Publish a manifest under `tag`. Returns the digest the registry reports (or the computed one).
    pub fn put_manifest(&self, tag: &str, body: &Path, media_type: &str) -> Result<String> {
        let dest = self.scratch("manifest-reply");
        let h = self.headers(Some(("Content-Type", media_type.to_string())));
        let r = http::send_retry(&Req { method: "PUT", url: &self.url(&format!("manifests/{tag}")), headers: &h, upload: Some(body) }, &dest, TRIES)?;
        let reply = fs::read_to_string(&dest).unwrap_or_default();
        let _ = fs::remove_file(&dest);
        if r.status != 201 {
            return Err(err(format!("publishing manifest: HTTP {} {}", r.status, reply.trim())));
        }
        Ok(r.content_digest.unwrap_or_else(|| format!("sha256:{:x}", Sha256::digest(fs::read(body).unwrap_or_default()))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_challenge() {
        let h = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#;
        assert_eq!(
            parse_challenge(h),
            Some(("https://auth.docker.io/token".into(), Some("registry.docker.io".into())))
        );
        assert_eq!(parse_challenge("Basic realm=\"x\""), None);
    }

    #[test]
    fn picks_matching_platform() {
        let m: Manifest = serde_json::from_str(
            r#"{"manifests":[
              {"digest":"sha256:a","platform":{"architecture":"amd64","os":"linux"}},
              {"digest":"sha256:att"},
              {"digest":"sha256:b","platform":{"architecture":"arm64","os":"linux"}},
              {"digest":"sha256:c","platform":{"architecture":"arm64","os":"windows"}}]}"#,
        )
        .unwrap();
        assert_eq!(pick_platform(&m, "linux", "arm64"), Some("sha256:b"));
        assert_eq!(pick_platform(&m, "windows", "arm64"), Some("sha256:c"));
        assert_eq!(pick_platform(&m, "linux", "riscv64"), None);
    }

    #[test]
    fn parses_platform_flag() {
        assert_eq!(parse_platform("linux/amd64").unwrap(), ("linux".into(), "amd64".into()));
        assert_eq!(parse_platform("linux/arm64/v8").unwrap(), ("linux".into(), "arm64".into()));
        assert_eq!(parse_platform("arm64").unwrap(), ("linux".into(), "arm64".into()));
        assert_eq!(parse_platform("linux/x86_64").unwrap().1, "amd64");
        for bad in ["", "/", "linux/", "a/b/c/d", "linux/ar m"] {
            assert!(parse_platform(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn upload_range_to_next_offset() {
        assert_eq!(next_offset(Some("0-1023")), 1024);
        assert_eq!(next_offset(Some("0-0")), 1);
        assert_eq!(next_offset(None), 0);
        assert_eq!(next_offset(Some("garbage")), 0);
    }

    #[test]
    fn plain_http_only_for_loopback() {
        assert_eq!(scheme("localhost:5000"), "http");
        assert_eq!(scheme("127.0.0.1:5000"), "http");
        assert_eq!(scheme("localhost"), "http");
        assert_eq!(scheme("registry-1.docker.io"), "https");
        assert_eq!(scheme("ghcr.io"), "https");
        assert_eq!(scheme("my.registry.com:5000"), "https");
    }
}
