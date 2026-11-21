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

pub fn load(root: &Path, registry: &str) -> Option<Creds> {
    read_all(root).remove(registry)
}

/// Check the credentials against the registry, then remember them. Returns the registry's canonical name.
pub fn login(root: &Path, registry: &str, user: &str, password: &str) -> Result<String> {
    if user.is_empty() || password.is_empty() {
        return Err(err("a username and a password are required"));
    }
    let reg = normalize_registry(registry)?;
    let creds = Creds { user: user.into(), password: password.into() };
    Client::probe(&reg, &root.join("tmp"), &creds)?;
    let mut all = read_all(root);
    all.insert(reg.clone(), creds);
    write_all(root, &all)?;
    Ok(reg)
}

/// Forget the credentials. Returns false if there were none.
pub fn logout(root: &Path, registry: &str) -> Result<bool> {
    let reg = normalize_registry(registry)?;
    let mut all = read_all(root);
    let had = all.remove(&reg).is_some();
    if had {
        write_all(root, &all)?;
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
