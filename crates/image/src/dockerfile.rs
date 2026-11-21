//! Dockerfile parsing: logical lines, instructions, shell-style words, `$VAR` substitution.
use crate::error::{err, Error, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// `RUN echo hi`: run through `/bin/sh -c`.
    Shell(String),
    /// `RUN ["echo", "hi"]`
    Exec(Vec<String>),
}

/// `HEALTHCHECK [--interval=30s] [--timeout=30s] [--start-period=0s] [--retries=3] CMD ...`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSpec {
    pub cmd: Cmd,
    pub interval_s: Option<u64>,
    pub timeout_s: Option<u64>,
    pub start_period_s: Option<u64>,
    pub retries: Option<u32>,
}

/// `30s`, `5m`, `1h30m`, `500ms` (rounded up to whole seconds), or a bare number of seconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    let (mut total_ms, mut num, mut any) = (0u64, String::new(), false);
    let mut chars = s.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        total_ms += match c {
            'h' => n * 3_600_000,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                n
            }
            'm' => n * 60_000,
            's' => n * 1000,
            _ => return None,
        };
        any = true;
    }
    if !num.is_empty() {
        total_ms += num.parse::<u64>().ok()? * 1000; // bare seconds
        any = true;
    }
    any.then(|| total_ms.div_ceil(1000))
}

fn healthcheck(rest: &str, line: usize) -> Result<Option<HealthSpec>> {
    if rest.trim().eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let mut spec = HealthSpec { cmd: Cmd::Shell(String::new()), interval_s: None, timeout_s: None, start_period_s: None, retries: None };
    let mut rest = rest.trim_start();
    while let Some(flag) = rest.strip_prefix("--") {
        let end = flag.find(char::is_whitespace).unwrap_or(flag.len());
        let (name, value) = flag[..end].split_once('=').ok_or_else(|| at(line, "HEALTHCHECK flags look like --interval=30s"))?;
        let secs = || parse_duration(value).ok_or_else(|| at(line, format!("bad duration {value:?}")));
        match name {
            "interval" => spec.interval_s = Some(secs()?),
            "timeout" => spec.timeout_s = Some(secs()?),
            "start-period" => spec.start_period_s = Some(secs()?),
            "retries" => spec.retries = Some(value.parse().map_err(|_| at(line, "bad --retries"))?),
            other => return Err(at(line, format!("unknown HEALTHCHECK flag --{other}"))),
        }
        rest = flag[end..].trim_start();
    }
    let body = rest.strip_prefix("CMD").or_else(|| rest.strip_prefix("cmd")).ok_or_else(|| at(line, "HEALTHCHECK needs `CMD <command>` or `NONE`"))?;
    spec.cmd = cmd(body.trim());
    Ok(Some(spec))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    From { image: String, alias: Option<String> },
    Run(Cmd),
    Cmd(Cmd),
    Entrypoint(Cmd),
    /// `COPY` and `ADD` (local files only). `from` is a build stage or an image; `chown` is `user[:group]`.
    Copy { srcs: Vec<String>, dst: String, from: Option<String>, chown: Option<String> },
    Env(Vec<(String, String)>),
    Arg { name: String, default: Option<String> },
    Workdir(String),
    Expose(Vec<String>),
    Label(Vec<(String, String)>),
    User(String),
    /// `HEALTHCHECK NONE` is `None`.
    Healthcheck(Option<HealthSpec>),
    /// Accepted but has no effect here (VOLUME, STOPSIGNAL, MAINTAINER).
    Ignored(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub line: usize,
    /// The instruction as written (continuations joined), for output and cache keys.
    pub text: String,
    pub inst: Instruction,
}

fn at(line: usize, msg: impl std::fmt::Display) -> Error {
    err(format!("Dockerfile line {line}: {msg}"))
}

/// Logical lines (continuations joined, comments and blanks dropped) with their starting line number.
fn logical_lines(src: &str) -> Vec<(usize, String)> {
    let mut out = vec![];
    let mut cur: Option<(usize, String)> = None;
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            continue; // comments and blank lines are skipped, even inside a continuation
        }
        let (n, mut text) = cur.take().unwrap_or((i + 1, String::new()));
        if !text.is_empty() {
            text.push(' ');
        }
        match t.strip_suffix('\\') {
            Some(body) => {
                text.push_str(body.trim_end());
                cur = Some((n, text));
            }
            None => {
                text.push_str(t);
                out.push((n, text));
            }
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    out
}

/// Shell-like word splitting: whitespace separates, `'..'` and `".."` group, `\x` escapes.
/// `\$` is kept as-is so `subst` can tell it was escaped.
pub fn words(s: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut started = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err(err("unterminated single quote")),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => cur.push(c),
                            Some('$') => cur.push_str("\\$"),
                            Some(c) => {
                                cur.push('\\');
                                cur.push(c);
                            }
                            None => return Err(err("unterminated double quote")),
                        },
                        Some(c) => cur.push(c),
                        None => return Err(err("unterminated double quote")),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some('$') => cur.push_str("\\$"),
                    Some(c) => cur.push(c),
                    None => cur.push('\\'),
                }
            }
            c => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// Expand `$NAME`, `${NAME}` and `${NAME:-default}`; `\$` is a literal dollar. Unknown names expand to "".
pub fn subst(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if chars.get(i + 1) == Some(&'$') => {
                out.push('$');
                i += 2;
            }
            '$' if chars.get(i + 1) == Some(&'{') => match chars[i + 2..].iter().position(|&c| c == '}') {
                Some(end) => {
                    let body: String = chars[i + 2..i + 2 + end].iter().collect();
                    let (name, default) = match body.split_once(":-") {
                        Some((n, d)) => (n.to_string(), Some(d.to_string())),
                        None => (body, None),
                    };
                    match vars.get(&name).filter(|v| !v.is_empty()) {
                        Some(v) => out.push_str(v),
                        None => out.push_str(&default.unwrap_or_default()),
                    }
                    i += 3 + end;
                }
                None => {
                    out.push('$');
                    i += 1;
                }
            },
            '$' if chars.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == '_') => {
                let name: String = chars[i + 1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == '_').collect();
                out.push_str(vars.get(&name).map_or("", String::as_str));
                i += 1 + name.chars().count();
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn cmd(rest: &str) -> Cmd {
    if rest.trim_start().starts_with('[')
        && let Ok(v) = serde_json::from_str::<Vec<String>>(rest.trim())
    {
        return Cmd::Exec(v);
    }
    Cmd::Shell(rest.trim().to_string())
}

/// `K=V K2="v 2"` pairs (ENV, LABEL).
fn pairs(rest: &str, line: usize) -> Result<Vec<(String, String)>> {
    words(rest)?
        .into_iter()
        .map(|w| {
            w.split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| at(line, format!("expected KEY=VALUE, got {w:?}")))
        })
        .collect()
}

fn string_list(rest: &str) -> Result<Vec<String>> {
    if rest.trim_start().starts_with('[')
        && let Ok(v) = serde_json::from_str::<Vec<String>>(rest.trim())
    {
        return Ok(v);
    }
    words(rest)
}

/// Leading `--from=X` / `--chown=U:G` flags of COPY/ADD; returns them and the rest of the line.
fn copy_flags(rest: &str, kw: &str, line: usize) -> Result<(Option<String>, Option<String>, String)> {
    let (mut from, mut chown) = (None, None);
    let mut rest = rest.trim_start();
    while let Some(flag) = rest.strip_prefix("--") {
        let end = flag.find(char::is_whitespace).unwrap_or(flag.len());
        let (name, value) = flag[..end].split_once('=').unwrap_or((&flag[..end], ""));
        match (name, value) {
            ("from", v) if !v.is_empty() && kw == "COPY" => from = Some(v.to_string()),
            ("chown", v) if !v.is_empty() => chown = Some(v.to_string()),
            _ => return Err(at(line, format!("{kw} --{name} is not supported (supported: --from=<stage|image>, --chown=<user[:group]>)"))),
        }
        rest = flag[end..].trim_start();
    }
    Ok((from, chown, rest.to_string()))
}

fn reject_flags(rest: &str, kw: &str, line: usize) -> Result<()> {
    match rest.trim_start().strip_prefix("--") {
        Some(flag) => Err(at(line, format!("{kw} --{} is not supported yet", flag.split(['=', ' ']).next().unwrap_or("")))),
        None => Ok(()),
    }
}

pub fn parse(src: &str) -> Result<Vec<Step>> {
    let mut steps = vec![];
    for (line, text) in logical_lines(src) {
        let (kw, rest) = text.split_once(char::is_whitespace).map_or((text.as_str(), ""), |(k, r)| (k, r.trim()));
        let upper = kw.to_ascii_uppercase();
        if rest.is_empty() && upper != "ENTRYPOINT" {
            return Err(at(line, format!("{upper} needs an argument")));
        }
        let inst = match upper.as_str() {
            "FROM" => {
                reject_flags(rest, "FROM", line)?;
                let w = words(rest)?;
                match w.as_slice() {
                    [img] => Instruction::From { image: img.clone(), alias: None },
                    [img, as_, name] if as_.eq_ignore_ascii_case("as") => {
                        Instruction::From { image: img.clone(), alias: Some(name.clone()) }
                    }
                    _ => return Err(at(line, "expected FROM <image> [AS <name>]")),
                }
            }
            "RUN" => {
                reject_flags(rest, "RUN", line)?;
                Instruction::Run(cmd(rest))
            }
            "CMD" => Instruction::Cmd(cmd(rest)),
            "ENTRYPOINT" => Instruction::Entrypoint(if rest.is_empty() { Cmd::Exec(vec![]) } else { cmd(rest) }),
            "COPY" | "ADD" => {
                let (from, chown, rest) = copy_flags(rest, &upper, line)?;
                let mut w = string_list(&rest)?;
                if w.len() < 2 {
                    return Err(at(line, format!("{upper} needs at least a source and a destination")));
                }
                let dst = w.pop().expect("len >= 2");
                if upper == "ADD" && w.iter().any(|s| s.contains("://")) {
                    return Err(at(line, "ADD from a URL is not supported yet"));
                }
                Instruction::Copy { srcs: w, dst, from, chown }
            }
            "ENV" => {
                let first = rest.split_whitespace().next().unwrap_or("");
                if first.contains('=') {
                    Instruction::Env(pairs(rest, line)?)
                } else {
                    // legacy `ENV key value with spaces`
                    let (k, v) = rest.split_once(char::is_whitespace).ok_or_else(|| at(line, "ENV needs a value"))?;
                    Instruction::Env(vec![(k.to_string(), v.trim().to_string())])
                }
            }
            "ARG" => match rest.split_once('=') {
                Some((n, d)) => Instruction::Arg { name: n.trim().into(), default: Some(words(d)?.concat()) },
                None => Instruction::Arg { name: rest.into(), default: None },
            },
            "WORKDIR" => Instruction::Workdir(rest.to_string()),
            "EXPOSE" => Instruction::Expose(words(rest)?),
            "LABEL" => Instruction::Label(pairs(rest, line)?),
            "USER" => Instruction::User(rest.to_string()),
            "HEALTHCHECK" => Instruction::Healthcheck(healthcheck(rest, line)?),
            "VOLUME" | "STOPSIGNAL" | "MAINTAINER" => Instruction::Ignored(upper),
            "SHELL" | "ONBUILD" => return Err(at(line, format!("{upper} is not supported yet"))),
            other => return Err(at(line, format!("unknown instruction {other}"))),
        };
        steps.push(Step { line, text, inst });
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(kv: &[(&str, &str)]) -> HashMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn one(s: &str) -> Instruction {
        parse(s).unwrap().remove(0).inst
    }

    #[test]
    fn joins_continuations_and_skips_comments() {
        let steps = parse("# hi\nFROM alpine\n\nRUN echo a \\\n  # note\n  && echo b\nCMD [\"sh\"]\n").unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1].line, 4);
        assert_eq!(steps[1].inst, Instruction::Run(Cmd::Shell("echo a && echo b".into())));
        assert_eq!(steps[2].inst, Instruction::Cmd(Cmd::Exec(vec!["sh".into()])));
    }

    #[test]
    fn keywords_are_case_insensitive_and_from_alias() {
        assert_eq!(one("from alpine:3.19 as build"), Instruction::From { image: "alpine:3.19".into(), alias: Some("build".into()) });
    }

    #[test]
    fn env_both_forms() {
        assert_eq!(
            one("ENV A=1 B=\"two words\" C='x y'"),
            Instruction::Env(vec![("A".into(), "1".into()), ("B".into(), "two words".into()), ("C".into(), "x y".into())])
        );
        assert_eq!(one("ENV GREETING hello big world"), Instruction::Env(vec![("GREETING".into(), "hello big world".into())]));
    }

    #[test]
    fn copy_arg_workdir_label_expose() {
        assert_eq!(one("COPY a b /dst/"), Instruction::Copy { srcs: vec!["a".into(), "b".into()], dst: "/dst/".into(), from: None, chown: None });
        assert_eq!(one("COPY [\"a b\", \"/d\"]"), Instruction::Copy { srcs: vec!["a b".into()], dst: "/d".into(), from: None, chown: None });
        assert_eq!(
            one("COPY --from=build --chown=1000:1000 /out/app /usr/bin/app"),
            Instruction::Copy { srcs: vec!["/out/app".into()], dst: "/usr/bin/app".into(), from: Some("build".into()), chown: Some("1000:1000".into()) }
        );
        assert_eq!(one("ADD --chown=u f /f"), Instruction::Copy { srcs: vec!["f".into()], dst: "/f".into(), from: None, chown: Some("u".into()) });
        assert_eq!(one("ARG V=1.2"), Instruction::Arg { name: "V".into(), default: Some("1.2".into()) });
        assert_eq!(one("ARG V"), Instruction::Arg { name: "V".into(), default: None });
        assert_eq!(one("WORKDIR /app"), Instruction::Workdir("/app".into()));
        assert_eq!(one("EXPOSE 80 443/tcp"), Instruction::Expose(vec!["80".into(), "443/tcp".into()]));
        assert_eq!(one("LABEL a=b"), Instruction::Label(vec![("a".into(), "b".into())]));
    }

    #[test]
    fn healthcheck_instruction() {
        assert_eq!(one("HEALTHCHECK NONE"), Instruction::Healthcheck(None));
        assert_eq!(
            one("HEALTHCHECK --interval=5s --timeout=2s --start-period=1m --retries=2 CMD curl -f http://localhost/"),
            Instruction::Healthcheck(Some(HealthSpec {
                cmd: Cmd::Shell("curl -f http://localhost/".into()),
                interval_s: Some(5),
                timeout_s: Some(2),
                start_period_s: Some(60),
                retries: Some(2)
            }))
        );
        let Instruction::Healthcheck(Some(h)) = one("HEALTHCHECK CMD [\"/bin/check\", \"-x\"]") else { panic!() };
        assert_eq!(h.cmd, Cmd::Exec(vec!["/bin/check".into(), "-x".into()]));
        for bad in ["HEALTHCHECK curl x", "HEALTHCHECK --interval=zz CMD x", "HEALTHCHECK --bogus=1 CMD x", "HEALTHCHECK --interval CMD x"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations() {
        for (s, want) in [("30s", 30), ("5m", 300), ("1h30m", 5400), ("1m30s", 90), ("45", 45), ("500ms", 1), ("1500ms", 2)] {
            assert_eq!(parse_duration(s), Some(want), "{s}");
        }
        for bad in ["", "s", "5x", "1.5s"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn exec_form_falls_back_to_shell_on_bad_json() {
        assert_eq!(one("RUN [not json"), Instruction::Run(Cmd::Shell("[not json".into())));
    }

    #[test]
    fn word_splitting() {
        assert_eq!(words(r#"a "b c" 'd e' f\ g"#).unwrap(), ["a", "b c", "d e", "f g"]);
        assert_eq!(words("''").unwrap(), [""]);
        assert!(words("'open").is_err());
    }

    #[test]
    fn variable_substitution() {
        let v = vars(&[("A", "1"), ("EMPTY", "")]);
        assert_eq!(subst("x$A-${A}-$NOPE-${NOPE:-dflt}-${EMPTY:-e}", &v), "x1-1--dflt-e");
        assert_eq!(subst("cost \\$5 and $", &v), "cost $5 and $");
        assert_eq!(subst("${unterminated", &v), "${unterminated");
    }

    #[test]
    fn rejects_what_it_cannot_do() {
        for bad in [
            "FROMM x", "RUN", "COPY a", "COPY --from= a /b", "COPY --chmod=755 a /b", "ADD --from=b a /b", "RUN --mount=type=cache x", "ONBUILD RUN x",
            "FROM a b c", "ENV A", "ADD http://x/y /z", "LABEL novalue",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(parse("ENTRYPOINT").is_ok()); // resets the entrypoint
    }
}
