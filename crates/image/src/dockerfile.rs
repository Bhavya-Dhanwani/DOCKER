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
    From { image: String, alias: Option<String>, platform: Option<String> },
    Run { cmd: Cmd, mounts: Vec<Mount> },
    Cmd(Cmd),
    Entrypoint(Cmd),
    /// `COPY` and `ADD`.
    Copy(CopySpec),
    Env(Vec<(String, String)>),
    Arg { name: String, default: Option<String> },
    Workdir(String),
    Expose(Vec<String>),
    Label(Vec<(String, String)>),
    User(String),
    /// `HEALTHCHECK NONE` is `None`.
    Healthcheck(Option<HealthSpec>),
    /// `SHELL ["/bin/bash", "-c"]`: what shell-form RUN/CMD/ENTRYPOINT use from here on.
    Shell(Vec<String>),
    /// `ONBUILD <instruction>`: the instruction text, run when this image is used as a `FROM`.
    Onbuild(String),
    Volume(Vec<String>),
    StopSignal(String),
    /// Accepted but has no effect here (MAINTAINER).
    Ignored(String),
}

/// `COPY` / `ADD`. `from` is a build stage or an image; `chown` is `user[:group]`, `chmod` octal.
/// `add` enables URL download and tar auto-extraction. A source named like a heredoc is an inline file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CopySpec {
    pub srcs: Vec<String>,
    pub dst: String,
    pub from: Option<String>,
    pub chown: Option<String>,
    pub chmod: Option<String>,
    pub add: bool,
    pub heredocs: Vec<Heredoc>,
}

/// An inline file: `COPY <<EOF /dst`, or the script of `RUN <<EOF`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heredoc {
    pub name: String,
    /// Always newline-terminated; leading tabs already stripped for `<<-`.
    pub body: String,
    /// Unquoted delimiter: `$VAR` in the body is expanded (COPY only; the shell does it for RUN).
    pub expand: bool,
}

/// `RUN --mount=type=...`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// A persistent directory shared between builds.
    Cache { target: String, id: Option<String> },
    /// A directory (or file) of the build context, read-only unless `rw` (then writes are discarded).
    Bind { source: String, target: String, rw: bool },
    Tmpfs { target: String },
    /// `docker build --secret id=<id>,src=<file>`; mounted at `/run/secrets/<id>` unless `target` is set.
    Secret { id: String, target: Option<String>, required: bool },
}

/// A signal name (`SIGTERM`, `term`; real-time signals are not supported) or number, as `STOPSIGNAL` takes.
pub fn parse_signal(s: &str) -> Option<i32> {
    if let Ok(n) = s.trim().parse::<i32>() {
        return (1..=64).contains(&n).then_some(n);
    }
    let up = s.trim().to_ascii_uppercase();
    let name = up.strip_prefix("SIG").unwrap_or(&up);
    Some(match name {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "ABRT" => 6,
        "KILL" => 9,
        "USR1" => 10,
        "SEGV" => 11,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        "CHLD" => 17,
        "CONT" => 18,
        "STOP" => 19,
        "TSTP" => 20,
        "WINCH" => 28,
        _ => return None,
    })
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

/// One instruction as written: continuations joined, plus the heredoc bodies that follow it.
struct Logical {
    line: usize,
    text: String,
    heredocs: Vec<Heredoc>,
}

/// `<<EOF`, `<<-EOF`, `<<"EOF"` markers in an instruction line: (delimiter, strip leading tabs, expand).
/// `<<<` (here-string) and `<<` followed by a non-identifier (shell arithmetic) are not markers.
fn heredoc_markers(text: &str) -> Vec<(String, bool, bool)> {
    let c: Vec<char> = text.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i + 2 < c.len() {
        if c[i] != '<' || c[i + 1] != '<' || (i > 0 && c[i - 1] == '<') || c[i + 2] == '<' {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        let strip = c.get(j) == Some(&'-');
        if strip {
            j += 1;
        }
        let quote = c.get(j).copied().filter(|q| *q == '"' || *q == '\'');
        if quote.is_some() {
            j += 1;
        }
        let start = j;
        while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '_') {
            j += 1;
        }
        let ident_ok = c.get(start).is_some_and(|ch| ch.is_ascii_alphabetic() || *ch == '_');
        if ident_ok && quote.is_none_or(|q| c.get(j) == Some(&q)) {
            out.push((c[start..j].iter().collect(), strip, quote.is_none()));
        }
        i = j.max(i + 2);
    }
    out
}

/// Logical lines (continuations joined, comments and blanks dropped) with their starting line number.
/// `RUN`/`COPY`/`ADD` heredoc bodies are consumed from the lines after the instruction.
fn logical_lines(src: &str) -> Result<Vec<Logical>> {
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut out = vec![];
    let mut cur: Option<(usize, String)> = None;
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        i += 1;
        if t.starts_with('#') || t.is_empty() {
            continue; // comments and blank lines are skipped, even inside a continuation
        }
        let (n, mut text) = cur.take().unwrap_or((i, String::new()));
        if !text.is_empty() {
            text.push(' ');
        }
        if let Some(body) = t.strip_suffix('\\') {
            text.push_str(body.trim_end());
            cur = Some((n, text));
            continue;
        }
        text.push_str(t);
        let kw = text.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        let mut heredocs = vec![];
        if matches!(kw.as_str(), "RUN" | "COPY" | "ADD") {
            for (name, strip, expand) in heredoc_markers(&text) {
                let mut body = String::new();
                loop {
                    let Some(raw) = lines.get(i) else { return Err(at(n, format!("heredoc <<{name} is never terminated"))) };
                    i += 1;
                    let l = if strip { raw.trim_start_matches('\t') } else { raw };
                    if l == name {
                        break;
                    }
                    body.push_str(l);
                    body.push('\n');
                }
                heredocs.push(Heredoc { name, body, expand });
            }
        }
        out.push(Logical { line: n, text, heredocs });
    }
    if let Some((line, text)) = cur {
        out.push(Logical { line, text, heredocs: vec![] });
    }
    Ok(out)
}

/// Shell-like word splitting: whitespace separates, `'..'` and `".."` group, `\x` escapes.
/// `\$` is kept as-is so `subst` can tell it was escaped.
pub fn words(s: &str) -> Result<Vec<String>> {
    Ok(split_words(s)?.into_iter().map(|(w, _)| w).collect())
}

/// Like `words`, also giving the byte offset of each word's first *unquoted* `=` (so `"a b"="c d"` splits right).
fn split_words(s: &str) -> Result<Vec<(String, Option<usize>)>> {
    let mut out = vec![];
    let mut eq: Option<usize> = None;
    let mut cur = String::new();
    let mut started = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    out.push((std::mem::take(&mut cur), eq.take()));
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
                if c == '=' && eq.is_none() {
                    eq = Some(cur.len());
                }
                cur.push(c);
            }
        }
    }
    if started {
        out.push((cur, eq));
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

/// `K=V K2="v 2" "k 3"=v` pairs (ENV, LABEL). The key may be quoted.
fn pairs(rest: &str, line: usize) -> Result<Vec<(String, String)>> {
    split_words(rest)?
        .into_iter()
        .map(|(w, eq)| {
            eq.filter(|&i| i > 0)
                .map(|i| (w[..i].to_string(), w[i + 1..].to_string()))
                .ok_or_else(|| at(line, format!("expected KEY=VALUE, got {w:?}")))
        })
        .collect()
}

/// `LABEL k=v k2=v2`, or the legacy `LABEL key value with spaces`.
fn label_pairs(rest: &str, line: usize) -> Result<Vec<(String, String)>> {
    let first = split_words(rest)?.into_iter().next();
    if first.is_some_and(|(_, eq)| eq.is_some()) {
        return pairs(rest, line);
    }
    let (k, v) = rest.split_once(char::is_whitespace).ok_or_else(|| at(line, "LABEL needs KEY=VALUE"))?;
    Ok(vec![(words(k)?.concat(), words(v)?.join(" "))])
}

fn string_list(rest: &str) -> Result<Vec<String>> {
    if rest.trim_start().starts_with('[')
        && let Ok(v) = serde_json::from_str::<Vec<String>>(rest.trim())
    {
        return Ok(v);
    }
    words(rest)
}

/// Leading `--flag[=value]` words of an instruction: (name, value) pairs and the rest of the line.
fn leading_flags(rest: &str) -> (Vec<(String, String)>, &str) {
    let mut flags = vec![];
    let mut rest = rest.trim_start();
    while let Some(flag) = rest.strip_prefix("--") {
        let end = flag.find(char::is_whitespace).unwrap_or(flag.len());
        let (name, value) = flag[..end].split_once('=').unwrap_or((&flag[..end], ""));
        flags.push((name.to_string(), value.to_string()));
        rest = flag[end..].trim_start();
    }
    (flags, rest)
}

/// `COPY`/`ADD` flags: `--from` (COPY only), `--chown`, `--chmod` (octal), `--link` (accepted, no effect).
fn copy_flags(rest: &str, kw: &str, line: usize) -> Result<(CopySpec, String)> {
    let mut spec = CopySpec { add: kw == "ADD", ..CopySpec::default() };
    let (flags, rest) = leading_flags(rest);
    for (name, v) in flags {
        match (name.as_str(), v) {
            ("from", v) if !v.is_empty() && kw == "COPY" => spec.from = Some(v),
            ("chown", v) if !v.is_empty() => spec.chown = Some(v),
            ("chmod", v) if !v.is_empty() && v.len() <= 4 && v.bytes().all(|b| (b'0'..=b'7').contains(&b)) => spec.chmod = Some(v),
            ("link", _) => {}
            (name, _) => return Err(at(line, format!("{kw} --{name} is not supported (supported: --from=<stage|image>, --chown=<user[:group]>, --chmod=<octal>)"))),
        }
    }
    Ok((spec, rest.to_string()))
}

/// `--mount=type=cache,target=/x,id=y` (comma separated `key[=value]` list).
fn parse_mount(spec: &str, line: usize) -> Result<Mount> {
    let kv: Vec<(&str, &str)> = spec.split(',').filter(|s| !s.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).collect();
    let get = |names: &[&str]| kv.iter().find(|(k, _)| names.contains(k)).map(|(_, v)| v.to_string()).filter(|v| !v.is_empty());
    let target = get(&["target", "dst", "destination"]);
    let need_target = || target.clone().ok_or_else(|| at(line, "--mount needs target=<path>"));
    let ty = get(&["type"]).unwrap_or_else(|| "bind".into());
    Ok(match ty.as_str() {
        "cache" => Mount::Cache { target: need_target()?, id: get(&["id"]) },
        "tmpfs" => Mount::Tmpfs { target: need_target()? },
        "bind" => {
            if get(&["from"]).is_some() {
                return Err(at(line, "--mount=type=bind,from=... is not supported (bind mounts read the build context)"));
            }
            let rw = kv.iter().any(|(k, v)| matches!(*k, "rw" | "readwrite") && *v != "false");
            Mount::Bind { source: get(&["source", "src"]).unwrap_or_else(|| "/".into()), target: need_target()?, rw }
        }
        "secret" => {
            let id = get(&["id"]).ok_or_else(|| at(line, "--mount=type=secret needs id=<name>"))?;
            Mount::Secret { id, target, required: kv.iter().any(|(k, v)| *k == "required" && *v != "false") }
        }
        other => return Err(at(line, format!("--mount type={other} is not supported (cache, bind, tmpfs, secret)"))),
    })
}

fn heredoc_word(src: &str) -> Option<&str> {
    src.strip_prefix("<<").map(|w| w.trim_start_matches('-').trim_matches(['"', '\'']))
}

pub fn parse(src: &str) -> Result<Vec<Step>> {
    let mut steps = vec![];
    for Logical { line, text, heredocs } in logical_lines(src)? {
        let (kw, rest) = text.split_once(char::is_whitespace).map_or((text.as_str(), ""), |(k, r)| (k, r.trim()));
        let upper = kw.to_ascii_uppercase();
        if rest.is_empty() && upper != "ENTRYPOINT" {
            return Err(at(line, format!("{upper} needs an argument")));
        }
        let inst = match upper.as_str() {
            "FROM" => {
                let (flags, rest) = leading_flags(rest);
                let mut platform = None;
                for (name, v) in flags {
                    match name.as_str() {
                        "platform" if !v.is_empty() => platform = Some(v),
                        _ => return Err(at(line, format!("FROM --{name} is not supported (supported: --platform=<os/arch>)"))),
                    }
                }
                let w = words(rest)?;
                match w.as_slice() {
                    [img] => Instruction::From { image: img.clone(), alias: None, platform },
                    [img, as_, name] if as_.eq_ignore_ascii_case("as") => {
                        Instruction::From { image: img.clone(), alias: Some(name.clone()), platform }
                    }
                    _ => return Err(at(line, "expected FROM [--platform=<p>] <image> [AS <name>]")),
                }
            }
            "RUN" => {
                let (flags, rest) = leading_flags(rest);
                let mut mounts = vec![];
                for (name, v) in flags {
                    match name.as_str() {
                        "mount" => mounts.push(parse_mount(&v, line)?),
                        "network" | "security" => {} // the build network is the host's; accepted for compatibility
                        _ => return Err(at(line, format!("RUN --{name} is not supported (supported: --mount)"))),
                    }
                }
                let cmd = match heredocs.as_slice() {
                    [] => cmd(rest),
                    // `RUN <<EOF`: the body is the script itself
                    [h] if heredoc_word(rest.trim()).is_some() => Cmd::Shell(h.body.clone()),
                    // `RUN cat <<EOF > f`: the shell reads the bodies, which must follow the line in order
                    hs => {
                        let mut s = rest.to_string();
                        for h in hs {
                            s.push('\n');
                            s.push_str(&h.body);
                            s.push_str(&h.name);
                        }
                        Cmd::Shell(s)
                    }
                };
                Instruction::Run { cmd, mounts }
            }
            "CMD" => Instruction::Cmd(cmd(rest)),
            "ENTRYPOINT" => Instruction::Entrypoint(if rest.is_empty() { Cmd::Exec(vec![]) } else { cmd(rest) }),
            "COPY" | "ADD" => {
                let (mut spec, rest) = copy_flags(rest, &upper, line)?;
                let mut w = string_list(&rest)?;
                if w.len() < 2 {
                    return Err(at(line, format!("{upper} needs at least a source and a destination")));
                }
                spec.dst = w.pop().expect("len >= 2");
                for s in &mut w {
                    if let Some(name) = heredoc_word(s).map(str::to_string) {
                        let h = heredocs.iter().find(|h| h.name == name).ok_or_else(|| at(line, format!("heredoc <<{name} has no body")))?;
                        spec.heredocs.push(h.clone());
                        *s = name;
                    }
                }
                if spec.from.is_some() && !spec.heredocs.is_empty() {
                    return Err(at(line, "--from cannot be combined with a heredoc source"));
                }
                spec.srcs = w;
                Instruction::Copy(spec)
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
            "LABEL" => Instruction::Label(label_pairs(rest, line)?),
            "USER" => Instruction::User(rest.to_string()),
            "HEALTHCHECK" => Instruction::Healthcheck(healthcheck(rest, line)?),
            "VOLUME" => Instruction::Volume(string_list(rest)?),
            "STOPSIGNAL" => {
                if parse_signal(rest).is_none() {
                    return Err(at(line, format!("invalid signal {rest:?}")));
                }
                Instruction::StopSignal(rest.to_string())
            }
            "SHELL" => match serde_json::from_str::<Vec<String>>(rest) {
                Ok(v) if !v.is_empty() => Instruction::Shell(v),
                _ => return Err(at(line, "SHELL needs a JSON array, e.g. [\"/bin/bash\", \"-c\"]")),
            },
            "ONBUILD" => {
                let inner = parse(rest).map_err(|e| at(line, format!("ONBUILD: {e}")))?;
                match inner.as_slice() {
                    [s] if !matches!(s.inst, Instruction::From { .. } | Instruction::Onbuild(_)) => Instruction::Onbuild(rest.to_string()),
                    _ => return Err(at(line, "ONBUILD takes one instruction other than FROM or ONBUILD")),
                }
            }
            "MAINTAINER" => Instruction::Ignored(upper),
            other => return Err(at(line, format!("unknown instruction {other}"))),
        };
        // heredoc bodies are not part of the displayed text
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
        assert_eq!(steps[1].inst, Instruction::Run { cmd: Cmd::Shell("echo a && echo b".into()), mounts: vec![] });
        assert_eq!(steps[2].inst, Instruction::Cmd(Cmd::Exec(vec!["sh".into()])));
    }

    #[test]
    fn keywords_are_case_insensitive_and_from_alias() {
        assert_eq!(one("from alpine:3.19 as build"), Instruction::From { image: "alpine:3.19".into(), alias: Some("build".into()), platform: None });
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
        let cp = |srcs: &[&str], dst: &str| CopySpec { srcs: srcs.iter().map(|s| s.to_string()).collect(), dst: dst.into(), ..CopySpec::default() };
        assert_eq!(one("COPY a b /dst/"), Instruction::Copy(cp(&["a", "b"], "/dst/")));
        assert_eq!(one("COPY [\"a b\", \"/d\"]"), Instruction::Copy(cp(&["a b"], "/d")));
        assert_eq!(
            one("COPY --from=build --chown=1000:1000 --chmod=0755 --link /out/app /usr/bin/app"),
            Instruction::Copy(CopySpec {
                from: Some("build".into()),
                chown: Some("1000:1000".into()),
                chmod: Some("0755".into()),
                ..cp(&["/out/app"], "/usr/bin/app")
            })
        );
        assert_eq!(one("ADD --chown=u --chmod=644 f /f"), Instruction::Copy(CopySpec { chown: Some("u".into()), chmod: Some("644".into()), add: true, ..cp(&["f"], "/f") }));
        assert_eq!(one("ADD http://x/y.tgz a.tar /z/"), Instruction::Copy(CopySpec { add: true, ..cp(&["http://x/y.tgz", "a.tar"], "/z/") }));
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
        assert_eq!(one("RUN [not json"), Instruction::Run { cmd: Cmd::Shell("[not json".into()), mounts: vec![] });
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
    fn new_instructions() {
        assert_eq!(one("SHELL [\"/bin/bash\", \"-c\"]"), Instruction::Shell(vec!["/bin/bash".into(), "-c".into()]));
        assert_eq!(one("ONBUILD RUN echo hi"), Instruction::Onbuild("RUN echo hi".into()));
        assert_eq!(one("VOLUME [\"/a\", \"/b\"]"), Instruction::Volume(vec!["/a".into(), "/b".into()]));
        assert_eq!(one("VOLUME /a /b"), Instruction::Volume(vec!["/a".into(), "/b".into()]));
        assert_eq!(one("STOPSIGNAL SIGINT"), Instruction::StopSignal("SIGINT".into()));
        assert_eq!(
            one("FROM --platform=linux/amd64 alpine AS a"),
            Instruction::From { image: "alpine".into(), alias: Some("a".into()), platform: Some("linux/amd64".into()) }
        );
        assert_eq!(parse_signal("SIGTERM"), Some(15));
        assert_eq!(parse_signal("kill"), Some(9));
        assert_eq!(parse_signal("10"), Some(10));
        assert_eq!(parse_signal("NOPE"), None);
    }

    #[test]
    fn label_forms() {
        let l = |s: &str| match one(s) {
            Instruction::Label(p) => p,
            other => panic!("{other:?}"),
        };
        assert_eq!(l("LABEL a=b c=\"d e\""), [("a".into(), "b".into()), ("c".into(), "d e".into())]);
        assert_eq!(l("LABEL \"com.x y\"=\"v 1\""), [("com.x y".into(), "v 1".into())]);
        assert_eq!(l("LABEL legacy key with spaces"), [("legacy".into(), "key with spaces".into())]);
        assert_eq!(l("LABEL a=b=c"), [("a".into(), "b=c".into())]);
    }

    #[test]
    fn run_mounts() {
        let Instruction::Run { mounts, cmd } = one("RUN --mount=type=cache,target=/c,id=x --mount=type=secret,id=tok,required --mount=type=bind,source=src,target=/s,rw --mount=type=tmpfs,target=/t --network=none echo hi") else { panic!() };
        assert_eq!(cmd, Cmd::Shell("echo hi".into()));
        assert_eq!(
            mounts,
            [
                Mount::Cache { target: "/c".into(), id: Some("x".into()) },
                Mount::Secret { id: "tok".into(), target: None, required: true },
                Mount::Bind { source: "src".into(), target: "/s".into(), rw: true },
                Mount::Tmpfs { target: "/t".into() },
            ]
        );
    }

    #[test]
    fn heredocs() {
        let steps = parse("FROM a\nRUN <<EOF\nset -e\n# not a comment line\necho $X\nEOF\nRUN cat <<-'END' > /f\n\tkeep\n\tEND\nCOPY <<EOF /etc/conf\nk=$V\nEOF\nCMD x\n").unwrap();
        assert_eq!(steps.len(), 5);
        assert_eq!(steps[1].inst, Instruction::Run { cmd: Cmd::Shell("set -e\n# not a comment line\necho $X\n".into()), mounts: vec![] });
        assert_eq!(steps[2].inst, Instruction::Run { cmd: Cmd::Shell("cat <<-'END' > /f\nkeep\nEND".into()), mounts: vec![] });
        let Instruction::Copy(c) = &steps[3].inst else { panic!() };
        assert_eq!((c.srcs.as_slice(), c.dst.as_str()), (&["EOF".to_string()][..], "/etc/conf"));
        assert_eq!(c.heredocs, [Heredoc { name: "EOF".into(), body: "k=$V\n".into(), expand: true }]);
        assert!(parse("RUN <<EOF\nnever ends\n").is_err());
        // arithmetic and here-strings are not heredocs
        assert_eq!(one("RUN echo $((1<<2)) <<< x"), Instruction::Run { cmd: Cmd::Shell("echo $((1<<2)) <<< x".into()), mounts: vec![] });
    }

    #[test]
    fn rejects_what_it_cannot_do() {
        for bad in [
            "FROMM x", "RUN", "COPY a", "COPY --from= a /b", "COPY --chmod=abc a /b", "ADD --from=b a /b", "RUN --mount=type=nfs,target=/x y",
            "RUN --mount=type=cache y", "ONBUILD", "ONBUILD FROM x", "ONBUILD ONBUILD RUN x", "SHELL bash", "STOPSIGNAL NOPE", "FROM --bogus=1 a",
            "FROM a b c", "ENV A", "LABEL novalue",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(parse("ENTRYPOINT").is_ok()); // resets the entrypoint
    }
}
