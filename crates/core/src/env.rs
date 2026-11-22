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

/// The lines of an env file (`docker run --env-file`, compose `env_file:`) as `KEY=VALUE` entries.
/// Blank lines and `#` comments are skipped, `export ` is allowed, values may be quoted, and a bare `KEY` copies the host's value.
pub fn parse_env_file(text: &str) -> Vec<String> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let l = l.strip_prefix("export ").map_or(l, str::trim_start);
            let Some((k, v)) = l.split_once('=') else { return Some(l.to_string()) };
            let (k, v) = (k.trim(), v.trim());
            let v = match v.chars().next() {
                Some(q @ ('"' | '\'')) => v[1..].split_once(q).map_or(&v[1..], |(inside, _)| inside).to_string(),
                _ => v.split(" #").next().unwrap_or("").trim_end().to_string(), // an unquoted value ends at ` #comment`
            };
            (!k.is_empty()).then(|| format!("{k}={v}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_lines() {
        let t = "\u{feff}# comment\nA=1\nexport B = two words \nC=\"quoted # not a comment\"\nD='single'\nE=plain # trailing\n\nF\nURL=postgres://u:p@h/db?x=1\n";
        assert_eq!(
            parse_env_file(t),
            ["A=1", "B=two words", "C=quoted # not a comment", "D=single", "E=plain", "F", "URL=postgres://u:p@h/db?x=1"]
        );
    }

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
