//! Registry v2 client: auth handshake (bearer token or basic), manifests, blobs, uploads.
use crate::{
    auth::Creds,
    error::{err, Result},
    http::{self, Req},
    reference::Reference,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, io, path::{Path, PathBuf}};

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

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

/// Digest of the linux image for `arch` inside an index.
pub fn pick_platform<'a>(m: &'a Manifest, arch: &str) -> Option<&'a str> {
    m.manifests
        .iter()
        .find(|d| d.platform.as_ref().is_some_and(|p| p.os == "linux" && p.architecture == arch))
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

pub struct Client {
    base: String,
    repo: String,
    /// Full `Authorization` header value.
    auth: Option<String>,
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
        let scope = format!("repository:{}:{}", r.repo, if push { "pull,push" } else { "pull" });
        Self::open(&r.registry, &r.repo, tmp, creds, Some(&scope))
    }

    /// Authenticate only (for `login`).
    pub fn probe(registry: &str, tmp: &Path, creds: &Creds) -> Result<Self> {
        Self::open(registry, "", tmp, Some(creds), None)
    }

    fn open(registry: &str, repo: &str, tmp: &Path, creds: Option<&Creds>, scope: Option<&str>) -> Result<Self> {
        fs::create_dir_all(tmp)?;
        let mut c = Client {
            base: format!("{}://{registry}", scheme(registry)),
            repo: repo.into(),
            auth: None,
            tmp: tmp.into(),
        };
        c.authenticate(creds, scope)?;
        Ok(c)
    }

    fn authenticate(&mut self, creds: Option<&Creds>, scope: Option<&str>) -> Result<()> {
        let probe = self.tmp.join("probe");
        let reply = http::get(&format!("{}/v2/", self.base), &[], &probe)?;
        let _ = fs::remove_file(&probe);
        match reply.status {
            200 => Ok(()),
            401 => {
                let challenge = reply.www_authenticate.unwrap_or_default();
                if let Some((realm, service)) = parse_challenge(&challenge) {
                    let mut url = format!("{realm}?");
                    if let Some(s) = scope {
                        url.push_str(&format!("scope={s}&"));
                    }
                    if let Some(s) = service {
                        url.push_str(&format!("service={s}"));
                    }
                    let headers: Vec<(&str, String)> = creds.map(|c| ("Authorization", c.basic())).into_iter().collect();
                    let body = self.tmp.join("token");
                    let r = http::get(url.trim_end_matches(['?', '&']), &headers, &body)?;
                    let bytes = fs::read(&body).unwrap_or_default();
                    let _ = fs::remove_file(&body);
                    match r.status {
                        200 => {
                            let t: Token = serde_json::from_slice(&bytes)?;
                            let t = t.token.or(t.access_token).ok_or_else(|| err("no token in response"))?;
                            self.auth = Some(format!("Bearer {t}"));
                            Ok(())
                        }
                        401 | 403 if creds.is_some() => Err(err("incorrect username or password")),
                        401 | 403 => Err(err("this registry requires `docker login`")),
                        s => Err(err(format!("token request failed: HTTP {s}"))),
                    }
                } else if challenge.trim_start().to_ascii_lowercase().starts_with("basic") {
                    let c = creds.ok_or_else(|| err("this registry requires `docker login`"))?;
                    self.auth = Some(c.basic());
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
        if let Some(a) = &self.auth {
            h.push(("Authorization", a.clone()));
        }
        h.extend(extra);
        h
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v2/{}/{path}", self.base, self.repo)
    }

    fn fetch(&self, path: &str, accept: Option<&str>, dest: &Path) -> Result<()> {
        let h = self.headers(accept.map(|a| ("Accept", a.to_string())));
        let r = http::get(&self.url(path), &h, dest)?;
        if r.status != 200 {
            let _ = fs::remove_file(dest);
            return Err(err(format!("{path}: HTTP {}", r.status)));
        }
        Ok(())
    }

    pub fn manifest(&self, tag_or_digest: &str) -> Result<Manifest> {
        let dest = self.tmp.join("manifest");
        self.fetch(&format!("manifests/{tag_or_digest}"), Some(MANIFEST_TYPES), &dest)?;
        let m = serde_json::from_slice(&fs::read(&dest)?)?;
        let _ = fs::remove_file(&dest);
        Ok(m)
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
        let dest = self.tmp.join("head");
        let r = http::send(&Req { method: "HEAD", url: &self.url(&format!("blobs/{digest}")), headers: &self.headers(None), upload: None }, &dest)?;
        let _ = fs::remove_file(&dest);
        match r.status {
            200 => Ok(true),
            404 => Ok(false),
            s => Err(err(format!("checking blob {digest}: HTTP {s}"))),
        }
    }

    /// Monolithic upload: open a session, then PUT the whole file with `?digest=`.
    pub fn upload_blob(&self, digest: &str, file: &Path) -> Result<()> {
        let dest = self.tmp.join("upload");
        let mut h = self.headers(Some(("Content-Length", "0".into())));
        let r = http::send(&Req { method: "POST", url: &self.url("blobs/uploads/"), headers: &h, upload: None }, &dest)?;
        let _ = fs::remove_file(&dest);
        if r.status != 202 {
            return Err(err(format!("starting upload: HTTP {}", r.status)));
        }
        let loc = r.location.ok_or_else(|| err("registry sent no upload location"))?;
        let mut url = if loc.starts_with('/') { format!("{}{loc}", self.base) } else { loc };
        url.push_str(if url.contains('?') { "&" } else { "?" });
        url.push_str(&format!("digest={digest}"));
        h = self.headers(Some(("Content-Type", "application/octet-stream".into())));
        let r = http::send(&Req { method: "PUT", url: &url, headers: &h, upload: Some(file) }, &dest)?;
        let body = fs::read_to_string(&dest).unwrap_or_default();
        let _ = fs::remove_file(&dest);
        if r.status != 201 {
            return Err(err(format!("uploading {digest}: HTTP {} {}", r.status, body.trim())));
        }
        Ok(())
    }

    /// Publish a manifest under `tag`. Returns the digest the registry reports (or the computed one).
    pub fn put_manifest(&self, tag: &str, body: &Path, media_type: &str) -> Result<String> {
        let dest = self.tmp.join("manifest-reply");
        let h = self.headers(Some(("Content-Type", media_type.to_string())));
        let r = http::send(&Req { method: "PUT", url: &self.url(&format!("manifests/{tag}")), headers: &h, upload: Some(body) }, &dest)?;
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
              {"digest":"sha256:b","platform":{"architecture":"arm64","os":"linux"}},
              {"digest":"sha256:c","platform":{"architecture":"arm64","os":"windows"}}]}"#,
        )
        .unwrap();
        assert_eq!(pick_platform(&m, "arm64"), Some("sha256:b"));
        assert_eq!(pick_platform(&m, "riscv64"), None);
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
