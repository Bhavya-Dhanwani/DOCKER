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

/// Parse the inside of a `[...]` class: (negated, members, pattern after the `]`).
fn class(p: &[char]) -> Option<(bool, Vec<char>, &[char])> {
    let (negate, p) = match p.first() {
        Some('^' | '!') => (true, &p[1..]),
        _ => (false, p),
    };
    let end = p.iter().skip(1).position(|&c| c == ']')? + 1; // a leading `]` is a member
    Some((negate, p[..end].to_vec(), &p[end + 1..]))
}

fn class_has(set: &[char], c: char) -> bool {
    let mut i = 0;
    while i < set.len() {
        if set.get(i + 1) == Some(&'-') && i + 2 < set.len() {
            if (set[i]..=set[i + 2]).contains(&c) {
                return true;
            }
            i += 3;
        } else {
            if set[i] == c {
                return true;
            }
            i += 1;
        }
    }
    false
}

fn glob(pat: &str, s: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|k| go(rest, &t[k..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            // `[a-z]`, `[^abc]`: one character of the class (an unclosed `[` is a literal)
            Some(('[', rest)) => match class(rest) {
                Some((negate, set, after)) => t.first().is_some_and(|&c| class_has(&set, c) != negate) && go(after, &t[1..]),
                None => t.first() == Some(&'[') && go(rest, &t[1..]),
            },
            // `*`: the next character literally
            Some(('\\', rest)) if !rest.is_empty() => t.first() == Some(&rest[0]) && go(&rest[1..], &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(&pat.chars().collect::<Vec<_>>(), &s.chars().collect::<Vec<_>>())
}

/// Expand a `COPY` source with wildcards (`package*.json`, `src/*.rs`) against the build context.
/// Returns the matching paths relative to `ctx`, sorted; a pattern without wildcards is returned as is.
pub fn expand_glob(ctx: &std::path::Path, pattern: &str) -> Vec<String> {
    expand_with(
        pattern,
        |dir| std::fs::read_dir(ctx.join(dir)).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default(),
        |p| ctx.join(p).symlink_metadata().is_ok(),
    )
}

/// The same for any file tree: `list(dir)` gives the names inside a directory, `exists(path)` tests a plain path
/// (both relative, `""` is the root). Used for `COPY --from=<stage>`, where the tree is a set of image layers.
pub fn expand_with(pattern: &str, list: impl Fn(&str) -> Vec<String>, exists: impl Fn(&str) -> bool) -> Vec<String> {
    let pat = pattern.trim_start_matches('/');
    if !pat.contains(['*', '?', '[']) {
        return vec![pattern.to_string()];
    }
    let mut found = vec![String::new()];
    for seg in pat.split('/').filter(|s| !s.is_empty() && *s != ".") {
        let mut next = vec![];
        for base in &found {
            let join = |n: &str| if base.is_empty() { n.to_string() } else { format!("{base}/{n}") };
            if !seg.contains(['*', '?', '[']) {
                if exists(&join(seg)) {
                    next.push(join(seg));
                }
                continue;
            }
            next.extend(list(base).into_iter().filter(|n| glob(seg, n)).map(|n| join(&n)));
        }
        found = next;
    }
    found.sort();
    found
}

fn segs_match(pat: &[String], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        // a trailing `**` needs at least one more segment (`a/**` does not match `a` itself, like Docker)
        Some((first, rest)) if first == "**" && rest.is_empty() => !path.is_empty(),
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
    fn classes_escapes_and_trailing_doublestar() {
        let i = ig("file[0-9].txt\n[^a-c]x\n\\*star\nout/**\n!out/keep\n");
        assert!(i.is_ignored("file3.txt") && !i.is_ignored("filex.txt"));
        assert!(i.is_ignored("dx") && !i.is_ignored("bx"));
        assert!(i.is_ignored("*star") && !i.is_ignored("zstar"));
        assert!(i.is_ignored("out/a/b") && !i.is_ignored("out") && !i.is_ignored("out/keep"));
    }

    #[test]
    fn copy_wildcards_expand_against_the_context() {
        let d = std::env::temp_dir().join(format!("expand-glob-{}", std::process::id()));
        std::fs::create_dir_all(d.join("src")).unwrap();
        for f in ["package.json", "package-lock.json", "readme.md", "src/a.rs", "src/b.rs"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        assert_eq!(expand_glob(&d, "package*.json"), ["package-lock.json", "package.json"]);
        assert_eq!(expand_glob(&d, "/src/*.rs"), ["src/a.rs", "src/b.rs"]);
        assert_eq!(expand_glob(&d, "plain.txt"), ["plain.txt"]);
        assert!(expand_glob(&d, "*.zip").is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn glob_pieces() {
        assert!(glob("a*c", "abbbc") && glob("a?c", "abc") && !glob("a?c", "ac") && !glob("*.rs", "x.rsx"));
        assert!(!ig("*.txt").is_ignored("dir/file.txt"));
        assert!(ig("**/*.txt").is_ignored("a/b/c.txt") && ig("**/*.txt").is_ignored("c.txt"));
        assert!(!ig("").is_ignored("anything"));
    }
}
