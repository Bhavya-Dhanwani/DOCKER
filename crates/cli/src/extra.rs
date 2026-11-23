//! Commands that are mostly flags and formatting: `ps`, `images`, `inspect`, `logs`, `exec` with Docker's options and
//! `--format`, plus `info`, `events`, `create`, `diff`, `commit`, `export`, `import`, `port`, `attach`, `system prune`,
//! `network prune`, `builder prune`, `context` and `version --format`. Most of them reuse the Engine API handlers
//! in-process (`api::call`), so the CLI and the daemon behave the same.
use crate::{api, commands, format};
use docker_core::registry::{self, Entry};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    thread,
    time::Duration,
};

type Res<T = ()> = Result<T, Box<dyn Error>>;

/// Which command these arguments are for, and its own arguments; None when the main parser handles them.
fn route(args: &[String]) -> Option<(&'static str, Vec<String>)> {
    let first = args.first()?.as_str();
    let sub = args.get(1).map(String::as_str);
    let rest = |n: usize| args[n.min(args.len())..].to_vec();
    Some(match (first, sub) {
        ("ps", _) => ("ps", rest(1)),
        ("images", _) => ("images", rest(1)),
        ("logs", _) => ("logs", rest(1)),
        ("inspect", _) => ("inspect", rest(1)),
        ("exec", _) => ("exec", rest(1)),
        ("stop", _) => ("stop", rest(1)),
        ("kill", _) => ("kill", rest(1)),
        ("info", _) => ("info", rest(1)),
        ("events", _) => ("events", rest(1)),
        ("create", _) => ("create", rest(1)),
        ("diff", _) => ("diff", rest(1)),
        ("commit", _) => ("commit", rest(1)),
        ("export", _) => ("export", rest(1)),
        ("import", _) => ("import", rest(1)),
        ("port", _) => ("port", rest(1)),
        ("attach", _) => ("attach", rest(1)),
        ("context", _) => ("context", rest(1)),
        ("manifest", Some("inspect")) => ("manifest-inspect", rest(2)),
        ("version", _) if args.len() > 1 => ("version", rest(1)),
        ("container", Some("ls" | "list" | "ps")) => ("ps", rest(2)),
        ("container", Some("logs")) => ("logs", rest(2)),
        ("container", Some("inspect")) => ("inspect", rest(2)),
        ("container", Some("exec")) => ("exec", rest(2)),
        ("container", Some("stop")) => ("stop", rest(2)),
        ("container", Some("kill")) => ("kill", rest(2)),
        ("container", Some("create")) => ("create", rest(2)),
        ("container", Some("diff")) => ("diff", rest(2)),
        ("container", Some("commit")) => ("commit", rest(2)),
        ("container", Some("export")) => ("export", rest(2)),
        ("container", Some("port")) => ("port", rest(2)),
        ("container", Some("attach")) => ("attach", rest(2)),
        ("image", Some("ls" | "list")) => ("images", rest(2)),
        ("image", Some("import")) => ("import", rest(2)),
        ("image", Some("inspect")) => ("inspect", [vec!["--type=image".to_string()], rest(2)].concat()),
        ("system", Some("info")) => ("info", rest(2)),
        ("system", Some("events")) => ("events", rest(2)),
        ("system", Some("prune")) => ("system-prune", rest(2)),
        ("network", Some("prune")) => ("network-prune", rest(2)),
        ("builder", Some("prune")) => ("builder-prune", rest(2)),
        _ => return None,
    })
}

pub fn handles(args: &[String]) -> bool {
    route(args).is_some()
}

pub fn run(args: Vec<String>) -> Res<i32> {
    let (verb, rest) = route(&args).ok_or("unknown command")?;
    match verb {
        "ps" => ps(&rest),
        "images" => images(&rest),
        "logs" => logs(&rest),
        "inspect" => inspect(&rest),
        "exec" => exec(&rest),
        "stop" => stop(&rest),
        "kill" => kill(&rest),
        "info" => info(&rest),
        "events" => events(&rest),
        "create" => create(&rest),
        "diff" => diff(&rest),
        "commit" => commit(&rest),
        "export" => export(&rest),
        "import" => import(&rest),
        "port" => port(&rest),
        "attach" => attach(&rest),
        "context" => context(&rest),
        "manifest-inspect" => manifest_inspect(&rest),
        "version" => version(&rest),
        "system-prune" => system_prune(&rest),
        "network-prune" => network_prune(&rest).map(|()| 0),
        "builder-prune" => builder_prune(&rest),
        _ => Err("unknown command".into()),
    }
}

// ---- flags

#[derive(Default)]
pub(crate) struct Flags {
    pub(crate) on: HashSet<String>,
    pub(crate) vals: HashMap<String, Vec<String>>,
    pub(crate) pos: Vec<String>,
}

impl Flags {
    pub(crate) fn has(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.on.contains(*n))
    }
    pub(crate) fn get(&self, names: &[&str]) -> Option<&String> {
        names.iter().find_map(|n| self.vals.get(*n).and_then(|v| v.last()))
    }
    pub(crate) fn all(&self, names: &[&str]) -> Vec<String> {
        names.iter().flat_map(|n| self.vals.get(*n).cloned().unwrap_or_default()).collect()
    }
}

/// `bools`: flags without a value (`-a`, `--all`; clusters like `-aq` work), `vals`: flags with one (`-n 5`, `--format x`,
/// `--format=x`). With `stop`, the first positional ends the flags (`docker exec ctr cmd -x`).
pub(crate) fn flags(args: &[String], bools: &[&str], vals: &[&str], stop: bool) -> Res<Flags> {
    let mut f = Flags::default();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--" {
            f.pos.extend(it.by_ref().cloned());
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            if vals.contains(&name) {
                let v = inline.or_else(|| it.next().cloned()).ok_or_else(|| format!("flag needs an argument: --{name}"))?;
                f.vals.entry(name.into()).or_default().push(v);
            } else if bools.contains(&name) {
                f.on.insert(name.into());
            } else {
                return Err(format!("unknown flag: --{name}").into());
            }
        } else if a.len() > 1 && a.starts_with('-') && !a[1..].starts_with(|c: char| c.is_ascii_digit()) {
            let shorts: Vec<char> = a[1..].chars().collect();
            let mut i = 0;
            while i < shorts.len() {
                let name = shorts[i].to_string();
                if vals.contains(&name.as_str()) {
                    let tail: String = shorts[i + 1..].iter().collect();
                    let v = if tail.is_empty() { it.next().cloned().ok_or_else(|| format!("flag needs an argument: -{name}"))? } else { tail };
                    f.vals.entry(name).or_default().push(v);
                    break;
                } else if bools.contains(&name.as_str()) {
                    f.on.insert(name);
                } else {
                    return Err(format!("unknown shorthand flag: '{}' in {a}", shorts[i]).into());
                }
                i += 1;
            }
        } else {
            f.pos.push(a.clone());
            if stop {
                f.pos.extend(it.by_ref().cloned());
                break;
            }
        }
    }
    Ok(f)
}

// ---- Docker-style text

/// 7.8MB, 124MB, 1.18GB: decimal units with three significant digits, like Docker.
pub fn human_size(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let (mut v, mut i) = (bytes as f64, 0);
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes}B")
    } else if v >= 100.0 {
        format!("{v:.0}{}", U[i])
    } else if v >= 10.0 {
        format!("{v:.1}{}", U[i])
    } else {
        format!("{v:.2}{}", U[i])
    }
}

/// "3 minutes", "About an hour", "2 days": Docker's wording for a span.
pub fn human_duration(secs: u64) -> String {
    let (min, hour, day) = (secs / 60, secs / 3600, secs / 86400);
    match secs {
        0 => "Less than a second".into(),
        1 => "1 second".into(),
        s if s < 60 => format!("{s} seconds"),
        _ if min == 1 => "About a minute".into(),
        _ if min < 60 => format!("{min} minutes"),
        _ if hour == 1 => "About an hour".into(),
        _ if hour < 48 => format!("{hour} hours"),
        _ if day < 14 => format!("{day} days"),
        _ if day < 60 => format!("{} weeks", day / 7),
        _ if day < 730 => format!("{} months", day / 30),
        _ => format!("{} years", day / 365),
    }
}

/// `2026-11-23 01:02:03 +0000 UTC`
fn created_at(secs: u64) -> String {
    let t = api::rfc3339(secs);
    format!("{} {} +0000 UTC", &t[..10], &t[11..19])
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

pub(crate) fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

pub(crate) fn print_rows(template: &str, rows: &[Value], headers: &[(&str, &str)]) -> Res {
    print!("{}", format::render_rows(template, rows, headers)?);
    Ok(())
}

// ---- ps

fn port_text(e: &Entry) -> String {
    e.ports
        .iter()
        .filter_map(|p| {
            let (h, c) = p.split_once("->")?;
            let (c, proto) = c.split_once('/').unwrap_or((c, "tcp"));
            let (ip, h) = h.rsplit_once(':').unwrap_or(("0.0.0.0", h));
            Some(format!("{ip}:{h}->{c}/{proto}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn container_row(e: &Entry, now: u64, full_id: bool) -> Value {
    let state = api::state_of(e);
    let cmd = api::effective_cmd(e).join(" ");
    let c = e.config.clone().unwrap_or_default();
    let status = match state {
        "running" => format!("Up {}{}", human_duration(now.saturating_sub(e.started_unix)).to_lowercase_first(), e.health.as_deref().map(|h| format!(" ({h})")).unwrap_or_default()),
        "created" => "Created".into(),
        "restarting" => format!("Restarting ({}) {} ago", e.restarts, human_duration(now.saturating_sub(e.started_unix)).to_lowercase_first()),
        _ => format!("Exited ({}) {} ago", e.exit_code.unwrap_or(-1), human_duration(now.saturating_sub(e.finished_unix.unwrap_or(now))).to_lowercase_first()),
    };
    json!({
        "ID": if full_id { e.id.clone() } else { short(&e.id) }, "Image": e.image, "Command": format!("\"{}\"", clip(&cmd, 20)),
        "CreatedAt": created_at(e.started_unix), "RunningFor": format!("{} ago", human_duration(now.saturating_sub(e.started_unix)).to_lowercase_first()),
        "Ports": port_text(e), "State": state, "Status": status, "Names": e.name.clone().unwrap_or_else(|| short(&e.id)),
        "Labels": api::labels_of(&e.id).iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(","),
        "Mounts": c.volumes.iter().map(|v| v.container.clone()).collect::<Vec<_>>().join(","),
        "Networks": c.network_name.clone().unwrap_or_else(|| "bridge".into()), "Size": "0B", "LocalVolumes": c.volumes.iter().filter(|v| v.is_named()).count().to_string(),
        // not part of Docker's table, but the desktop app reads them
        "Id": e.id, "FullCommand": cmd, "IP": e.ip.clone().unwrap_or_default(), "Restarts": e.restarts, "UptimeSeconds": now.saturating_sub(e.started_unix),
        "ExitCode": e.exit_code, "Health": e.health, "PortsRaw": e.ports.join(","),
    })
}

trait LowerFirst {
    fn to_lowercase_first(&self) -> String;
}
impl LowerFirst for String {
    /// "About an hour" -> "About an hour" (Docker keeps that), "3 Minutes" never occurs: only "Less than a second" needs care
    fn to_lowercase_first(&self) -> String {
        if self.starts_with("Less") { self.to_lowercase() } else { self.clone() }
    }
}

/// `--filter name=web` (substring/regex-free), `status=running`, `id=ab`, `ancestor=alpine`, `label=k[=v]`, `network=n`, `exited=0`.
fn matches_filters(e: &Entry, filters: &[(String, String)]) -> bool {
    filters.iter().all(|(k, v)| match k.as_str() {
        "name" => e.name.as_deref().is_some_and(|n| n.contains(v.trim_start_matches('^').trim_end_matches('$'))),
        "id" => e.id.starts_with(v.as_str()),
        "status" => api::state_of(e) == v,
        "exited" => e.exit_code.is_some_and(|c| c.to_string() == *v),
        "ancestor" => e.image == *v || e.image.starts_with(&format!("{v}:")),
        "network" => e.config.as_ref().and_then(|c| c.network_name.clone()).unwrap_or_else(|| "bridge".into()) == *v,
        "label" => {
            let labels = api::labels_of(&e.id);
            match v.split_once('=') {
                Some((lk, lv)) => labels.get(lk).is_some_and(|x| x == lv),
                None => labels.contains_key(v),
            }
        }
        "volume" => e.config.as_ref().is_some_and(|c| c.volumes.iter().any(|x| x.host == *v || x.container == *v)),
        _ => true,
    })
}

pub(crate) fn parse_filters(list: &[String]) -> Vec<(String, String)> {
    list.iter().filter_map(|f| f.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect()
}

pub(crate) const PS_HEADERS: &[(&str, &str)] = &[("ID", "CONTAINER ID"), ("RunningFor", "CREATED"), ("CreatedAt", "CREATED")];
const PS_TABLE: &str = "table {{.ID}}\t{{.Image}}\t{{.Command}}\t{{.RunningFor}}\t{{.Status}}\t{{.Ports}}\t{{.Names}}";

fn ps(args: &[String]) -> Res<i32> {
    let f = flags(args, &["a", "all", "q", "quiet", "no-trunc", "l", "latest", "s", "size"], &["n", "last", "f", "filter", "format"], false)?;
    let filters = parse_filters(&f.all(&["f", "filter"]));
    let all = f.has(&["a", "all"]) || filters.iter().any(|(k, _)| k == "status" || k == "exited");
    let now = registry::now_unix();
    let mut list: Vec<Entry> = registry::list().into_iter().filter(|e| (all || e.running() || (e.restarting && e.exit_code.is_none())) && matches_filters(e, &filters)).collect();
    list.reverse(); // newest first, like Docker
    if f.has(&["l", "latest"]) {
        list.truncate(1);
    } else if let Some(n) = f.get(&["n", "last"]).and_then(|n| n.parse::<i64>().ok()).filter(|n| *n >= 0) {
        list.truncate(n as usize);
    }
    let full = f.has(&["no-trunc"]);
    if f.has(&["q", "quiet"]) {
        for e in &list {
            println!("{}", if full { e.id.clone() } else { short(&e.id) });
        }
        return Ok(0);
    }
    let rows: Vec<Value> = list.iter().map(|e| container_row(e, now, full)).collect();
    let template = f.get(&["format"]).cloned().unwrap_or_else(|| PS_TABLE.into());
    let template = if template == "json" { "{{json .}}".to_string() } else { template };
    print_rows(&template, &rows, PS_HEADERS)?;
    Ok(0)
}

// ---- images

pub(crate) fn split_repo_tag(name: &str) -> (String, String) {
    if let Some((r, _)) = name.split_once('@') {
        return (r.to_string(), "<none>".to_string()); // a pull by digest: the digest has its own column
    }
    match name.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), tag.to_string()),
        _ => (name.to_string(), "latest".to_string()),
    }
}

fn image_rows(full_id: bool) -> Vec<Value> {
    let store = api::store();
    let now = registry::now_unix();
    store
        .list()
        .iter()
        .map(|m| {
            let (repo, tag) = split_repo_tag(&m.name);
            let created = docker_image::Reference::parse(&m.name)
                .ok()
                .and_then(|r| fs::metadata(store.root().join("images").join(format!("{}.json", r.key()))).ok())
                .and_then(|md| md.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(now, |d| d.as_secs());
            let id = api::fake_id(&m.name);
            json!({
                "Repository": repo, "Tag": tag, "ID": if full_id { format!("sha256:{id}") } else { short(&id) },
                "CreatedAt": created_at(created), "CreatedSince": format!("{} ago", human_duration(now.saturating_sub(created))),
                "Size": human_size(m.size()), "Digest": m.name.split_once('@').map_or("<none>", |(_, d)| d), "Containers": "N/A", "SharedSize": "N/A", "UniqueSize": "N/A", "VirtualSize": human_size(m.size()),
                "Name": m.name, "Layers": m.layers.len(), "SizeBytes": m.size(),
            })
        })
        .collect()
}

fn images(args: &[String]) -> Res<i32> {
    let f = flags(args, &["a", "all", "q", "quiet", "no-trunc", "digests"], &["f", "filter", "format"], false)?;
    let filters = parse_filters(&f.all(&["f", "filter"]));
    let mut rows = image_rows(f.has(&["no-trunc"]));
    rows.retain(|r| {
        filters.iter().all(|(k, v)| match k.as_str() {
            "reference" => {
                let name = format!("{}:{}", r["Repository"].as_str().unwrap_or(""), r["Tag"].as_str().unwrap_or(""));
                api::name_matches(v, &name) || api::name_matches(v, r["Repository"].as_str().unwrap_or(""))
            }
            "dangling" => v == "false",
            _ => true,
        })
    });
    if let Some(pos) = f.pos.first() {
        let want = split_repo_tag(pos);
        rows.retain(|r| r["Repository"] == want.0 && (pos.contains(':') && !pos.ends_with(':') && want.1 != "latest" || !pos.contains(':') || r["Tag"] == want.1));
    }
    if f.has(&["q", "quiet"]) {
        for r in &rows {
            println!("{}", r["ID"].as_str().unwrap_or(""));
        }
        return Ok(0);
    }
    let table = if f.has(&["digests"]) { "table {{.Repository}}\t{{.Tag}}\t{{.Digest}}\t{{.ID}}\t{{.CreatedSince}}\t{{.Size}}" } else { "table {{.Repository}}\t{{.Tag}}\t{{.ID}}\t{{.CreatedSince}}\t{{.Size}}" };
    let template = f.get(&["format"]).cloned().unwrap_or_else(|| table.into());
    let template = if template == "json" { "{{json .}}".to_string() } else { template };
    print_rows(&template, &rows, &[("ID", "IMAGE ID"), ("CreatedSince", "CREATED"), ("CreatedAt", "CREATED")])?;
    Ok(0)
}

// ---- inspect

fn network_json(name: &str) -> Option<Value> {
    if name == "bridge" {
        return Some(json!({ "Name": "bridge", "Id": api::fake_id("bridge"), "Driver": "bridge", "Scope": "local", "Containers": {} }));
    }
    docker_core::networks::exists(name).then(|| {
        let members = docker_core::networks::members(Some(name), "");
        let containers: serde_json::Map<String, Value> = members.iter().map(|e| (e.id.clone(), json!({ "Name": e.label(), "IPv4Address": e.ip.clone().unwrap_or_default() }))).collect();
        let s = docker_core::networks::subnet_of(Some(name));
        let subnet = format!("{}/{}", s.base, s.prefix);
        json!({ "Name": name, "Id": api::fake_id(&format!("net-{name}")), "Driver": "bridge", "Scope": "local", "Containers": containers,
                "IPAM": { "Driver": "default", "Config": [{ "Subnet": subnet }] } })
    })
}

fn inspect(args: &[String]) -> Res<i32> {
    let f = flags(args, &["s", "size"], &["f", "format", "type"], false)?;
    let want = f.get(&["type"]).map(String::as_str);
    let ok = |t: &str| want.is_none_or(|w| w == t);
    let (mut found, mut missing) = (vec![], vec![]);
    for q in &f.pos {
        let v = (ok("container").then(|| registry::find(q).ok().map(|e| api::container_inspect(&e))).flatten())
            .or_else(|| ok("image").then(|| {
                let name = api::image_by_id(q, &api::image_names());
                docker_image::Reference::parse(&name).ok().and_then(|r| docker_image::inspect(&api::store(), &r).ok())
            }).flatten())
            .or_else(|| ok("volume").then(|| docker_core::volumes::exists(q).then(|| json!({ "Name": q, "Driver": "local", "Mountpoint": docker_core::volumes::path(q).to_string_lossy(), "Scope": "local", "Labels": {} }))).flatten())
            .or_else(|| ok("network").then(|| network_json(q)).flatten());
        match v {
            Some(v) => found.push(v),
            None => missing.push(q.clone()),
        }
    }
    if f.pos.is_empty() {
        return Err("\"docker inspect\" requires at least 1 argument".into());
    }
    match f.get(&["f", "format"]) {
        Some(t) => print_rows(t, &found, &[])?,
        None => println!("{}", serde_json::to_string_pretty(&found)?),
    }
    for m in &missing {
        eprintln!("Error: No such object: {m}");
    }
    Ok(i32::from(!missing.is_empty()))
}

// ---- logs

/// Byte offset where the last `n` lines of the file begin.
pub(crate) fn tail_offset(f: &mut fs::File, n: usize) -> std::io::Result<u64> {
    let len = f.metadata()?.len();
    if n == 0 {
        return Ok(len);
    }
    let (mut pos, mut lines, mut chunk) = (len, 0usize, [0u8; 8192]);
    while pos > 0 {
        let step = pos.min(chunk.len() as u64);
        pos -= step;
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut chunk[..step as usize])?;
        for i in (0..step as usize).rev() {
            // a trailing newline ends the last line; it is not the start of a new one
            if chunk[i] == b'\n' && pos + i as u64 + 1 < len {
                lines += 1;
                if lines == n {
                    return Ok(pos + i as u64 + 1);
                }
            }
        }
    }
    Ok(0)
}

fn logs(args: &[String]) -> Res<i32> {
    let f = flags(args, &["f", "follow", "t", "timestamps", "details"], &["n", "tail", "since", "until"], false)?;
    let q = f.pos.first().ok_or("\"docker logs\" requires exactly 1 argument")?;
    let e = registry::find(q)?;
    if f.has(&["t", "timestamps"]) || f.get(&["since"]).is_some() || f.get(&["until"]).is_some() {
        eprintln!("warning: this engine does not timestamp container output yet: -t, --since and --until are ignored");
    }
    let mut file = fs::File::open(registry::log_path(&e.id)).map_err(|_| format!("no logs for {} (only detached containers keep logs)", e.label()))?;
    if let Some(t) = f.get(&["n", "tail"]).filter(|t| t.as_str() != "all") {
        let n: usize = t.parse().map_err(|_| format!("invalid value for --tail: {t}"))?;
        let at = tail_offset(&mut file, n)?;
        file.seek(SeekFrom::Start(at))?;
    }
    let follow = f.has(&["f", "follow"]);
    let mut out = std::io::stdout();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n > 0 {
            out.write_all(&buf[..n])?;
            out.flush()?;
            continue;
        }
        if !follow || !registry::find(&e.id).is_ok_and(|x| x.running()) {
            let pos = file.stream_position()?;
            if file.metadata()?.len() > pos {
                file.seek(SeekFrom::Start(pos))?;
                continue;
            }
            return Ok(0);
        }
        thread::sleep(Duration::from_millis(150));
    }
}

// ---- exec

pub(crate) fn exec(args: &[String]) -> Res<i32> {
    let f = flags(args, &["i", "interactive", "t", "tty", "d", "detach", "privileged"], &["e", "env", "w", "workdir", "u", "user", "env-file", "detach-keys"], true)?;
    let q = f.pos.first().ok_or("\"docker exec\" requires at least 2 arguments")?;
    let cmd = &f.pos[1..];
    if cmd.is_empty() {
        return Err("\"docker exec\" requires at least 2 arguments".into());
    }
    let e = registry::find(q)?;
    if !e.running() {
        return Err(format!("container {} is not running", e.label()).into());
    }
    let mut e2 = e.clone();
    for file in f.all(&["env-file"]) {
        e2.env.extend(docker_core::env::parse_env_file(&fs::read_to_string(&file).map_err(|er| format!("--env-file {file}: {er}"))?));
    }
    for kv in f.all(&["e", "env"]) {
        let kv = if kv.contains('=') { kv } else { std::env::var(&kv).map(|v| format!("{kv}={v}")).unwrap_or_default() };
        if let Some((k, _)) = kv.split_once('=') {
            e2.env.retain(|x| !x.starts_with(&format!("{k}=")));
            e2.env.push(kv);
        }
    }
    if let Some(w) = f.get(&["w", "workdir"]) {
        e2.workdir = w.clone();
    }
    if let Some(u) = f.get(&["u", "user"]) {
        e2.user = u.clone();
    }
    if f.has(&["d", "detach"]) {
        // run it in the background: this binary again, without -d, with nothing attached
        let exe = std::env::current_exe()?;
        let mut c = std::process::Command::new(exe);
        c.arg("exec").args(args.iter().filter(|a| *a != "-d" && *a != "--detach")).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        c.spawn()?;
        return Ok(0);
    }
    Ok(docker_core::platform::exec(&e2, cmd, f.has(&["t", "tty"]))?)
}

// ---- stop / kill with a signal

fn signal_of(s: &str) -> Res<i32> {
    docker_image::dockerfile::parse_signal(s).ok_or_else(|| format!("invalid signal: {s}").into())
}

fn stop(args: &[String]) -> Res<i32> {
    let f = flags(args, &[], &["t", "time", "s", "signal"], false)?;
    let q = f.pos.first().ok_or("\"docker stop\" requires at least 1 argument")?;
    let secs = f.get(&["t", "time"]).map(|t| t.parse::<u64>()).transpose().map_err(|_| "invalid value for -t (seconds)")?.unwrap_or(10);
    let sig = f.get(&["s", "signal"]).map(|s| signal_of(s)).transpose()?;
    commands::stop_with(q, secs, sig)?;
    Ok(0)
}

fn kill(args: &[String]) -> Res<i32> {
    let f = flags(args, &[], &["s", "signal"], false)?;
    let q = f.pos.first().ok_or("\"docker kill\" requires at least 1 argument")?;
    match f.get(&["s", "signal"]) {
        None => commands::kill(q)?,
        Some(s) => {
            let e = registry::find(q)?;
            if !e.running() {
                return Err(format!("container {} is not running", e.label()).into());
            }
            docker_core::platform::signal(&e, signal_of(s)?)?;
            println!("{}", e.label());
        }
    }
    Ok(0)
}

// ---- API-backed commands

fn api_json(method: &str, target: &str, body: Vec<u8>) -> Res<Value> {
    let mut sink = std::io::sink();
    let (code, v) = api::call(method, target, body, &mut sink)?;
    if code >= 400 {
        let msg = v.as_ref().and_then(|v| v["message"].as_str()).unwrap_or("request failed").to_string();
        return Err(msg.into());
    }
    Ok(v.unwrap_or(Value::Null))
}

fn info(args: &[String]) -> Res<i32> {
    let f = flags(args, &[], &["f", "format"], false)?;
    let i = api::info();
    if let Some(t) = f.get(&["f", "format"]) {
        print_rows(t, &[i], &[])?;
        return Ok(0);
    }
    let n = |k: &str| i[k].as_u64().unwrap_or(0);
    let s = |k: &str| i[k].as_str().unwrap_or("").to_string();
    println!("Client:\n Version:    {}\n Context:    default\n", env!("CARGO_PKG_VERSION"));
    println!("Server:");
    println!(" Containers: {}\n  Running: {}\n  Paused: 0\n  Stopped: {}", n("Containers"), n("ContainersRunning"), n("ContainersStopped"));
    println!(" Images: {}\n Server Version: {}\n Storage Driver: {}", n("Images"), s("ServerVersion"), s("Driver"));
    println!(" Cgroup Driver: cgroupfs\n Cgroup Version: 2\n Runtimes: sasta\n Default Runtime: sasta\n Security Options:\n  seccomp\n   Profile: builtin");
    println!(" Kernel Version: {}", std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim());
    println!(" Operating System: {}\n OSType: linux\n Architecture: {}\n CPUs: {}\n Total Memory: {}", s("OperatingSystem"), s("Architecture"), n("NCPU"), human_size(n("MemTotal")));
    println!(" Name: {}\n Docker Root Dir: {}", s("Name"), s("DockerRootDir"));
    Ok(0)
}

fn version(args: &[String]) -> Res<i32> {
    let f = flags(args, &[], &["f", "format"], false)?;
    let v = json!({
        "Client": { "Version": env!("CARGO_PKG_VERSION"), "ApiVersion": "1.43", "Os": std::env::consts::OS, "Arch": std::env::consts::ARCH, "GoVersion": "n/a (rust)" },
        "Server": { "Version": env!("CARGO_PKG_VERSION"), "ApiVersion": "1.43", "Os": "linux", "Arch": std::env::consts::ARCH, "Engine": { "Version": env!("CARGO_PKG_VERSION") } },
    });
    match f.get(&["f", "format"]) {
        Some(t) => print_rows(t, &[v], &[])?,
        None => println!("{}", serde_json::to_string_pretty(&v)?),
    }
    Ok(0)
}

/// One line of `docker events`.
fn event_line(ev: &Value, fmt: Option<&str>) -> Res<String> {
    if let Some(t) = fmt {
        return Ok(if t == "json" { ev.to_string() } else { format::render(t, ev)? });
    }
    let nano = ev["timeNano"].as_u64().unwrap_or_else(|| ev["time"].as_u64().unwrap_or(0) * 1_000_000_000);
    let t = api::rfc3339(nano / 1_000_000_000);
    let attrs = ev["Actor"]["Attributes"].as_object().map(|m| m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    Ok(format!("{}.{:09}Z {} {} {} ({attrs})", &t[..19], nano % 1_000_000_000, ev["Type"].as_str().unwrap_or(""), ev["Action"].as_str().unwrap_or(""), ev["Actor"]["ID"].as_str().unwrap_or("")))
}

struct EventSink<'a> {
    pending: Vec<u8>,
    fmt: Option<&'a str>,
}

impl Write for EventSink<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(data);
        while let Some(nl) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=nl).collect();
            if let Ok(ev) = serde_json::from_slice::<Value>(&line) {
                match event_line(&ev, self.fmt) {
                    Ok(l) => {
                        println!("{l}");
                        let _ = std::io::stdout().flush();
                    }
                    Err(e) => return Err(std::io::Error::other(e.to_string())),
                }
            }
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn events(args: &[String]) -> Res<i32> {
    let f = flags(args, &[], &["since", "until", "f", "filter", "format"], false)?;
    let mut filters = serde_json::Map::new();
    for (k, v) in parse_filters(&f.all(&["f", "filter"])) {
        if let Some(a) = filters.entry(k).or_insert_with(|| json!([])).as_array_mut() {
            a.push(json!(v));
        }
    }
    let mut target = String::from("/events?");
    for k in ["since", "until"] {
        if let Some(v) = f.get(&[k]) {
            target += &format!("{k}={v}&");
        }
    }
    if !filters.is_empty() {
        target += &format!("filters={}", Value::Object(filters));
    }
    let mut sink = EventSink { pending: vec![], fmt: f.get(&["format"]).map(String::as_str) };
    api::call("GET", &target, vec![], &mut sink)?;
    Ok(0)
}

fn create(args: &[String]) -> Res<i32> {
    let run_args: Vec<String> = std::iter::once("run".to_string()).chain(args.iter().cloned()).collect();
    let docker_core::cli::Command::Run(mut cfg) = docker_core::cli::parse(run_args)? else { return Err("usage: docker create [flags] <image> [command...]".into()) };
    commands::ensure_image(&cfg.target)?;
    cfg.detach = true;
    let e = registry::create(&cfg)?;
    println!("{}", e.id);
    Ok(0)
}

fn diff(args: &[String]) -> Res<i32> {
    let q = args.first().ok_or("\"docker diff\" requires exactly 1 argument")?;
    let e = registry::find(q)?;
    let v = api_json("GET", &format!("/containers/{}/changes", e.id), vec![])?;
    for c in v.as_array().into_iter().flatten() {
        let kind = match c["Kind"].as_u64() {
            Some(0) => 'C',
            Some(1) => 'A',
            _ => 'D',
        };
        println!("{kind} {}", c["Path"].as_str().unwrap_or(""));
    }
    Ok(0)
}

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn commit(args: &[String]) -> Res<i32> {
    let f = flags(args, &["p", "pause"], &["m", "message", "a", "author", "c", "change"], false)?;
    let q = f.pos.first().ok_or("\"docker commit\" requires at least 1 argument")?;
    let e = registry::find(q)?;
    let mut target = format!("/commit?container={}", e.id);
    if let Some(r) = f.pos.get(1) {
        let (repo, tag) = split_repo_tag(r);
        target += &format!("&repo={}", pct(&repo));
        if r.contains(':') && tag != "latest" || r.ends_with(":latest") {
            target += &format!("&tag={}", pct(&tag));
        }
    }
    if let Some(m) = f.get(&["m", "message"]) {
        target += &format!("&comment={}", pct(m));
    }
    if let Some(a) = f.get(&["a", "author"]) {
        target += &format!("&author={}", pct(a));
    }
    let changes = f.all(&["c", "change"]);
    if !changes.is_empty() {
        target += &format!("&changes={}", pct(&changes.join("\n")));
    }
    let v = api_json("POST", &target, b"{}".to_vec())?;
    println!("{}", v["Id"].as_str().unwrap_or(""));
    Ok(0)
}

fn export(args: &[String]) -> Res<i32> {
    let f = flags(args, &[], &["o", "output"], false)?;
    let q = f.pos.first().ok_or("\"docker export\" requires exactly 1 argument")?;
    let e = registry::find(q)?;
    let target = format!("/containers/{}/export", e.id);
    let code = match f.get(&["o", "output"]) {
        Some(path) => {
            let mut file = fs::File::create(path)?;
            api::call("GET", &target, vec![], &mut file)?.0
        }
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                return Err("cowardly refusing to save to a terminal. Use the -o flag or redirect".into());
            }
            api::call("GET", &target, vec![], &mut std::io::stdout().lock())?.0
        }
    };
    if code >= 400 {
        return Err(format!("export failed ({code})").into());
    }
    Ok(0)
}

fn import(args: &[String]) -> Res<i32> {
    let f = flags(args, &[], &["c", "change", "m", "message", "platform"], false)?;
    let src = f.pos.first().ok_or("\"docker import\" requires at least 1 argument")?;
    let name = f.pos.get(1).cloned().unwrap_or_else(|| format!("import-{}", registry::new_id()));
    let store = api::store();
    let rf = docker_image::Reference::parse(&name)?;
    let stage = store.tmp_dir().join(format!("import-{}", registry::new_id()));
    fs::create_dir_all(&stage)?;
    let tar_path = if src == "-" {
        let p = stage.join("input.tar");
        std::io::copy(&mut std::io::stdin().lock(), &mut fs::File::create(&p)?)?;
        p
    } else {
        std::path::PathBuf::from(src)
    };
    let bytes = fs::read(&tar_path).map_err(|e| format!("{src}: {e}"))?;
    let digest = {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(&bytes))
    };
    let root = stage.join("root");
    fs::create_dir_all(&root)?;
    if !docker_image::extract_archive(&tar_path, &root, &stage.join("untar.tmp"))? {
        return Err("not a tar archive".into());
    }
    let dir = store.adopt_layer(&digest, &root)?;
    let _ = fs::remove_dir_all(&stage);
    let mut meta = docker_image::ImageMeta { name: rf.display(), ..Default::default() };
    meta.layers.push(docker_image::LayerRef { digest, media_type: "application/vnd.rustdocker.layer.v1+dir".into(), size: docker_image::dir_size(&dir) });
    for c in f.all(&["c", "change"]) {
        api::apply_change(&mut meta, &c)?;
    }
    store.save(&rf, &meta)?;
    println!("sha256:{}", api::fake_id(&meta.name));
    Ok(0)
}

fn port(args: &[String]) -> Res<i32> {
    let q = args.first().ok_or("\"docker port\" requires at least 1 argument")?;
    let e = registry::find(q)?;
    let want = args.get(1).map(|p| if p.contains('/') { p.clone() } else { format!("{p}/tcp") });
    let c = e.config.clone().unwrap_or_default();
    let mut shown = false;
    for p in &c.ports {
        let key = format!("{}/{}", p.container, if p.udp { "udp" } else { "tcp" });
        if want.as_ref().is_some_and(|w| *w != key) {
            continue;
        }
        let ip = p.host_ip.map_or("0.0.0.0".to_string(), |i| i.to_string());
        if want.is_some() { println!("{ip}:{}", p.host) } else { println!("{key} -> {ip}:{}", p.host) }
        shown = true;
    }
    if !shown && want.is_some() {
        return Err(format!("no public port '{}' published for {}", want.unwrap_or_default(), e.label()).into());
    }
    Ok(0)
}

fn attach(args: &[String]) -> Res<i32> {
    let f = flags(args, &["no-stdin", "sig-proxy"], &["detach-keys"], false)?;
    let q = f.pos.first().ok_or("\"docker attach\" requires exactly 1 argument")?;
    let e = registry::find(q)?;
    if !e.running() {
        return Err(format!("container {} is not running", e.label()).into());
    }
    // follows the container's output until it exits (input is not forwarded to a running container's stdin)
    logs(&["-f".to_string(), "--tail".to_string(), "0".to_string(), e.id.clone()])?;
    let code = registry::find(&e.id).ok().and_then(|x| x.exit_code).unwrap_or(0);
    Ok(code)
}

fn manifest_inspect(args: &[String]) -> Res<i32> {
    let f = flags(args, &["verbose", "insecure"], &[], false)?;
    let name = f.pos.first().ok_or("usage: docker manifest inspect IMAGE")?;
    let v = docker_image::manifest_inspect(&api::store(), &docker_image::Reference::parse(name)?)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(0)
}

fn context(args: &[String]) -> Res<i32> {
    match args.first().map(String::as_str) {
        None | Some("ls" | "list") => {
            let host = std::env::var("DOCKER_HOST").unwrap_or_else(|_| "unix:///var/run/docker.sock".into());
            println!("NAME        DESCRIPTION                               DOCKER ENDPOINT   ERROR\ndefault *   Current DOCKER_HOST based configuration   {host}");
            Ok(0)
        }
        Some("show") => {
            println!("default");
            Ok(0)
        }
        Some("use") if args.get(1).is_some_and(|c| c == "default") => {
            println!("default\nCurrent context is now \"default\"");
            Ok(0)
        }
        Some("inspect") => {
            println!("{}", serde_json::to_string_pretty(&[json!({ "Name": "default", "Metadata": {}, "Endpoints": { "docker": { "Host": std::env::var("DOCKER_HOST").unwrap_or_default() } } })])?);
            Ok(0)
        }
        _ => Err("only the `default` context exists: set DOCKER_HOST (or run `docker daemon`) to talk to another engine".into()),
    }
}

// ---- prune family

fn network_prune(_args: &[String]) -> Res {
    let v = api_json("POST", "/networks/prune", vec![])?;
    println!("Deleted Networks:");
    for n in v["NetworksDeleted"].as_array().into_iter().flatten() {
        println!("{}", n.as_str().unwrap_or(""));
    }
    Ok(())
}

fn builder_prune(_args: &[String]) -> Res<i32> {
    let freed = api::store().prune()?;
    println!("Total:\t{}", human_size(freed));
    Ok(0)
}

fn system_prune(args: &[String]) -> Res<i32> {
    let f = flags(args, &["a", "all", "f", "force", "volumes"], &["filter"], false)?;
    if !f.has(&["f", "force"]) {
        println!("WARNING! This will remove:\n  - all stopped containers\n  - all networks not used by at least one container{}\n  - all {} images\n  - all build cache\n\nAre you sure you want to continue? [y/N] ", if f.has(&["volumes"]) { "\n  - all anonymous volumes not used by at least one container" } else { "" }, if f.has(&["a", "all"]) { "unused" } else { "dangling" });
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        if !a.trim().eq_ignore_ascii_case("y") {
            println!("Total reclaimed space: 0B");
            return Ok(0);
        }
    }
    commands::container_prune()?;
    network_prune(&[])?;
    if f.has(&["volumes"]) {
        let (names, _) = docker_core::volumes::prune();
        println!("Deleted Volumes:\n{}", names.join("\n"));
    }
    let mut freed = 0;
    if f.has(&["a", "all"]) {
        // images no container uses
        let used: HashSet<String> = registry::list().iter().map(|e| e.image.clone()).collect();
        for m in api::store().list().iter().filter(|m| !used.contains(&m.name)) {
            if let Ok(r) = docker_image::Reference::parse(&m.name) {
                println!("Untagged: {}", m.name);
                freed += api::store().remove(&r).unwrap_or(0);
            }
        }
    }
    freed += api::store().prune()?;
    println!("Total reclaimed space: {}", human_size(freed));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn human_sizes_and_durations_read_like_docker() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(7_800_000), "7.80MB");
        assert_eq!(human_size(124_000_000), "124MB");
        assert_eq!(human_size(13_300_000), "13.3MB");
        assert_eq!(human_size(1_180_000_000), "1.18GB");
        assert_eq!(human_duration(0), "Less than a second");
        assert_eq!(human_duration(30), "30 seconds");
        assert_eq!(human_duration(75), "About a minute");
        assert_eq!(human_duration(600), "10 minutes");
        assert_eq!(human_duration(3700), "About an hour");
        assert_eq!(human_duration(86400 * 3), "3 days");
        assert_eq!(human_duration(86400 * 5), "5 days");
        assert_eq!(human_duration(86400 * 21), "3 weeks");
    }

    #[test]
    fn flags_clusters_values_and_stop() {
        let f = flags(&v("-aq -n 3 --format={{.ID}} --filter a=b -f c=d"), &["a", "q"], &["n", "format", "f", "filter"], false).unwrap();
        assert!(f.has(&["a"]) && f.has(&["q"]));
        assert_eq!((f.get(&["n"]).map(String::as_str), f.get(&["format"]).map(String::as_str)), (Some("3"), Some("{{.ID}}")));
        assert_eq!(f.all(&["f", "filter"]), ["c=d", "a=b"].map(String::from));
        // exec: everything after the container belongs to the command
        let f = flags(&v("-e A=1 web ls -la"), &["i"], &["e"], true).unwrap();
        assert_eq!(f.pos, ["web", "ls", "-la"]);
        assert!(flags(&v("--nope"), &[], &[], false).is_err());
    }

    #[test]
    fn routing_of_commands() {
        assert_eq!(route(&v("container ls -a")).map(|r| r.0), Some("ps"));
        assert_eq!(route(&v("image ls")).map(|r| r.0), Some("images"));
        assert_eq!(route(&v("system prune -f")).map(|r| r.0), Some("system-prune"));
        assert_eq!(route(&v("image inspect alpine")).unwrap().1[0], "--type=image");
        assert!(route(&v("run alpine")).is_none() && route(&v("volume ls")).is_none() && route(&v("version")).is_none());
        assert_eq!(route(&v("version -f x")).map(|r| r.0), Some("version"));
    }

    #[test]
    fn repo_and_tag_split() {
        assert_eq!(split_repo_tag("alpine:latest"), ("alpine".into(), "latest".into()));
        assert_eq!(split_repo_tag("localhost:5000/app"), ("localhost:5000/app".into(), "latest".into()));
        assert_eq!(split_repo_tag("a/b:1.2"), ("a/b".into(), "1.2".into()));
    }

    #[test]
    fn logs_tail_finds_the_last_lines() {
        let p = std::env::temp_dir().join(format!("tail-test-{}", std::process::id()));
        fs::write(&p, "one\ntwo\nthree\nfour\n").unwrap();
        let mut f = fs::File::open(&p).unwrap();
        assert_eq!(tail_offset(&mut f, 2).unwrap(), "one\ntwo\n".len() as u64);
        assert_eq!(tail_offset(&mut f, 10).unwrap(), 0);
        assert_eq!(tail_offset(&mut f, 0).unwrap(), 19);
        let _ = fs::remove_file(p);
    }
}
