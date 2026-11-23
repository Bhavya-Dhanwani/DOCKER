//! Go-template formatting for `--format`, the way `docker ps`, `docker images`, `docker inspect` and friends use it:
//! `{{.Names}}`, `{{.State.Running}}`, `{{json .Config.Env}}`, `{{join .Names ","}}`, `{{range .Mounts}}{{.Source}} {{end}}`,
//! `{{if .State.Running}}up{{else}}down{{end}}`, `{{printf "%-10s" .ID}}`, pipelines (`{{.Image | upper}}`) and the
//! `table ...` prefix. Values are JSON, so a command only has to produce Docker-shaped JSON for each row.
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Action(String),
    Range(String, Vec<Node>),
    If(String, Vec<Node>, Vec<Node>),
}

fn parse(src: &str) -> Result<Vec<Node>, String> {
    let (nodes, rest) = parse_block(src)?;
    match rest {
        None => Ok(nodes),
        Some(_) => Err("unexpected {{end}} or {{else}}".into()),
    }
}

/// Parse until `{{end}}`/`{{else}}`; returns the nodes and which keyword stopped it (with the remaining text).
/// What a block ended with: the keyword (`end`/`else`) and the text after it.
type Stop = Option<(String, String)>;

fn parse_block(mut src: &str) -> Result<(Vec<Node>, Stop), String> {
    let mut out = vec![];
    while let Some(i) = src.find("{{") {
        if i > 0 {
            out.push(Node::Text(src[..i].to_string()));
        }
        let rest = &src[i + 2..];
        let j = rest.find("}}").ok_or("unclosed {{")?;
        let action = rest[..j].trim().trim_start_matches('-').trim_end_matches('-').trim().to_string();
        src = &rest[j + 2..];
        if action == "end" || action == "else" {
            return Ok((out, Some((action, src.to_string()))));
        }
        if let Some(arg) = action.strip_prefix("range ") {
            let (body, stop) = parse_block(src)?;
            let (kw, after) = stop.ok_or("missing {{end}}")?;
            if kw != "end" {
                return Err("{{else}} in range is not supported".into());
            }
            out.push(Node::Range(arg.trim().to_string(), body));
            src = Box::leak(after.into_boxed_str());
        } else if let Some(arg) = action.strip_prefix("if ") {
            let (yes, stop) = parse_block(src)?;
            let (kw, after) = stop.ok_or("missing {{end}}")?;
            let after: &str = Box::leak(after.into_boxed_str());
            if kw == "else" {
                let (no, stop2) = parse_block(after)?;
                let (_, after2) = stop2.ok_or("missing {{end}}")?;
                out.push(Node::If(arg.trim().to_string(), yes, no));
                src = Box::leak(after2.into_boxed_str());
            } else {
                out.push(Node::If(arg.trim().to_string(), yes, vec![]));
                src = after;
            }
        } else {
            out.push(Node::Action(action));
        }
    }
    if !src.is_empty() {
        out.push(Node::Text(src.to_string()));
    }
    Ok((out, None))
}

/// Go's default text for a value.
pub fn show(v: &Value) -> String {
    match v {
        Value::Null => "<no value>".into(),
        Value::String(s) => s.clone(),
        Value::Array(a) => format!("[{}]", a.iter().map(show).collect::<Vec<_>>().join(" ")),
        Value::Object(m) => {
            let sorted: BTreeMap<_, _> = m.iter().collect();
            format!("map[{}]", sorted.iter().map(|(k, v)| format!("{k}:{}", show(v))).collect::<Vec<_>>().join(" "))
        }
        other => other.to_string(),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Split an action into words, keeping `"quoted strings"` together and `(parenthesised calls)` as one word.
fn words(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quoted) = (vec![], String::new(), 0, false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            '(' if !quoted => {
                depth += 1;
                cur.push(c);
            }
            ')' if !quoted => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn field(root: &Value, dot: &Value, path: &str) -> Value {
    let base = if path.starts_with("$.") || path == "$" { root } else { dot };
    let path = path.trim_start_matches('$');
    let mut cur = base;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        cur = match cur.get(part) {
            Some(v) => v,
            None => return Value::Null,
        };
    }
    cur.clone()
}

fn arg(root: &Value, dot: &Value, w: &str) -> Result<Value, String> {
    if let Some(s) = w.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Ok(Value::String(s.replace("\\n", "\n").replace("\\t", "\t").replace("\\\"", "\"")));
    }
    if w == "." {
        return Ok(dot.clone());
    }
    if w.starts_with('.') || w.starts_with('$') {
        return Ok(field(root, dot, w));
    }
    if let Some(inner) = w.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        return eval(root, dot, inner);
    }
    if w == "true" || w == "false" {
        return Ok(Value::Bool(w == "true"));
    }
    if let Ok(n) = w.parse::<i64>() {
        return Ok(Value::from(n));
    }
    Err(format!("unknown value {w:?}"))
}

fn sprintf(fmt: &str, args: &[Value]) -> String {
    let (mut out, mut it, mut chars) = (String::new(), args.iter(), fmt.chars().peekable());
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut spec = String::new();
        while let Some(&n) = chars.peek() {
            spec.push(n);
            chars.next();
            if n.is_ascii_alphabetic() || n == '%' {
                break;
            }
        }
        let verb = spec.chars().last().unwrap_or('v');
        if verb == '%' {
            out.push('%');
            continue;
        }
        let flags = &spec[..spec.len() - 1];
        let left = flags.starts_with('-');
        let width: usize = flags.trim_start_matches('-').split('.').next().and_then(|w| w.parse().ok()).unwrap_or(0);
        let v = it.next().map(|v| if verb == 'q' { format!("{:?}", show(v)) } else { show(v) }).unwrap_or_default();
        let pad = width.saturating_sub(v.chars().count());
        if left { out.push_str(&v); out.push_str(&" ".repeat(pad)); } else { out.push_str(&" ".repeat(pad)); out.push_str(&v); }
    }
    out
}

/// One pipeline: `json .Config`, `.Names | join ","`, `index .Labels "k"`.
fn eval(root: &Value, dot: &Value, expr: &str) -> Result<Value, String> {
    // a `|` inside a "quoted string" or (parentheses) is not a pipe
    let (mut parts, mut cur, mut depth, mut quoted) = (vec![], String::new(), 0, false);
    for c in expr.chars() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            '|' if !quoted && depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    let mut stages = parts.iter().map(|p| p.trim());
    let first = stages.next().unwrap_or("");
    let mut value = call(root, dot, &words(first), None)?;
    for stage in stages {
        value = call(root, dot, &words(stage), Some(value))?;
    }
    Ok(value)
}

fn call(root: &Value, dot: &Value, w: &[String], piped: Option<Value>) -> Result<Value, String> {
    let Some(name) = w.first() else { return Ok(Value::Null) };
    let mut args: Vec<Value> = w[1..].iter().map(|a| arg(root, dot, a)).collect::<Result<_, _>>()?;
    args.extend(piped);
    let s = |i: usize| args.get(i).map(show).unwrap_or_default();
    Ok(match name.as_str() {
        "json" => Value::String(serde_json::to_string(args.first().unwrap_or(&Value::Null)).map_err(|e| e.to_string())?),
        "join" => Value::String(args.first().and_then(Value::as_array).map(|a| a.iter().map(show).collect::<Vec<_>>().join(&s(1))).unwrap_or_default()),
        "index" => match (args.first(), args.get(1)) {
            (Some(Value::Object(m)), Some(k)) => m.get(&show(k)).cloned().unwrap_or(Value::Null),
            (Some(Value::Array(a)), Some(Value::Number(n))) => n.as_u64().and_then(|i| a.get(i as usize)).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "upper" | "ToUpper" => Value::String(s(0).to_uppercase()),
        "lower" | "ToLower" => Value::String(s(0).to_lowercase()),
        "title" | "Title" => Value::String(s(0).split(' ').map(|w| { let mut c = w.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() }).collect::<Vec<_>>().join(" ")),
        "len" => Value::from(match args.first() { Some(Value::Array(a)) => a.len(), Some(Value::Object(o)) => o.len(), Some(Value::String(t)) => t.chars().count(), _ => 0 }),
        "printf" => Value::String(sprintf(&s(0), &args[1.min(args.len())..])),
        "print" => Value::String(args.iter().map(show).collect::<Vec<_>>().concat()),
        "println" => Value::String(args.iter().map(show).collect::<Vec<_>>().join(" ") + "\n"),
        "eq" => Value::Bool(args.windows(2).all(|p| show(&p[0]) == show(&p[1]))),
        "ne" => Value::Bool(show(&args[0]) != show(&args[1])),
        "not" => Value::Bool(!truthy(args.first().unwrap_or(&Value::Null))),
        "and" => Value::Bool(args.iter().all(truthy)),
        "or" => Value::Bool(args.iter().any(truthy)),
        "split" => Value::Array(s(0).split(&s(1)).map(|p| Value::String(p.to_string())).collect()),
        "truncate" => { let n = args.get(1).and_then(Value::as_u64).unwrap_or(0) as usize; Value::String(s(0).chars().take(n).collect()) }
        _ if args.is_empty() && w.len() == 1 => arg(root, dot, name)?,
        other => return Err(format!("function {other:?} not defined")),
    })
}

fn render_nodes(nodes: &[Node], root: &Value, dot: &Value, out: &mut String) -> Result<(), String> {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Action(a) => out.push_str(&show(&eval(root, dot, a)?)),
            Node::Range(expr, body) => {
                let items = eval(root, dot, expr)?;
                let list: Vec<Value> = match items {
                    Value::Array(a) => a,
                    Value::Object(m) => m.into_iter().map(|(_, v)| v).collect(),
                    _ => vec![],
                };
                for item in &list {
                    render_nodes(body, root, item, out)?;
                }
            }
            Node::If(cond, yes, no) => {
                let v = eval(root, dot, cond)?;
                render_nodes(if truthy(&v) { yes } else { no }, root, dot, out)?;
            }
        }
    }
    Ok(())
}

/// Render `template` for one JSON value.
pub fn render(template: &str, value: &Value) -> Result<String, String> {
    let nodes = parse(&template.replace("\\n", "\n").replace("\\t", "\t"))?;
    let mut out = String::new();
    render_nodes(&nodes, value, value, &mut out)?;
    Ok(out)
}

/// Render every row: a plain template gives one line per row, `table ...` gives aligned columns with a header.
/// `headers` maps a field name to the column title (`ID` -> `CONTAINER ID`); other fields use their upper-cased name.
pub fn render_rows(template: &str, rows: &[Value], headers: &[(&str, &str)]) -> Result<String, String> {
    let Some(table) = template.trim_start().strip_prefix("table") else {
        return rows.iter().map(|r| render(template, r).map(|l| l + "\n")).collect();
    };
    let tpl = if table.trim().is_empty() { String::new() } else { table.trim_start().to_string() };
    // the title of each column comes from the field its cell shows
    let cells: Vec<&str> = tpl.split('\t').collect();
    let title = |cell: &str| -> String {
        let f = cell.trim().trim_start_matches("{{").trim_end_matches("}}").trim().rsplit(' ').next().unwrap_or("").trim_start_matches('.').to_string();
        headers.iter().find(|(k, _)| *k == f).map_or_else(|| f.to_uppercase(), |(_, t)| (*t).to_string())
    };
    let mut table_rows: Vec<Vec<String>> = vec![cells.iter().map(|c| title(c)).collect()];
    for r in rows {
        table_rows.push(cells.iter().map(|c| render(c, r)).collect::<Result<_, _>>()?);
    }
    let cols = cells.len();
    let widths: Vec<usize> = (0..cols).map(|c| table_rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for r in &table_rows {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| if i + 1 == cols { c.clone() } else { format!("{c:<w$}", w = widths[i] + 3) }).collect();
        out.push_str(line.concat().trim_end());
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn c() -> Value {
        json!({"ID": "abc123", "Names": "web", "State": {"Running": true, "Status": "running"}, "Config": {"Env": ["A=1", "B=2"]},
               "Labels": {"app": "x"}, "Mounts": [{"Source": "/a", "Destination": "/b"}, {"Source": "/c", "Destination": "/d"}],
               "Networks": {"bridge": {"IPAddress": "172.30.0.2"}}})
    }

    #[test]
    fn fields_functions_and_pipelines() {
        let v = c();
        assert_eq!(render("{{.ID}} {{.Names}}", &v).unwrap(), "abc123 web");
        assert_eq!(render("{{.State.Running}}", &v).unwrap(), "true");
        assert_eq!(render("{{json .Config.Env}}", &v).unwrap(), r#"["A=1","B=2"]"#);
        assert_eq!(render(r#"{{join .Config.Env ","}}"#, &v).unwrap(), "A=1,B=2");
        assert_eq!(render("{{.Names | upper}}", &v).unwrap(), "WEB");
        assert_eq!(render(r#"{{index .Labels "app"}}"#, &v).unwrap(), "x");
        assert_eq!(render(r#"{{printf "%-6s|" .Names}}"#, &v).unwrap(), "web   |");
        assert_eq!(render("{{len .Mounts}}", &v).unwrap(), "2");
        assert_eq!(render("a\\tb\\n", &v).unwrap(), "a\tb\n");
    }

    #[test]
    fn range_and_if() {
        let v = c();
        assert_eq!(render("{{range .Mounts}}{{.Source}}:{{.Destination}} {{end}}", &v).unwrap(), "/a:/b /c:/d ");
        assert_eq!(render("{{range .Networks}}{{.IPAddress}}{{end}}", &v).unwrap(), "172.30.0.2");
        assert_eq!(render("{{if .State.Running}}up{{else}}down{{end}}", &v).unwrap(), "up");
        assert_eq!(render("{{if .Nothing}}yes{{else}}no{{end}}", &v).unwrap(), "no");
        assert_eq!(render(r#"{{if eq .State.Status "running"}}R{{end}}"#, &v).unwrap(), "R");
    }

    #[test]
    fn table_aligns_columns_and_titles_them() {
        let rows = vec![json!({"ID": "a", "Image": "alpine"}), json!({"ID": "bbbb", "Image": "x"})];
        let out = render_rows("table {{.ID}}\t{{.Image}}", &rows, &[("ID", "CONTAINER ID")]).unwrap();
        assert_eq!(out, "CONTAINER ID   IMAGE\na              alpine\nbbbb           x\n");
        assert_eq!(render_rows("{{.ID}}", &rows, &[]).unwrap(), "a\nbbbb\n");
    }

    #[test]
    fn errors_are_reported() {
        assert!(render("{{nope .X}}", &c()).is_err());
        assert!(render("{{.A", &c()).is_err());
        assert!(render("{{range .Mounts}}x", &c()).is_err());
    }
}
