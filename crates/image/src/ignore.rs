//! `.dockerignore`: which files of the build context `COPY` must not see.
//! Patterns are relative to the context root; `*` and `?` stay inside one path segment, `**` spans segments,
//! a pattern also excludes everything below a matching directory, `!pattern` re-includes, the last match wins.

#[derive(Debug, Default, Clone)]
pub struct Ignore {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    segs: Vec<String>,
    negate: bool,
}

fn glob(pat: &str, s: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|k| go(rest, &t[k..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(&pat.chars().collect::<Vec<_>>(), &s.chars().collect::<Vec<_>>())
}

fn segs_match(pat: &[String], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => (0..=path.len()).any(|k| segs_match(rest, &path[k..])),
        Some((first, rest)) => !path.is_empty() && glob(first, path[0]) && segs_match(rest, &path[1..]),
    }
}

impl Ignore {
    pub fn parse(text: &str) -> Ignore {
        let rules = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| {
                let (negate, l) = l.strip_prefix('!').map_or((false, l), |r| (true, r.trim()));
                let segs: Vec<String> =
                    l.split('/').filter(|s| !s.is_empty() && *s != ".").map(String::from).collect();
                (!segs.is_empty()).then_some(Rule { segs, negate })
            })
            .collect();
        Ignore { rules }
    }

    /// With `!` rules a skipped directory may still hold re-included files, so callers must look inside.
    pub fn has_negations(&self) -> bool {
        self.rules.iter().any(|r| r.negate)
    }

    /// `rel` is the path inside the context, `/`-separated.
    pub fn is_ignored(&self, rel: &str) -> bool {
        let path: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        let mut ignored = false;
        for r in &self.rules {
            if (1..=path.len()).any(|n| segs_match(&r.segs, &path[..n])) {
                ignored = !r.negate;
            }
        }
        ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(t: &str) -> Ignore {
        Ignore::parse(t)
    }

    #[test]
    fn names_globs_and_directories() {
        let i = ig("# build output\nnode_modules\n*.log\n/dist/\n\n**/tmp\nsrc/*/gen\n");
        for hit in ["node_modules", "node_modules/a/b.js", "app.log", "dist", "dist/x", "a/b/tmp", "tmp/x", "src/mod/gen/f"] {
            assert!(i.is_ignored(hit), "{hit} should be ignored");
        }
        for miss in ["src/main.rs", "src/gen", "app.logx", "a/dist", "readme.md", "src/a/b/gen"] {
            assert!(!i.is_ignored(miss), "{miss} should be kept");
        }
    }

    #[test]
    fn negation_and_last_match_wins() {
        let i = ig("*.md\n!README.md\nvendor\n!vendor/keep.txt\n");
        assert!(i.is_ignored("CHANGELOG.md") && !i.is_ignored("README.md"));
        assert!(i.is_ignored("vendor/other") && !i.is_ignored("vendor/keep.txt"));
        assert!(i.has_negations() && !ig("a\nb").has_negations());
    }

    #[test]
    fn glob_pieces() {
        assert!(glob("a*c", "abbbc") && glob("a?c", "abc") && !glob("a?c", "ac") && !glob("*.rs", "x.rsx"));
        assert!(!ig("*.txt").is_ignored("dir/file.txt"));
        assert!(ig("**/*.txt").is_ignored("a/b/c.txt") && ig("**/*.txt").is_ignored("c.txt"));
        assert!(!ig("").is_ignored("anything"));
    }
}
