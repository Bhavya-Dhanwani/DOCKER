//! Merge `-e` overrides into an image's environment.

/// `K=V` replaces/adds K; a bare `K` copies the host's value (and is skipped if unset).
pub fn merge(base: &[String], overrides: &[String], host: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out: Vec<String> = base.to_vec();
    for o in overrides {
        let (key, kv) = match o.split_once('=') {
            Some((k, _)) => (k, o.clone()),
            None => match host(o) {
                Some(v) => (o.as_str(), format!("{o}={v}")),
                None => continue,
            },
        };
        let prefix = format!("{key}=");
        match out.iter().position(|e| e.starts_with(&prefix)) {
            Some(i) => out[i] = kv,
            None => out.push(kv),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn overrides_adds_and_inherits() {
        let host = |k: &str| (k == "HOME").then(|| "/h".to_string());
        let out = merge(&s(&["PATH=/bin", "A=1"]), &s(&["A=2", "B=3", "HOME", "MISSING"]), host);
        assert_eq!(out, s(&["PATH=/bin", "A=2", "B=3", "HOME=/h"]));
    }

    #[test]
    fn empty_value_is_kept() {
        assert_eq!(merge(&[], &s(&["E="]), |_| None), s(&["E="]));
    }
}
