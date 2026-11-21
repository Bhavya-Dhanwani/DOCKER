use crate::error::{err, Result};

pub const HUB: &str = "registry-1.docker.io";

/// `alpine`, `alpine:3.19`, `user/app:v1`, `ghcr.io/org/app:tag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repo: String,
    pub tag: String,
}

/// `docker login`'s server argument: "" and the Docker Hub aliases mean Docker Hub; a scheme is ignored.
pub fn normalize_registry(s: &str) -> Result<String> {
    let s = s.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
    let host = match s {
        "" | "docker.io" | "index.docker.io" | "registry-1.docker.io" => HUB,
        other => other,
    };
    if host.starts_with('.') || host.contains("..") || !host.chars().all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c)) {
        return Err(err(format!("bad registry name {s:?}")));
    }
    Ok(host.to_string())
}

impl Reference {
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() || s.contains(char::is_whitespace) || s.contains('@') {
            return Err(err(format!("unsupported image reference {s:?}")));
        }
        let (name, tag) = match s.rfind(':') {
            Some(i) if !s[i + 1..].contains('/') => (&s[..i], &s[i + 1..]),
            _ => (s, "latest"),
        };
        if name.is_empty() || tag.is_empty() {
            return Err(err(format!("bad image reference {s:?}")));
        }
        let (registry, repo) = match name.split_once('/') {
            Some((h, rest)) if h.contains('.') || h.contains(':') || h == "localhost" => {
                let h = if h == "docker.io" || h == "index.docker.io" { HUB } else { h };
                (h.to_string(), rest.to_string())
            }
            _ if name.contains('/') => (HUB.to_string(), name.to_string()),
            _ => (HUB.to_string(), format!("library/{name}")),
        };
        // the repo ends up in URLs and file names: keep it boring
        let ok = |c: char| c.is_ascii_alphanumeric() || "._-/".contains(c);
        let host_ok = registry.chars().all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c)) && !registry.starts_with('.');
        if !host_ok || !repo.chars().all(ok) || repo.contains("..") || !tag.chars().all(|c| ok(c) && c != '/') {
            return Err(err(format!("bad image reference {s:?}")));
        }
        Ok(Reference { registry, repo, tag: tag.to_string() })
    }

    /// Name as Docker prints it: `alpine:3.19`, `ghcr.io/org/app:tag`.
    pub fn display(&self) -> String {
        if self.registry == HUB {
            format!("{}:{}", self.repo.strip_prefix("library/").unwrap_or(&self.repo), self.tag)
        } else {
            format!("{}/{}:{}", self.registry, self.repo, self.tag)
        }
    }

    /// File-name-safe form of `display`.
    pub fn key(&self) -> String {
        self.display().replace(['/', ':'], "_")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> (String, String, String) {
        let r = Reference::parse(s).unwrap();
        (r.registry, r.repo, r.tag)
    }

    #[test]
    fn docker_hub_shortcuts() {
        assert_eq!(p("alpine"), (HUB.into(), "library/alpine".into(), "latest".into()));
        assert_eq!(p("alpine:3.19").2, "3.19");
        assert_eq!(p("user/app:v1"), (HUB.into(), "user/app".into(), "v1".into()));
        assert_eq!(p("docker.io/library/nginx").1, "library/nginx");
    }

    #[test]
    fn other_registries() {
        assert_eq!(p("ghcr.io/org/app:t"), ("ghcr.io".into(), "org/app".into(), "t".into()));
        assert_eq!(p("localhost:5000/a").0, "localhost:5000");
    }

    #[test]
    fn display_and_key() {
        let r = Reference::parse("alpine").unwrap();
        assert_eq!(r.display(), "alpine:latest");
        assert_eq!(Reference::parse("ghcr.io/o/a:t").unwrap().key(), "ghcr.io_o_a_t");
    }

    #[test]
    fn registry_names_are_normalized() {
        assert_eq!(normalize_registry("").unwrap(), HUB);
        assert_eq!(normalize_registry("docker.io").unwrap(), HUB);
        assert_eq!(normalize_registry("https://ghcr.io/").unwrap(), "ghcr.io");
        assert_eq!(normalize_registry("localhost:5000").unwrap(), "localhost:5000");
        assert!(normalize_registry("a b").is_err());
        assert!(normalize_registry("../x").is_err());
    }

    #[test]
    fn rejects_junk() {
        for s in ["", "a b", "a@sha256:x", "../etc", "a:", "a/../b"] {
            assert!(Reference::parse(s).is_err(), "{s}");
        }
    }
}
