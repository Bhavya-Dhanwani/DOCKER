//! Registry credentials: stored in `<store>/auth.json` (mode 0600). Not encrypted: treat like a token file.
use crate::{error::{err, Result}, reference::normalize_registry, remote::Client};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Creds {
    pub user: String,
    pub password: String,
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |a, (i, b)| a | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl Creds {
    pub(crate) fn basic(&self) -> String {
        format!("Basic {}", base64(format!("{}:{}", self.user, self.password).as_bytes()))
    }
}

fn file(root: &Path) -> std::path::PathBuf {
    root.join("auth.json")
}

fn read_all(root: &Path) -> BTreeMap<String, Creds> {
    fs::read(file(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_all(root: &Path, all: &BTreeMap<String, Creds>) -> Result<()> {
    fs::create_dir_all(root)?;
    let f = file(root);
    fs::write(&f, serde_json::to_vec_pretty(all)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let (mut out, mut acc, mut bits) = (vec![], 0u32, 0);
    for c in s.trim().trim_end_matches('=').bytes() {
        let v = ALPHABET.iter().position(|a| *a == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ---- Docker's own config (~/.docker/config.json) and credential helpers ----

/// `$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`, parsed.
fn docker_config() -> Option<serde_json::Value> {
    let dir = match std::env::var_os("DOCKER_CONFIG") {
        Some(d) => std::path::PathBuf::from(d),
        None => std::path::PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?).join(".docker"),
    };
    serde_json::from_slice(&fs::read(dir.join("config.json")).ok()?).ok()
}

/// Spellings of a registry used as keys in config.json (Docker Hub has several).
fn config_keys(registry: &str) -> Vec<String> {
    if registry == crate::reference::HUB {
        ["https://index.docker.io/v1/", "index.docker.io", "docker.io", registry].map(String::from).to_vec()
    } else {
        vec![registry.to_string(), format!("https://{registry}")]
    }
}

/// What a credential helper calls the server.
fn server_url(registry: &str) -> String {
    if registry == crate::reference::HUB { "https://index.docker.io/v1/".into() } else { registry.to_string() }
}

/// `credHelpers[registry]`, else the global `credsStore`: the suffix of a `docker-credential-<name>` program.
fn helper_name(cfg: &serde_json::Value, registry: &str) -> Option<String> {
    let per = config_keys(registry).iter().find_map(|k| cfg["credHelpers"][k.as_str()].as_str().map(String::from));
    per.or_else(|| cfg["credsStore"].as_str().map(String::from)).filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
}

/// A base64 `auth` (or plain `username`/`password`) entry under `auths`.
fn config_auth(cfg: &serde_json::Value, registry: &str) -> Option<Creds> {
    let e = config_keys(registry).iter().find_map(|k| cfg["auths"].get(k.as_str()).filter(|v| v.is_object()))?;
    if let Some(a) = e["auth"].as_str().filter(|a| !a.is_empty()) {
        let (u, p) = String::from_utf8(base64_decode(a)?).ok()?.split_once(':').map(|(u, p)| (u.to_string(), p.to_string()))?;
        return Some(Creds { user: u, password: p });
    }
    Some(Creds { user: e["username"].as_str()?.into(), password: e["password"].as_str()?.into() })
}

/// Run `docker-credential-<name> <verb>`, feeding `input` on stdin. None if the helper is missing or fails.
fn run_helper(name: &str, verb: &str, input: &str) -> Option<Vec<u8>> {
    use std::{io::Write, process::{Command, Stdio}};
    let mut c = Command::new(format!("docker-credential-{name}")).arg(verb).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    c.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = c.wait_with_output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn helper_get(name: &str, registry: &str) -> Option<Creds> {
    let v: serde_json::Value = serde_json::from_slice(&run_helper(name, "get", &server_url(registry))?).ok()?;
    let (u, p) = (v["Username"].as_str()?, v["Secret"].as_str()?);
    // "<token>" marks an identity/refresh token, which is not a password we can use for basic auth
    (u != "<token>" && !p.is_empty()).then(|| Creds { user: u.into(), password: p.into() })
}

/// Our own login first, then the credential helper docker is configured with, then base64 `auths` in config.json.
pub fn load(root: &Path, registry: &str) -> Option<Creds> {
    if let Some(c) = read_all(root).remove(registry) {
        return Some(c);
    }
    let cfg = docker_config()?;
    helper_name(&cfg, registry).and_then(|n| helper_get(&n, registry)).or_else(|| config_auth(&cfg, registry))
}

/// Check the credentials against the registry, then remember them: in the configured credential helper if
/// there is one (nothing secret touches disk here), else in `<store>/auth.json`. Returns the registry's canonical name.
pub fn login(root: &Path, registry: &str, user: &str, password: &str) -> Result<String> {
    if user.is_empty() || password.is_empty() {
        return Err(err("a username and a password are required"));
    }
    let reg = normalize_registry(registry)?;
    let creds = Creds { user: user.into(), password: password.into() };
    Client::probe(&reg, &root.join("tmp"), &creds)?;
    let mut all = read_all(root);
    let helper = docker_config().and_then(|c| helper_name(&c, &reg));
    let input = serde_json::json!({ "ServerURL": server_url(&reg), "Username": user, "Secret": password }).to_string();
    if helper.is_some_and(|n| run_helper(&n, "store", &input).is_some()) {
        if all.remove(&reg).is_some() {
            write_all(root, &all)?; // drop a stale plain-text copy
        }
        return Ok(reg);
    }
    // no helper, or it failed: the 0600 file is the fallback
    all.insert(reg.clone(), creds);
    write_all(root, &all)?;
    Ok(reg)
}

/// Forget the credentials (file and helper). Returns false if there were none.
pub fn logout(root: &Path, registry: &str) -> Result<bool> {
    let reg = normalize_registry(registry)?;
    let mut all = read_all(root);
    let mut had = all.remove(&reg).is_some();
    if had {
        write_all(root, &all)?;
    }
    if let Some(n) = docker_config().and_then(|c| helper_name(&c, &reg)) {
        had |= helper_get(&n, &reg).is_some() && run_helper(&n, "erase", &server_url(&reg)).is_some();
    }
    Ok(had)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
        assert_eq!(Creds { user: "u".into(), password: "p".into() }.basic(), "Basic dTpw");
    }

    #[test]
    fn base64_decodes_what_it_encodes() {
        for s in ["", "f", "fo", "foo", "foob", "me:s3cret/+"] {
            assert_eq!(base64_decode(&base64(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert!(base64_decode("a$b").is_none());
    }

    #[test]
    fn reads_docker_config() {
        let cfg: serde_json::Value = serde_json::from_str(
            r#"{"credsStore":"desktop","credHelpers":{"ghcr.io":"pass"},
                "auths":{"https://index.docker.io/v1/":{"auth":"dTpw"},"my.io":{"username":"a","password":"b"},"empty.io":{}}}"#,
        )
        .unwrap();
        assert_eq!(helper_name(&cfg, "ghcr.io").as_deref(), Some("pass")); // per-registry beats global
        assert_eq!(helper_name(&cfg, "my.io").as_deref(), Some("desktop"));
        assert_eq!(config_auth(&cfg, crate::reference::HUB), Some(Creds { user: "u".into(), password: "p".into() }));
        assert_eq!(config_auth(&cfg, "my.io").unwrap().password, "b");
        assert!(config_auth(&cfg, "empty.io").is_none() && config_auth(&cfg, "none.io").is_none());
        let bad: serde_json::Value = serde_json::from_str(r#"{"credsStore":"../evil"}"#).unwrap();
        assert!(helper_name(&bad, "x.io").is_none());
        assert_eq!(server_url(crate::reference::HUB), "https://index.docker.io/v1/");
    }

    #[test]
    fn credentials_round_trip_and_are_per_registry() {
        let root = std::env::temp_dir().join(format!("rd-auth-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut all = BTreeMap::new();
        all.insert("a.io".to_string(), Creds { user: "u".into(), password: "p".into() });
        write_all(&root, &all).unwrap();
        assert_eq!(load(&root, "a.io").unwrap().user, "u");
        assert!(load(&root, "b.io").is_none());
        assert!(logout(&root, "a.io").unwrap());
        assert!(!logout(&root, "a.io").unwrap());
        assert!(load(&root, "a.io").is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
