//! `docker daemon`: a subset of the Docker Engine API (HTTP over a unix socket or TCP), so SDKs, IDE plugins and
//! scripts that speak Docker can drive this engine. Covered: /_ping /version /info, containers (list, create,
//! inspect, start, stop, restart, kill, pause, unpause, wait, logs, remove), images (list, inspect, pull, tag,
//! remove), volumes and networks. Not covered: exec, attach, build, events, stats streams, swarm.
//!
//! The daemon is multi-threaded, so it never forks containers itself: start/stop/pull/remove run this same binary
//! as a subprocess (`docker start <id>` ...), whose detached supervisor then lives on independently.
use docker_core::{
    cli::{is_volume_name, Config, Network, Port, Restart, Volume},
    networks,
    registry::{self, Entry},
    volumes,
};
use docker_image::{Health, Reference, Store};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    error::Error,
    sync::{mpsc, Mutex, OnceLock},
    fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const API_VERSION: &str = "1.43";

/// The architecture the way Go (and so every Docker client) spells it.
fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

// ---- HTTP plumbing

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: Vec<u8>,
    /// The client asked to take over the connection (`Connection: Upgrade`), as SDKs do for exec/attach.
    pub upgrade: bool,
    /// What the client keeps sending after an upgrade (its stdin); set by the connection loop.
    pub stdin: Mutex<Option<Box<dyn Read + Send>>>,
}

/// A connection we can read from on a second thread while we write to it.
pub trait Duplex: Read + Write {
    fn reader(&self) -> Option<Box<dyn Read + Send>>;
}
impl Duplex for std::net::TcpStream {
    fn reader(&self) -> Option<Box<dyn Read + Send>> {
        self.try_clone().ok().map(|c| Box::new(c) as _)
    }
}
#[cfg(unix)]
impl Duplex for std::os::unix::net::UnixStream {
    fn reader(&self) -> Option<Box<dyn Read + Send>> {
        self.try_clone().ok().map(|c| Box::new(c) as _)
    }
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or((pct_decode(p), String::new()), |(k, v)| (pct_decode(k), pct_decode(v)))).collect()
}

/// `/v1.43/containers/json?all=1` -> ("/containers/json", {all: 1})
fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = match path.strip_prefix("/v") {
        Some(rest) if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) => rest.find('/').map_or("/", |i| &rest[i..]),
        _ => path,
    };
    (pct_decode(path), parse_query(query))
}

pub fn read_request<R: BufRead>(r: &mut R) -> io::Result<Option<Request>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut it = line.split_whitespace();
    let (Some(method), Some(target)) = (it.next(), it.next()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request line"));
    };
    let (mut len, mut upgrade, mut chunked) = (0usize, false, false);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("upgrade") {
                upgrade = true;
            } else if k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked") {
                chunked = true; // `docker build` streams its context this way
            }
        }
    }
    const MAX_BODY: usize = 512 << 20;
    let too_big = || io::Error::new(io::ErrorKind::InvalidData, "request body too large");
    let mut body = vec![];
    if chunked {
        loop {
            let mut size = String::new();
            r.read_line(&mut size)?;
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if n == 0 {
                loop {
                    // trailers, up to the blank line
                    let mut t = String::new();
                    if r.read_line(&mut t)? == 0 || t.trim().is_empty() {
                        break;
                    }
                }
                break;
            }
            if body.len() + n > MAX_BODY {
                return Err(too_big());
            }
            let at = body.len();
            body.resize(at + n, 0);
            r.read_exact(&mut body[at..])?;
            r.read_exact(&mut [0u8; 2])?; // the CRLF after the chunk
        }
    } else {
        if len > MAX_BODY {
            return Err(too_big());
        }
        body = vec![0u8; len];
        r.read_exact(&mut body)?;
    }
    let (path, query) = split_target(target);
    Ok(Some(Request { method: method.to_ascii_uppercase(), path, query, body, upgrade, stdin: Mutex::new(None) }))
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        101 => "UPGRADED",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        501 => "Not Implemented",
        _ => "Internal Server Error",
    }
}

fn head(w: &mut dyn Write, code: u16, ctype: &str, len: Option<usize>) -> io::Result<()> {
    write!(w, "HTTP/1.1 {code} {}\r\nApi-Version: {API_VERSION}\r\nServer: Sasta Docker\r\nDocker-Experimental: false\r\nContent-Type: {ctype}\r\n", status_text(code))?;
    match len {
        Some(n) => write!(w, "Content-Length: {n}\r\n\r\n"),
        None => write!(w, "Transfer-Encoding: chunked\r\n\r\n"),
    }
}

fn chunk(w: &mut dyn Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(w, "{:x}\r\n", data.len())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")?;
    w.flush()
}

fn end_chunks(w: &mut dyn Write) -> io::Result<()> {
    w.write_all(b"0\r\n\r\n")?;
    w.flush()
}

/// What a handler produced. `Streamed` means it already wrote the whole response.
pub enum Reply {
    Json(u16, Value),
    Text(u16, String),
    Empty(u16),
    Streamed,
    /// Streamed over a hijacked (upgraded) connection: the server closes it when done.
    Hijacked,
}

fn fail(code: u16, msg: impl Into<String>) -> Reply {
    Reply::Json(code, json!({ "message": msg.into() }))
}

// ---- helpers

/// RFC 3339 UTC timestamp from unix seconds (days-from-civil algorithm).
pub fn rfc3339(secs: u64) -> String {
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// A stable 64-hex-char id derived from text (images have no config digest of their own in this store).
fn fake_id(text: &str) -> String {
    let mut out = String::new();
    for round in 0..4u64 {
        let mut h: u64 = 0xcbf29ce484222325 ^ round.wrapping_mul(0x9e3779b97f4a7c15);
        for b in text.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        out.push_str(&format!("{h:016x}"));
    }
    out
}

/// Run this binary with `args` (so daemon threads never fork containers). Ok(stdout) or Err(message).
fn cli(args: &[&str]) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = Command::new(exe).args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let e = String::from_utf8_lossy(&out.stderr);
        Err(e.trim().trim_start_matches("docker: ").to_string())
    }
}

fn store() -> Store {
    Store::open(registry::data_dir().join("store"))
}

fn state_of(e: &Entry) -> &'static str {
    if e.running() {
        "running"
    } else if e.restarting && e.exit_code.is_none() {
        "restarting"
    } else if e.created {
        "created"
    } else {
        "exited"
    }
}

fn status_of(e: &Entry) -> String {
    let now = registry::now_unix();
    match state_of(e) {
        "running" => format!("Up {} seconds", now.saturating_sub(e.started_unix)),
        "created" => "Created".into(),
        "restarting" => format!("Restarting ({}) ", e.restarts),
        _ => format!("Exited ({}) {} seconds ago", e.exit_code.unwrap_or(-1), now.saturating_sub(e.finished_unix.unwrap_or(now))),
    }
}

fn names_of(e: &Entry) -> Vec<String> {
    vec![format!("/{}", e.name.clone().unwrap_or_else(|| e.id.chars().take(12).collect()))]
}

fn port_list(e: &Entry) -> Vec<Value> {
    e.ports
        .iter()
        .filter_map(|p| {
            let (h, c) = p.split_once("->")?;
            Some(json!({ "PrivatePort": c.parse::<u16>().ok()?, "PublicPort": h.parse::<u16>().ok()?, "Type": "tcp", "IP": "0.0.0.0" }))
        })
        .collect()
}

fn container_summary(e: &Entry) -> Value {
    json!({
        "Id": e.id, "Names": names_of(e), "Image": e.image, "ImageID": fake_id(&e.image),
        "Command": e.cmd.join(" "), "Created": e.started_unix, "State": state_of(e), "Status": status_of(e),
        "Ports": port_list(e), "Labels": {}, "HostConfig": { "NetworkMode": e.config.as_ref().and_then(|c| c.network_name.clone()).unwrap_or_else(|| "bridge".into()) },
    })
}

fn container_inspect(e: &Entry) -> Value {
    let c = e.config.clone().unwrap_or_default();
    let nets = match (&c.network_name, &e.ip) {
        (Some(n), Some(ip)) => json!({ n: { "IPAddress": ip, "Aliases": c.aliases } }),
        (None, Some(ip)) => json!({ "bridge": { "IPAddress": ip } }),
        _ => json!({}),
    };
    let exposed: Map<String, Value> = c.ports.iter().map(|p| (format!("{}/tcp", p.container), json!({}))).collect();
    let bindings: Map<String, Value> = c.ports.iter().map(|p| (format!("{}/tcp", p.container), json!([{ "HostIp": "", "HostPort": p.host.to_string() }]))).collect();
    json!({
        "Id": e.id, "Created": rfc3339(e.started_unix), "Path": e.cmd.first(), "Args": e.cmd.iter().skip(1).collect::<Vec<_>>(),
        "State": {
            "Status": state_of(e), "Running": e.running(), "Paused": false, "Restarting": e.restarting, "OOMKilled": false, "Dead": false,
            "Pid": e.pid, "ExitCode": e.exit_code.unwrap_or(0), "Error": "",
            "StartedAt": rfc3339(e.started_unix), "FinishedAt": e.finished_unix.map_or("0001-01-01T00:00:00Z".into(), rfc3339),
            "Health": e.health.as_ref().map(|h| json!({ "Status": h })),
        },
        "Image": fake_id(&e.image), "Name": names_of(e)[0], "RestartCount": e.restarts,
        "HostConfig": {
            "NetworkMode": c.network_name.clone().unwrap_or_else(|| "default".into()), "PortBindings": bindings, "Privileged": c.privileged,
            "RestartPolicy": { "Name": c.restart.to_arg().split(':').next().unwrap_or("no") },
            "Binds": c.volumes.iter().map(|v| format!("{}:{}{}", v.host, v.container, if v.ro { ":ro" } else { "" })).collect::<Vec<_>>(),
            "Memory": c.mem_bytes.unwrap_or(0), "PidsLimit": c.procs, "AutoRemove": c.auto_remove,
            "LogConfig": { "Type": "json-file", "Config": {} },
        },
        "Mounts": c.volumes.iter().map(|v| json!({ "Type": "bind", "Source": v.host, "Destination": v.container, "RW": !v.ro })).collect::<Vec<_>>(),
        "Config": {
            "Hostname": "container", "User": c.user, "Tty": c.tty, "Env": if e.env.is_empty() { &c.env } else { &e.env }, "Cmd": e.cmd, "Image": e.image,
            "WorkingDir": e.workdir, "Entrypoint": c.entrypoint, "ExposedPorts": exposed, "Labels": {},
        },
        "NetworkSettings": { "IPAddress": e.ip.clone().unwrap_or_default(), "Networks": nets, "Ports": {} },
    })
}

fn image_json(m: &docker_image::ImageMeta) -> Value {
    let exposed: Map<String, Value> = m.exposed.iter().map(|p| (p.clone(), json!({}))).collect();
    json!({
        "Id": format!("sha256:{}", fake_id(&m.name)), "RepoTags": [m.name], "RepoDigests": [], "Created": 0, "Size": m.size(), "VirtualSize": m.size(),
        "Architecture": go_arch(), "Os": "linux",
        "Config": {
            "Env": m.env, "Cmd": m.cmd, "Entrypoint": m.entrypoint, "WorkingDir": m.working_dir, "User": m.user,
            "ExposedPorts": exposed, "Labels": m.labels,
        },
        "RootFS": { "Type": "layers", "Layers": m.layers.iter().map(|l| &l.digest).collect::<Vec<_>>() },
    })
}

// ---- POST /containers/create

fn dur(v: Option<&Value>, default: u64) -> u64 {
    v.and_then(Value::as_u64).filter(|n| *n > 0).map_or(default, |ns| (ns / 1_000_000_000).max(1))
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Map the API's create body onto a container config.
pub fn config_from_create(body: &Value, ncpu: u64) -> Result<Config, String> {
    let image = body.get("Image").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("Image is required")?;
    let host = body.get("HostConfig").cloned().unwrap_or(json!({}));
    let mut cfg = Config { target: image.to_string(), detach: true, ..Config::default() };
    cfg.cmd = match body.get("Cmd") {
        Some(Value::String(s)) => docker_image::dockerfile::words(s).map_err(|e| e.to_string())?,
        other => strings(other),
    };
    let mut entry = match body.get("Entrypoint") {
        Some(Value::String(s)) => vec![s.clone()],
        other => strings(other),
    };
    if !entry.is_empty() {
        cfg.entrypoint = Some(entry.remove(0));
        entry.extend(cfg.cmd);
        cfg.cmd = entry;
    }
    cfg.env = strings(body.get("Env"));
    cfg.workdir = body.get("WorkingDir").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    cfg.user = body.get("User").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    cfg.tty = body.get("Tty").and_then(Value::as_bool).unwrap_or(false);

    for b in strings(host.get("Binds")) {
        // "src:dst" or "src:dst:opts" (opts are comma-separated: ro, rw, z, ...)
        let mut parts = b.splitn(3, ':');
        let (src, dst, opts) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let ro = opts.split(',').any(|o| o == "ro");
        cfg.volumes.push(Volume::parse(&format!("{src}:{dst}{}", if ro { ":ro" } else { "" })).map_err(|e| e.to_string())?);
    }
    if let Some(Value::Object(pb)) = host.get("PortBindings") {
        for (k, v) in pb {
            let (container, proto) = k.split_once('/').unwrap_or((k, "tcp"));
            if proto != "tcp" {
                return Err(format!("{proto} port bindings are not supported"));
            }
            for b in v.as_array().into_iter().flatten() {
                if let Some(hp) = b.get("HostPort").and_then(Value::as_str).filter(|h| !h.is_empty()) {
                    cfg.ports.push(Port::parse(&format!("{hp}:{container}")).map_err(|e| e.to_string())?);
                }
            }
        }
    }
    cfg.mem_bytes = host.get("Memory").and_then(Value::as_u64).filter(|m| *m > 0);
    if let Some(n) = host.get("NanoCpus").and_then(Value::as_u64).filter(|n| *n > 0) {
        cfg.cpu_percent = Some(((n as f64 / 1e9 / ncpu.max(1) as f64 * 100.0).round() as u32).clamp(1, 100));
    }
    if let Some(rp) = host.get("RestartPolicy") {
        let name = rp.get("Name").and_then(Value::as_str).unwrap_or("");
        let max = rp.get("MaximumRetryCount").and_then(Value::as_u64).unwrap_or(0);
        cfg.restart = match name {
            "" | "no" => Restart::No,
            "on-failure" if max > 0 => Restart::OnFailure(max as u32),
            other => Restart::parse(other).map_err(|e| e.to_string())?,
        };
    }
    cfg.auto_remove = host.get("AutoRemove").and_then(Value::as_bool).unwrap_or(false);
    cfg.privileged = host.get("Privileged").and_then(Value::as_bool).unwrap_or(false);
    cfg.cap_add = strings(host.get("CapAdd"));
    cfg.cap_drop = strings(host.get("CapDrop"));
    cfg.security_opt = strings(host.get("SecurityOpt")).into_iter().map(|s| s.replace("seccomp:unconfined", "seccomp=unconfined")).collect();
    match host.get("NetworkMode").and_then(Value::as_str).unwrap_or("") {
        "" | "default" | "bridge" => cfg.network = Some(Network::Bridge),
        "host" => cfg.network = Some(Network::Host),
        "none" => cfg.network = Some(Network::None),
        name if is_volume_name(name) => {
            cfg.network = Some(Network::Bridge);
            cfg.network_name = Some(name.to_string());
        }
        other => return Err(format!("unsupported NetworkMode {other:?}")),
    }
    if let Some(Value::Object(eps)) = body.pointer("/NetworkingConfig/EndpointsConfig") {
        for (net, ep) in eps {
            if cfg.network_name.is_none() && !matches!(net.as_str(), "bridge" | "host" | "none" | "default") && is_volume_name(net) {
                cfg.network_name = Some(net.clone());
                cfg.network = Some(Network::Bridge);
            }
            cfg.aliases.extend(strings(ep.get("Aliases")));
        }
    }
    if let Some(h) = body.get("Healthcheck") {
        let t = strings(h.get("Test"));
        let test = match t.first().map(String::as_str) {
            Some("CMD-SHELL") => Some(vec!["/bin/sh".to_string(), "-c".to_string(), t[1..].join(" ")]),
            Some("CMD") => Some(t[1..].to_vec()),
            Some("NONE") => {
                cfg.no_healthcheck = true;
                None
            }
            _ => None,
        };
        if let Some(test) = test {
            let mut hc = Health::new(test);
            hc.interval_s = dur(h.get("Interval"), hc.interval_s);
            hc.timeout_s = dur(h.get("Timeout"), hc.timeout_s);
            hc.start_period_s = h.get("StartPeriod").and_then(Value::as_u64).map_or(0, |n| n / 1_000_000_000);
            hc.retries = h.get("Retries").and_then(Value::as_u64).filter(|n| *n > 0).map_or(hc.retries, |n| n as u32);
            cfg.health = Some(hc);
        }
    }
    Ok(cfg)
}

// ---- handlers

fn q_flag(r: &Request, k: &str) -> bool {
    r.query.get(k).is_some_and(|v| !matches!(v.as_str(), "" | "0" | "false"))
}

fn container(id: &str) -> Result<Entry, Reply> {
    registry::find(id).map_err(|_| fail(404, format!("No such container: {id}")))
}

fn create_container(r: &Request) -> Reply {
    let body: Value = match serde_json::from_slice(&r.body) {
        Ok(v) => v,
        Err(e) => return fail(400, format!("invalid JSON: {e}")),
    };
    let ncpu = thread::available_parallelism().map_or(1, |n| n.get() as u64);
    let mut cfg = match config_from_create(&body, ncpu) {
        Ok(c) => c,
        Err(m) => return fail(400, m),
    };
    cfg.target = image_by_id(&cfg.target, &image_names());
    match Reference::parse(&cfg.target) {
        Ok(rf) if store().get(&rf).ok().flatten().is_some() => {}
        _ => return fail(404, format!("No such image: {}", cfg.target)),
    }
    if let Some(n) = &cfg.network_name
        && !networks::exists(n)
    {
        return fail(404, format!("network {n} not found"));
    }
    cfg.name = r.query.get("name").filter(|n| !n.is_empty()).cloned();
    if let Some(n) = &cfg.name {
        if !is_volume_name(n) {
            return fail(400, format!("Invalid container name ({n})"));
        }
        if let Ok(other) = registry::find(n) {
            return fail(409, format!("Conflict. The container name \"/{n}\" is already in use by container \"{}\".", other.id));
        }
    }
    match registry::create(&cfg) {
        Ok(e) => Reply::Json(201, json!({ "Id": e.id, "Warnings": [] })),
        Err(e) => fail(500, e.to_string()),
    }
}

/// Start a streamed response: chunked normally, or a raw `101 UPGRADED` stream if the client asked to hijack.
fn start_stream(w: &mut dyn Write, upgrade: bool, ctype: &str) -> io::Result<()> {
    if upgrade {
        write!(w, "HTTP/1.1 101 UPGRADED\r\nContent-Type: {ctype}\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n")
    } else {
        head(w, 200, ctype, None)
    }
}

fn put(w: &mut dyn Write, upgrade: bool, data: &[u8]) -> io::Result<()> {
    if upgrade {
        w.write_all(data)?;
        w.flush()
    } else {
        chunk(w, data)
    }
}

fn frame(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut f = vec![kind, 0, 0, 0];
    f.extend((data.len() as u32).to_be_bytes());
    f.extend(data);
    f
}

/// Attach to a detached terminal container (`docker run -it` / `attach`): talk to its supervisor's attach socket.
#[cfg(target_os = "linux")]
fn attach_tty(r: &Request, e: &Entry, w: &mut dyn Write) -> io::Result<Reply> {
    start_stream(w, true, "application/vnd.docker.raw-stream")?;
    let path = docker_core::platform::attach_sock(&e.id);
    // `docker run` attaches before it starts the container: give the supervisor up to 30s to appear
    let mut sock = None;
    for _ in 0..200 {
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(_) if registry::find(&e.id).is_ok_and(|x| x.created || x.running()) => thread::sleep(Duration::from_millis(150)),
            Err(_) => break,
        }
    }
    let Some(mut sock) = sock else { return Ok(Reply::Hijacked) };
    if let Some(mut input) = r.stdin.lock().unwrap().take()
        && let Ok(mut to) = sock.try_clone()
    {
        thread::spawn(move || {
            let _ = io::copy(&mut input, &mut to);
        });
    }
    let mut buf = [0u8; 8192];
    while let Ok(n) = sock.read(&mut buf) {
        if n == 0 || put(w, true, &buf[..n]).is_err() {
            break;
        }
    }
    Ok(Reply::Hijacked)
}

fn logs(r: &Request, e: &Entry, w: &mut dyn Write, attach: bool) -> io::Result<Reply> {
    let tty = e.config.as_ref().is_some_and(|c| c.tty);
    #[cfg(target_os = "linux")]
    if attach && tty && r.upgrade {
        return attach_tty(r, e, w);
    }
    let upgrade = attach && r.upgrade;
    start_stream(w, upgrade, if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" })?;
    let (path, follow) = (registry::log_path(&e.id), attach || q_flag(r, "follow"));
    let tail: Option<usize> = r.query.get("tail").and_then(|t| t.parse().ok());
    let (mut offset, mut waiting) = (0u64, 0u32);
    if let (Some(n), Ok(text)) = (tail, fs::read_to_string(&path)) {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        offset = lines[..lines.len().saturating_sub(n)].iter().map(|l| l.len() as u64).sum();
    }
    let emit = |w: &mut dyn Write, data: &[u8]| -> io::Result<()> {
        if tty {
            return put(w, upgrade, data);
        }
        for part in data.chunks(32 * 1024) {
            put(w, upgrade, &frame(1, part))?;
        }
        Ok(())
    };
    // Keep the file open once we have it: `--rm` containers delete their log the moment they exit,
    // and an open handle still reads what was written.
    let mut file: Option<fs::File> = None;
    loop {
        let mut buf = vec![];
        if file.is_none() {
            file = fs::File::open(&path).ok().filter(|f| (&*f).seek(SeekFrom::Start(offset)).is_ok());
        }
        if let Some(f) = file.as_mut() {
            let _ = f.read_to_end(&mut buf);
        }
        offset += buf.len() as u64;
        emit(w, &buf)?;
        // `docker run` attaches *before* it starts the container, so a created container is worth waiting for (30s)
        waiting += 1;
        let live = registry::find(&e.id).is_ok_and(|x| x.running() || x.restarting || (x.created && waiting < 200));
        if !follow || (!live && buf.is_empty()) {
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }
    if upgrade {
        return Ok(Reply::Hijacked);
    }
    end_chunks(w)?;
    Ok(Reply::Streamed)
}

fn pull(r: &Request, w: &mut dyn Write) -> io::Result<Reply> {
    let Some(from) = r.query.get("fromImage").filter(|s| !s.is_empty()) else { return Ok(fail(400, "fromImage is required")) };
    let name = match r.query.get("tag").filter(|t| !t.is_empty()) {
        Some(t) if !from.rsplit('/').next().unwrap_or("").contains(':') => format!("{from}:{t}"),
        _ => from.clone(),
    };
    head(w, 200, "application/json", None)?;
    let line = |v: Value| -> Vec<u8> { format!("{v}\r\n").into_bytes() };
    chunk(w, &line(json!({ "status": format!("Pulling from {from}"), "id": name.rsplit(':').next() })))?;
    match cli(&["pull", &name]) {
        Ok(_) => chunk(w, &line(json!({ "status": format!("Status: Downloaded newer image for {name}") })))?,
        Err(e) => chunk(w, &line(json!({ "errorDetail": { "message": e }, "error": e })))?,
    }
    end_chunks(w)?;
    Ok(Reply::Streamed)
}

/// `/images/<name...>/json` -> name (image names contain slashes)
fn image_name<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    path.strip_prefix("/images/")?.strip_suffix(suffix).filter(|n| !n.is_empty())
}

/// An image id (`sha256:<hex>` or a hex prefix, as clients get from /images/json) -> its name; anything else is returned as is.
pub fn image_by_id(name: &str, images: &[String]) -> String {
    let hex = name.strip_prefix("sha256:").unwrap_or(name);
    if hex.len() >= 4 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut hits = images.iter().filter(|n| fake_id(n).starts_with(hex));
        if let (Some(n), None) = (hits.next(), hits.next()) {
            return n.clone();
        }
    }
    name.to_string()
}

fn network_exists(n: &str) -> bool {
    networks::exists(n) || matches!(n, "bridge" | "host" | "none")
}

/// A network id (as returned by create/list) or name -> its name.
fn network_by_id(id: &str) -> String {
    let all = ["bridge", "host", "none"].map(String::from).into_iter().chain(networks::list());
    all.into_iter().find(|n| id.len() >= 4 && fake_id(n).starts_with(id)).unwrap_or_else(|| id.to_string())
}

fn image_names() -> Vec<String> {
    store().list().into_iter().map(|m| m.name).collect()
}

fn find_image(name: &str) -> Result<docker_image::ImageMeta, Reply> {
    let name = &image_by_id(name, &image_names());
    let rf = Reference::parse(name).map_err(|_| fail(404, format!("No such image: {name}")))?;
    store().get(&rf).ok().flatten().ok_or_else(|| fail(404, format!("No such image: {name}")))
}

fn host_memory() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("MemTotal:")?.split_whitespace().next()?.parse::<u64>().ok()))
        .map_or(0, |kb| kb * 1024)
}

fn info() -> Value {
    let all = registry::list();
    let running = all.iter().filter(|e| e.running()).count();
    json!({
        "ID": fake_id("engine"), "Containers": all.len(), "ContainersRunning": running, "ContainersPaused": 0,
        "ContainersStopped": all.len() - running, "Images": store().list().len(), "Driver": "overlay2",
        "ServerVersion": env!("CARGO_PKG_VERSION"), "OperatingSystem": "Sasta Docker engine", "OSType": "linux",
        "Architecture": go_arch(), "NCPU": thread::available_parallelism().map_or(1, |n| n.get()),
        "MemTotal": host_memory(), "Name": "sasta-docker", "DockerRootDir": registry::data_dir().to_string_lossy(),
    })
}

fn route(r: &Request, w: &mut dyn Write) -> io::Result<Reply> {
    let segs: Vec<&str> = r.path.trim_matches('/').split('/').collect();
    let m = r.method.as_str();
    Ok(match (m, segs.as_slice()) {
        ("GET" | "HEAD", ["_ping"]) => Reply::Text(200, "OK".into()),
        ("GET", ["version"]) => Reply::Json(200, json!({
            "Version": env!("CARGO_PKG_VERSION"), "ApiVersion": API_VERSION, "MinAPIVersion": "1.24", "Os": "linux",
            "Arch": go_arch(), "GoVersion": "n/a (rust)", "GitCommit": "", "BuildTime": "",
            "Components": [{ "Name": "Engine", "Version": env!("CARGO_PKG_VERSION"), "Details": {
                "ApiVersion": API_VERSION, "MinAPIVersion": "1.24", "Os": "linux", "Arch": go_arch(),
                "GoVersion": "n/a (rust)", "GitCommit": "", "BuildTime": "", "Experimental": "false" } }],
        })),
        ("GET", ["info"]) => Reply::Json(200, info()),

        ("GET", ["containers", "json"]) => {
            let all = q_flag(r, "all");
            let mut v: Vec<Entry> = registry::list().into_iter().filter(|e| all || state_of(e) == "running" || state_of(e) == "restarting").collect();
            v.reverse(); // newest first, like Docker
            Reply::Json(200, Value::Array(v.iter().map(container_summary).collect()))
        }
        ("POST", ["containers", "create"]) => create_container(r),
        ("GET", ["containers", id, "json"]) => match container(id) {
            Ok(e) => Reply::Json(200, container_inspect(&e)),
            Err(rep) => rep,
        },
        ("POST", ["containers", id, "start"]) => match container(id) {
            Ok(e) if e.running() => Reply::Empty(304),
            Ok(e) => match cli(&["start", &e.id]) {
                Ok(_) => Reply::Empty(204),
                Err(m) => fail(500, m),
            },
            Err(rep) => rep,
        },
        ("POST", ["containers", id, op @ ("stop" | "restart")]) => match container(id) {
            Ok(e) if *op == "stop" && !e.running() && !e.restarting => Reply::Empty(304),
            Ok(e) => {
                let t = r.query.get("t").cloned().unwrap_or_else(|| "10".into());
                match cli(&[op, "-t", &t, &e.id]) {
                    Ok(_) => Reply::Empty(204),
                    Err(m) => fail(500, m),
                }
            }
            Err(rep) => rep,
        },
        ("POST", ["containers", id, op @ ("kill" | "pause" | "unpause")]) => match container(id) {
            Ok(e) if !e.running() => fail(409, format!("Container {} is not running", e.id)),
            Ok(e) => match cli(&[op, &e.id]) {
                Ok(_) => Reply::Empty(204),
                Err(m) => fail(500, m),
            },
            Err(rep) => rep,
        },
        ("POST", ["containers", id, "wait"]) => match container(id) {
            Ok(e) => {
                // Headers go out at once: `docker run` waits for them before it starts the container.
                head(w, 200, "application/json", None)?;
                let mut code = 0;
                loop {
                    match registry::find(&e.id) {
                        Ok(x) if !x.running() && !x.restarting && !x.created => {
                            code = x.exit_code.unwrap_or(-1);
                            break;
                        }
                        Ok(x) => code = x.exit_code.unwrap_or(code),
                        Err(_) => break, // removed (`--rm`): the last exit code we saw is all we have
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                chunk(w, format!("{}\n", json!({ "StatusCode": code })).as_bytes())?;
                end_chunks(w)?;
                Reply::Streamed
            }
            Err(rep) => rep,
        },
        ("GET", ["containers", id, "logs"]) => match container(id) {
            Ok(e) => return logs(r, &e, w, false),
            Err(rep) => rep,
        },
        ("DELETE", ["containers", id]) => match container(id) {
            Ok(e) if (e.running() || e.restarting) && !q_flag(r, "force") => fail(409, format!("You cannot remove a running container {}. Stop the container before attempting removal or force remove", e.id)),
            Ok(e) => {
                let args: Vec<&str> = if q_flag(r, "force") { vec!["rm", "-f", e.id.as_str()] } else { vec!["rm", e.id.as_str()] };
                match cli(&args) {
                    Ok(_) => Reply::Empty(204),
                    Err(m) => fail(500, m),
                }
            }
            Err(rep) => rep,
        },
        ("POST", ["containers", id, "attach"]) => match container(id) {
            Ok(e) => return logs(r, &e, w, true),
            Err(rep) => rep,
        },
        ("POST", ["containers", id, "exec"]) => exec_create(r, id),
        ("POST", ["exec", eid, "start"]) => return exec_start(r, eid, w),
        ("GET", ["exec", eid, "json"]) => match execs().lock().ok().and_then(|m| m.get(*eid).cloned()) {
            Some(x) => Reply::Json(200, json!({ "ID": eid, "ContainerID": x.container, "Running": x.exit.is_none(), "ExitCode": x.exit, "Pid": 0, "OpenStdin": false, "OpenStdout": true, "OpenStderr": true, "CanRemove": x.exit.is_some() })),
            None => fail(404, format!("No such exec instance: {eid}")),
        },
        ("GET", ["events"]) => return events(r, w),
        ("POST", ["build"]) => return build(r, w),
        ("POST", ["containers", id, "rename"]) => match (container(id), r.query.get("name")) {
            (Ok(e), Some(n)) => match cli(&["rename", &e.id, n]) {
                Ok(_) => Reply::Empty(204),
                Err(m) => fail(409, m),
            },
            (Ok(_), None) => fail(400, "name is required"),
            (Err(rep), _) => rep,
        },
        // a pty has no window size to change here (the terminal sits on the client's side of the stream)
        #[cfg(unix)]
        ("POST", ["exec", id, "resize"]) if ttys().lock().is_ok_and(|m| m.contains_key(*id)) => {
            let n = |k: &str| r.query.get(k).and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
            if let Some(fd) = ttys().lock().ok().and_then(|m| m.get(*id).copied()) {
                set_winsize(fd, n("h"), n("w"));
            }
            Reply::Empty(200)
        }
        ("POST", ["containers", id, "resize"]) | ("POST", ["exec", id, "resize"]) => match container(id) {
            Ok(_) => Reply::Empty(200),
            Err(_) if execs().lock().is_ok_and(|m| m.contains_key(*id)) => Reply::Empty(200),
            Err(rep) => rep,
        },
        ("POST", ["containers", id, "update"]) => match container(id) {
            Ok(e) => {
                let b: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                let mut args = vec!["update".to_string()];
                if let Some(m) = b.get("Memory").and_then(Value::as_u64) {
                    args.extend(["--mem".into(), (m >> 20).to_string()]);
                }
                if let Some(n) = b.get("NanoCpus").and_then(Value::as_u64) {
                    let ncpu = thread::available_parallelism().map_or(1.0, |n| n.get() as f64);
                    args.extend(["--cpu".into(), ((n as f64 / 1e9 / ncpu * 100.0).round() as u64).clamp(if n == 0 { 0 } else { 1 }, 100).to_string()]);
                }
                if let Some(n) = b.get("PidsLimit").and_then(Value::as_i64) {
                    args.extend(["--procs".into(), n.max(0).to_string()]);
                }
                if args.len() == 1 {
                    Reply::Json(200, json!({ "Warnings": ["only Memory, NanoCpus and PidsLimit can be updated"] }))
                } else {
                    args.push(e.id);
                    let a: Vec<&str> = args.iter().map(String::as_str).collect();
                    match cli(&a) {
                        Ok(_) => Reply::Json(200, json!({ "Warnings": [] })),
                        Err(m) => fail(409, m),
                    }
                }
            }
            Err(rep) => rep,
        },

        ("GET", ["images", "json"]) => Reply::Json(200, Value::Array(store().list().iter().map(image_json).collect())),
        ("POST", ["images", "create"]) => return pull(r, w),
        ("GET", _) if image_name(&r.path, "/json").is_some() => match find_image(image_name(&r.path, "/json").unwrap_or("")) {
            Ok(m) => Reply::Json(200, image_json(&m)),
            Err(rep) => rep,
        },
        ("POST", _) if image_name(&r.path, "/tag").is_some() => {
            let src = &image_by_id(image_name(&r.path, "/tag").unwrap_or(""), &image_names());
            let dst = match (r.query.get("repo"), r.query.get("tag")) {
                (Some(repo), Some(t)) if !t.is_empty() => format!("{repo}:{t}"),
                (Some(repo), _) => repo.clone(),
                _ => return Ok(fail(400, "repo is required")),
            };
            match cli(&["tag", src, &dst]) {
                Ok(_) => Reply::Empty(201),
                Err(m) => fail(404, m),
            }
        }
        ("DELETE", ["images", ..]) => {
            let name = r.path.strip_prefix("/images/").unwrap_or("");
            match find_image(name) {
                Ok(m) => match cli(&["rmi", &m.name]) {
                    Ok(_) => Reply::Json(200, json!([{ "Untagged": m.name }])),
                    Err(e) => fail(409, e),
                },
                Err(rep) => rep,
            }
        }

        ("GET", ["volumes"]) => Reply::Json(200, json!({
            "Volumes": volumes::list().iter().map(|v| json!({ "Name": v.name, "Driver": "local", "Mountpoint": volumes::path(&v.name).to_string_lossy(), "Scope": "local", "Labels": {} })).collect::<Vec<_>>(),
            "Warnings": null,
        })),
        ("POST", ["volumes", "create"]) => match serde_json::from_slice::<Value>(&r.body).ok().and_then(|b| b.get("Name").and_then(Value::as_str).map(String::from)) {
            Some(n) => match volumes::create(&n) {
                Ok(()) => Reply::Json(201, json!({ "Name": n, "Driver": "local", "Mountpoint": volumes::path(&n).to_string_lossy(), "Scope": "local", "Labels": {} })),
                Err(e) => fail(400, e.to_string()),
            },
            None => fail(400, "Name is required"),
        },
        ("GET", ["volumes", n]) if volumes::exists(n) => Reply::Json(200, json!({ "Name": n, "Driver": "local", "Mountpoint": volumes::path(n).to_string_lossy(), "Scope": "local", "Labels": {} })),
        ("DELETE", ["volumes", n]) => match volumes::remove(n) {
            Ok(()) => Reply::Empty(204),
            Err(e) if !volumes::exists(n) => fail(404, e.to_string()),
            Err(e) => fail(409, e.to_string()),
        },

        ("GET", ["networks"]) => {
            let mut v: Vec<Value> = ["bridge", "host", "none"].iter().map(|n| json!({ "Name": n, "Id": fake_id(n), "Driver": if *n == "bridge" { "bridge" } else { n }, "Scope": "local", "Containers": {} })).collect();
            v.extend(networks::list().iter().map(|n| json!({ "Name": n, "Id": fake_id(n), "Driver": "bridge", "Scope": "local", "Containers": {} })));
            Reply::Json(200, Value::Array(v))
        }
        ("POST", ["networks", "create"]) => match serde_json::from_slice::<Value>(&r.body).ok().and_then(|b| b.get("Name").and_then(Value::as_str).map(String::from)) {
            Some(n) => match networks::create(&n) {
                Ok(()) => Reply::Json(201, json!({ "Id": fake_id(&n), "Warning": "" })),
                Err(e) => fail(409, e.to_string()),
            },
            None => fail(400, "Name is required"),
        },
        ("GET", ["networks", id]) if network_exists(&network_by_id(id)) => {
            let n = &network_by_id(id);
            let members: Map<String, Value> = networks::members(Some(n), "").iter().map(|e| (e.id.clone(), json!({ "Name": e.label(), "IPv4Address": e.ip }))).collect();
            Reply::Json(200, json!({ "Name": n, "Id": fake_id(n), "Driver": "bridge", "Scope": "local", "Containers": members }))
        }
        ("DELETE", ["networks", id]) => {
            let n = &network_by_id(id);
            match networks::remove(n) {
                Ok(()) => Reply::Empty(204),
                Err(e) if !networks::exists(n) => fail(404, e.to_string()),
                Err(e) => fail(403, e.to_string()),
            }
        }

        _ => fail(404, "page not found"),
    })
}

fn serve_connection<S: Duplex>(stream: S) {
    let mut reader = BufReader::new(stream);
    while let Ok(Some(req)) = read_request(&mut reader) {
        if req.upgrade {
            // bytes the client already sent past the headers belong to the stream too
            let early = io::Cursor::new(reader.buffer().to_vec());
            if let Some(rest) = reader.get_ref().reader() {
                *req.stdin.lock().unwrap() = Some(Box::new(early.chain(rest)));
            }
        }
        if std::env::var_os("RUST_DOCKER_API_LOG").is_some() {
            eprintln!("{} {}?{:?} upgrade={} body={}B", req.method, req.path, req.query, req.upgrade, req.body.len());
        }
        let reply = {
            let w: &mut dyn Write = reader.get_mut();
            route(&req, w)
        };
        let w = reader.get_mut();
        let sent = match reply {
            Ok(Reply::Json(code, v)) => {
                let body = v.to_string().into_bytes();
                head(w, code, "application/json", Some(body.len())).and_then(|()| w.write_all(&body))
            }
            // HEAD gets the headers (with the real length) but no body, or the client sees a stray "OK" on the wire
            Ok(Reply::Text(code, t)) => head(w, code, "text/plain; charset=utf-8", Some(t.len())).and_then(|()| if req.method == "HEAD" { Ok(()) } else { w.write_all(t.as_bytes()) }),
            Ok(Reply::Empty(code)) => head(w, code, "application/json", Some(0)),
            Ok(Reply::Streamed) => Ok(()),
            Ok(Reply::Hijacked) => {
                let _ = w.flush();
                break; // the client reads until we close
            }
            Err(e) => {
                if std::env::var_os("RUST_DOCKER_API_LOG").is_some() {
                    eprintln!("  handler failed: {e}");
                }
                Err(e)
            }
        };
        if sent.and_then(|()| w.flush()).is_err() {
            break;
        }
    }
}

// ---- exec

#[derive(Clone)]
struct Exec {
    container: String,
    argv: Vec<String>,
    stdin: bool,
    #[cfg_attr(not(unix), allow(dead_code))] // terminals need a Unix pty
    tty: bool,
    exit: Option<i32>,
}

fn execs() -> &'static Mutex<HashMap<String, Exec>> {
    static E: OnceLock<Mutex<HashMap<String, Exec>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The argv the CLI's `exec` should run for an API exec body (Env and WorkingDir are applied via `env` / `sh -c cd`).
pub fn exec_argv(body: &Value) -> Result<Vec<String>, String> {
    let cmd = strings(body.get("Cmd"));
    if cmd.is_empty() {
        return Err("Cmd is required".into());
    }
    let mut argv = cmd;
    let env = strings(body.get("Env"));
    if !env.is_empty() {
        argv = ["env".to_string()].into_iter().chain(env).chain(argv).collect();
    }
    if let Some(dir) = body.get("WorkingDir").and_then(Value::as_str).filter(|d| !d.is_empty()) {
        argv = ["sh".to_string(), "-c".into(), "cd \"$0\" && exec \"$@\"".into(), dir.to_string()].into_iter().chain(argv).collect();
    }
    Ok(argv)
}

fn exec_create(r: &Request, id: &str) -> Reply {
    let e = match container(id) {
        Ok(e) if e.running() => e,
        Ok(e) => return fail(409, format!("Container {} is not running", e.id)),
        Err(rep) => return rep,
    };
    let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    match exec_argv(&body) {
        Ok(argv) => {
            let eid = registry::new_id();
            if let Ok(mut m) = execs().lock() {
                m.insert(eid.clone(), Exec { container: e.id, argv, stdin: body.get("AttachStdin").and_then(Value::as_bool).unwrap_or(false), tty: body.get("Tty").and_then(Value::as_bool).unwrap_or(false), exit: None });
            }
            Reply::Json(201, json!({ "Id": eid }))
        }
        Err(m) => fail(400, m),
    }
}

/// Run `exe args`, sending stdout/stderr to `sink` as (stream-type, bytes); returns the exit code.
fn run_streaming(args: &[String], input: Option<Box<dyn Read + Send>>, sink: &mut dyn FnMut(u8, &[u8]) -> io::Result<()>) -> io::Result<i32> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe).args(args).stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let (Some(mut from), Some(mut to)) = (input, child.stdin.take()) {
        // client stdin -> child; the copy ends (closing the child's stdin) when the client half-closes
        thread::spawn(move || {
            let _ = io::copy(&mut from, &mut to);
        });
    }
    let (tx, rx) = mpsc::channel::<(u8, Vec<u8>)>();
    let mut readers = vec![];
    for (kind, mut pipe) in [(1u8, child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>)), (2u8, child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>))]
        .into_iter()
        .filter_map(|(k, p)| p.map(|p| (k, p)))
    {
        let tx = tx.clone();
        readers.push(thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 || tx.send((kind, buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);
    let mut failed = None;
    for (kind, data) in rx {
        if failed.is_none()
            && let Err(e) = sink(kind, &data)
        {
            failed = Some(e); // the client went away: keep draining so the child can finish
        }
    }
    for t in readers {
        let _ = t.join();
    }
    let code = child.wait()?.code().unwrap_or(-1);
    failed.map_or(Ok(code), Err)
}

fn exec_start(r: &Request, eid: &str, w: &mut dyn Write) -> io::Result<Reply> {
    let Some(x) = execs().lock().ok().and_then(|m| m.get(eid).cloned()) else {
        return Ok(fail(404, format!("No such exec instance: {eid}")));
    };
    let detach = serde_json::from_slice::<Value>(&r.body).ok().and_then(|b| b.get("Detach").and_then(Value::as_bool)).unwrap_or(false);
    let mut args = vec!["exec".to_string()];
    #[cfg(unix)]
    if x.tty && r.upgrade && !detach {
        args.push("-t".into());
        args.push(x.container.clone());
        args.extend(x.argv.clone());
        return exec_tty(r, eid, &args, w);
    }
    if x.stdin && r.upgrade {
        args.push("-i".into());
    }
    args.push(x.container.clone());
    args.extend(x.argv.clone());
    let record = |code: i32| {
        if let Ok(mut m) = execs().lock()
            && let Some(e) = m.get_mut(eid)
        {
            e.exit = Some(code);
        }
    };
    if detach {
        let id = eid.to_string();
        thread::spawn(move || {
            let code = run_streaming(&args, None, &mut |_, _| Ok(())).unwrap_or(-1);
            if let Ok(mut m) = execs().lock()
                && let Some(e) = m.get_mut(&id)
            {
                e.exit = Some(code);
            }
        });
        return Ok(Reply::Empty(200));
    }
    start_stream(w, r.upgrade, "application/vnd.docker.multiplexed-stream")?;
    let upgrade = r.upgrade;
    let input = if x.stdin && upgrade { r.stdin.lock().unwrap().take() } else { None };
    let code = run_streaming(&args, input, &mut |kind, data| put(w, upgrade, &frame(kind, data)))?;
    record(code);
    if upgrade {
        return Ok(Reply::Hijacked);
    }
    end_chunks(w)?;
    Ok(Reply::Streamed)
}

// ---- exec with a terminal (Unix): a pty between the client's stream and `docker exec -t`

#[cfg(unix)]
fn ttys() -> &'static Mutex<HashMap<String, std::os::fd::RawFd>> {
    static T: OnceLock<Mutex<HashMap<String, std::os::fd::RawFd>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(unix)]
fn set_winsize(fd: std::os::fd::RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: plain ioctl on a pty master we own, with a valid winsize.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) };
}

#[cfg(unix)]
fn exec_tty(r: &Request, eid: &str, args: &[String], w: &mut dyn Write) -> io::Result<Reply> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let (mut m, mut s) = (0, 0);
    let ws = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty fills the two fds, which we immediately take ownership of.
    let (master, slave) = unsafe {
        if libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), &ws) != 0 {
            return Err(io::Error::last_os_error());
        }
        (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s))
    };
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(args).stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec: the child leads a session whose terminal is the pty.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    start_stream(w, true, "application/vnd.docker.raw-stream")?;
    let mut child = cmd.spawn()?;
    drop(cmd); // closes our copies of the slave, so reads on the master end when the child exits
    if let Ok(mut m) = ttys().lock() {
        m.insert(eid.to_string(), master.as_raw_fd());
    }
    let mut keys = fs::File::from(master.try_clone()?);
    if let Some(mut input) = r.stdin.lock().unwrap().take() {
        thread::spawn(move || {
            let _ = io::copy(&mut input, &mut keys);
        });
    }
    let mut out = fs::File::from(master);
    let mut buf = [0u8; 8192];
    loop {
        match out.read(&mut buf) {
            Ok(n) if n > 0 => {
                if put(w, true, &buf[..n]).is_err() {
                    let _ = child.kill(); // the client went away
                    break;
                }
            }
            _ => break, // EIO once the child has exited and everything was read
        }
    }
    let code = child.wait()?.code().unwrap_or(-1);
    if let Ok(mut m) = ttys().lock() {
        m.remove(eid);
    }
    if let Ok(mut m) = execs().lock()
        && let Some(e) = m.get_mut(eid)
    {
        e.exit = Some(code);
    }
    Ok(Reply::Hijacked)
}

// ---- events

/// One event-log line -> a Docker event object.
pub fn event_json(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    let (action, id) = (v.get("action")?.as_str()?, v.get("id")?.as_str()?);
    let secs = v.get("time").and_then(Value::as_u64).unwrap_or(0);
    let mut attrs = json!({ "image": v.get("image"), "name": v.get("name").and_then(Value::as_str).unwrap_or("") });
    if action == "die" {
        attrs["exitCode"] = json!(v.get("exitCode").and_then(Value::as_i64).unwrap_or(-1).to_string());
    }
    Some(json!({ "status": action, "id": id, "from": v.get("image"), "Type": "container", "Action": action,
                 "Actor": { "ID": id, "Attributes": attrs }, "scope": "local", "time": secs, "timeNano": secs * 1_000_000_000 }))
}

fn events(r: &Request, w: &mut dyn Write) -> io::Result<Reply> {
    head(w, 200, "application/json", None)?;
    let until: Option<u64> = r.query.get("until").and_then(|u| u.parse().ok());
    let since: Option<u64> = r.query.get("since").and_then(|u| u.parse().ok());
    let path = registry::events_path();
    // by default only what happens from now on; `since` replays the log from that time
    let mut offset = if since.is_some() { 0 } else { fs::metadata(&path).map_or(0, |m| m.len()) };
    let mut partial = String::new();
    loop {
        let t = registry::now_unix();
        if let Ok(mut f) = fs::File::open(&path)
            && f.seek(SeekFrom::Start(offset)).is_ok()
        {
            let mut buf = vec![];
            let _ = f.read_to_end(&mut buf);
            offset += buf.len() as u64;
            partial.push_str(&String::from_utf8_lossy(&buf));
            while let Some(i) = partial.find('\n') {
                let l: String = partial.drain(..=i).collect();
                if let Some(e) = event_json(&l).filter(|e| since.is_none_or(|s| e["time"].as_u64().unwrap_or(0) >= s)) {
                    chunk(w, format!("{e}\n").as_bytes())?;
                }
            }
        }
        if until.is_some_and(|u| t >= u) {
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }
    end_chunks(w)?;
    Ok(Reply::Streamed)
}

// ---- build

/// Unpack a build context (tar, optionally gzipped) into `dir`.
pub fn unpack_context(body: &[u8], dir: &Path) -> io::Result<()> {
    if body.starts_with(&[0x1f, 0x8b]) {
        tar::Archive::new(flate2::read::GzDecoder::new(body)).unpack(dir)
    } else {
        tar::Archive::new(body).unpack(dir)
    }
}

fn build(r: &Request, w: &mut dyn Write) -> io::Result<Reply> {
    let Some(tag) = r.query.get("t").filter(|t| !t.is_empty()).cloned() else { return Ok(fail(400, "t (the image name) is required")) };
    let dir = registry::data_dir().join("tmp").join(format!("api-build-{}", registry::new_id()));
    fs::create_dir_all(&dir)?;
    head(w, 200, "application/json", None)?;
    let line = |v: Value| format!("{v}\r\n").into_bytes();
    let result = (|| -> Result<(), String> {
        unpack_context(&r.body, &dir).map_err(|e| format!("cannot read the build context: {e}"))?;
        let dockerfile = r.query.get("dockerfile").cloned().unwrap_or_else(|| "Dockerfile".into());
        if !dir.join(&dockerfile).is_file() {
            let have: Vec<String> = fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
            return Err(format!("{dockerfile} not found in the build context (it has: {})", have.join(", ")));
        }
        let mut args = vec!["build".to_string(), "-t".into(), tag.clone(), "-f".into(), dir.join(&dockerfile).to_string_lossy().into_owned()];
        if let Some(Value::Object(m)) = r.query.get("buildargs").and_then(|b| serde_json::from_str::<Value>(b).ok()) {
            for (k, v) in m {
                args.extend(["--build-arg".to_string(), format!("{k}={}", v.as_str().unwrap_or(""))]);
            }
        }
        if q_flag(r, "nocache") {
            args.push("--no-cache".into());
        }
        args.push(dir.to_string_lossy().into_owned());
        let mut partial = String::new();
        let mut emit = |data: &[u8]| -> io::Result<()> {
            partial.push_str(&String::from_utf8_lossy(data));
            while let Some(i) = partial.find('\n') {
                let l: String = partial.drain(..=i).collect();
                chunk(w, &line(json!({ "stream": l })))?;
            }
            Ok(())
        };
        let mut errtext = String::new();
        let code = run_streaming(&args, None, &mut |kind, d| {
            if kind == 2 {
                errtext.push_str(&String::from_utf8_lossy(d));
                Ok(())
            } else {
                emit(d)
            }
        })
        .map_err(|e| e.to_string())?;
        if code == 0 { Ok(()) } else { Err(errtext.trim().trim_start_matches("docker: ").to_string()) }
    })();
    let _ = fs::remove_dir_all(&dir);
    match result {
        Ok(()) => {
            // clients (the Python SDK) read the new image's id from these lines
            let id = fake_id(&image_by_id(&tag, &image_names()));
            chunk(w, &line(json!({ "aux": { "ID": format!("sha256:{id}") } })))?;
            chunk(w, &line(json!({ "stream": format!("Successfully built {}\n", &id[..12]) })))?;
            chunk(w, &line(json!({ "stream": format!("Successfully tagged {tag}\n") })))?
        }
        Err(m) => chunk(w, &line(json!({ "errorDetail": { "message": m }, "error": m })))?,
    }
    end_chunks(w)?;
    Ok(Reply::Streamed)
}

/// Listen on `host` (`unix:///var/run/docker.sock` by default, or `tcp://127.0.0.1:2375`) and serve forever.
pub fn serve(host: Option<&str>) -> Result<(), Box<dyn Error>> {
    let host = host.unwrap_or("unix:///var/run/docker.sock");
    if let Some(addr) = host.strip_prefix("tcp://") {
        let l = TcpListener::bind(addr)?;
        eprintln!("docker engine API listening on {host}");
        for c in l.incoming().flatten() {
            thread::spawn(move || serve_connection(c));
        }
        return Ok(());
    }
    #[cfg(unix)]
    if let Some(path) = host.strip_prefix("unix://") {
        let _ = fs::remove_file(path);
        if let Some(dir) = std::path::Path::new(path).parent() {
            fs::create_dir_all(dir)?;
        }
        let l = std::os::unix::net::UnixListener::bind(path)?;
        eprintln!("docker engine API listening on {host}");
        for c in l.incoming().flatten() {
            thread::spawn(move || serve_connection(c));
        }
        return Ok(());
    }
    Err(format!("unsupported --host {host:?} (use unix:///path or tcp://host:port)").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn percent_decoding_and_queries() {
        assert_eq!(pct_decode("a%2Fb%3Ac+d%zz%"), "a/b:c d%zz%");
        let (p, q) = split_target("/v1.43/images/create?fromImage=localhost%3A5000%2Fapp&tag=v1");
        assert_eq!(p, "/images/create");
        assert_eq!((q["fromImage"].as_str(), q["tag"].as_str()), ("localhost:5000/app", "v1"));
        assert_eq!(split_target("/containers/json?all=1").0, "/containers/json");
        assert_eq!(split_target("/v1.24/_ping").0, "/_ping");
        assert_eq!(split_target("/volumes").0, "/volumes");
    }

    #[test]
    fn parses_requests_with_bodies() {
        let raw = b"POST /v1.43/containers/create?name=web HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"Image\":\"abc\"}GET /_ping HTTP/1.1\r\n\r\n";
        let mut r = BufReader::new(Cursor::new(&raw[..]));
        let a = read_request(&mut r).unwrap().unwrap();
        assert_eq!((a.method.as_str(), a.path.as_str(), a.query["name"].as_str()), ("POST", "/containers/create", "web"));
        assert_eq!(a.body, b"{\"Image\":\"abc\"}");
        let b = read_request(&mut r).unwrap().unwrap(); // keep-alive: a second request on the same connection
        assert_eq!((b.method.as_str(), b.path.as_str(), b.body.len()), ("GET", "/_ping", 0));
        assert!(read_request(&mut r).unwrap().is_none());
    }

    #[test]
    fn timestamps_ids_and_image_paths() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(fake_id("a").len(), 64);
        assert_eq!(fake_id("a"), fake_id("a"));
        assert_ne!(fake_id("a"), fake_id("b"));
        assert_eq!(image_name("/images/localhost:5000/team/app:v1/json", "/json"), Some("localhost:5000/team/app:v1"));
        assert_eq!(image_name("/images/json", "/json"), None);
    }

    #[test]
    fn chunked_request_bodies_are_decoded() {
        let raw = [
            "POST /build HTTP/1.1", "Transfer-Encoding: chunked", "", "5", "hello", "6;x=y", " world", "0", "", "GET /_ping HTTP/1.1", "", "",
        ]
        .join("\r\n");
        let mut r = io::BufReader::new(raw.as_bytes());
        assert_eq!(read_request(&mut r).unwrap().unwrap().body, b"hello world");
        assert_eq!(read_request(&mut r).unwrap().unwrap().path, "/_ping");
    }

    #[test]
    fn images_resolve_by_id_or_prefix() {
        let imgs = ["alpine:latest".to_string(), "node:alpine".to_string()];
        let id = fake_id("node:alpine");
        assert_eq!(image_by_id(&format!("sha256:{id}"), &imgs), "node:alpine");
        assert_eq!(image_by_id(&id[..12], &imgs), "node:alpine");
        assert_eq!(image_by_id("alpine", &imgs), "alpine"); // names pass through
        assert_eq!(image_by_id("abc", &imgs), "abc");
    }

    #[test]
    fn exec_bodies_become_argv() {
        let a = exec_argv(&json!({"Cmd": ["ls", "-l"], "Env": ["A=1"], "WorkingDir": "/srv"})).unwrap();
        assert_eq!(a, ["sh", "-c", "cd \"$0\" && exec \"$@\"", "/srv", "env", "A=1", "ls", "-l"]);
        assert_eq!(exec_argv(&json!({"Cmd": ["id"]})).unwrap(), ["id"]);
        assert!(exec_argv(&json!({})).is_err());
    }

    #[test]
    fn event_log_lines_become_docker_events() {
        let die = event_json(r#"{"time":5,"action":"die","id":"c1","name":"web","image":"alpine","exitCode":3}"#).unwrap();
        assert_eq!((die["Action"].as_str(), die["Actor"]["Attributes"]["exitCode"].as_str(), die["time"].as_u64()), (Some("die"), Some("3"), Some(5)));
        assert_eq!(die["Actor"]["Attributes"]["name"], "web");
        let start = event_json(r#"{"time":6,"action":"start","id":"c1","name":null,"image":"alpine","exitCode":null}"#).unwrap();
        assert_eq!(start["Actor"]["Attributes"]["name"], "");
        assert!(event_json("not json").is_none());
    }

    #[test]
    fn build_contexts_unpack_plain_and_gzipped() {
        let dir = std::env::temp_dir().join(format!("rd-api-ctx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut b = tar::Builder::new(vec![]);
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o644);
        b.append_data(&mut h, "Dockerfile", &b"FROM x"[..4]).ok();
        let plain = b.into_inner().unwrap();
        unpack_context(&plain, &dir).unwrap();
        assert!(dir.join("Dockerfile").is_file());
        let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
        gz.write_all(&plain).unwrap();
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        unpack_context(&gz.finish().unwrap(), &dir).unwrap();
        assert!(dir.join("Dockerfile").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_body_maps_onto_a_config() {
        let body = json!({
            "Image": "alpine:3.19", "Cmd": ["sleep", "5"], "Env": ["A=1"], "WorkingDir": "/srv", "User": "1000", "Tty": true,
            "Entrypoint": ["/bin/sh", "-c"],
            "HostConfig": {
                "Binds": ["/data:/data:ro", "vol1:/v", "/x:/y"], "PortBindings": {"80/tcp": [{"HostPort": "8080"}]},
                "Memory": 268435456u64, "NanoCpus": 500000000u64, "RestartPolicy": {"Name": "on-failure", "MaximumRetryCount": 3},
                "Privileged": false, "CapAdd": ["NET_ADMIN"], "CapDrop": ["ALL"], "NetworkMode": "appnet"
            },
            "NetworkingConfig": {"EndpointsConfig": {"appnet": {"Aliases": ["web"]}}},
            "Healthcheck": {"Test": ["CMD-SHELL", "true"], "Interval": 5000000000u64, "Retries": 2},
        });
        let c = config_from_create(&body, 2).unwrap();
        assert_eq!((c.target.as_str(), c.entrypoint.as_deref(), c.cmd.as_slice()), ("alpine:3.19", Some("/bin/sh"), ["-c", "sleep", "5"].map(String::from).as_slice()));
        assert_eq!((c.env.as_slice(), c.workdir.as_deref(), c.user.as_deref(), c.tty), (["A=1".to_string()].as_slice(), Some("/srv"), Some("1000"), true));
        assert_eq!(c.volumes.len(), 3);
        assert!(c.volumes[0].ro && !c.volumes[1].ro && c.volumes[1].is_named());
        assert_eq!(c.ports, [Port { host: 8080, container: 80 }]);
        assert_eq!((c.mem_bytes, c.cpu_percent, c.restart), (Some(268435456), Some(25), Restart::OnFailure(3)));
        assert_eq!((c.cap_add.as_slice(), c.cap_drop.as_slice()), (["NET_ADMIN".to_string()].as_slice(), ["ALL".to_string()].as_slice()));
        assert_eq!((c.network_name.as_deref(), c.aliases.as_slice()), (Some("appnet"), ["web".to_string()].as_slice()));
        let h = c.health.unwrap();
        assert_eq!((h.test[2].as_str(), h.interval_s, h.retries), ("true", 5, 2));
        assert!(c.detach);
        assert!(config_from_create(&json!({}), 1).is_err());
        assert!(config_from_create(&json!({"Image": "a", "HostConfig": {"PortBindings": {"53/udp": [{"HostPort": "53"}]}}}), 1).is_err());
        assert!(config_from_create(&json!({"Image": "a", "HostConfig": {"NetworkMode": "container:abc"}}), 1).is_err());
    }
}
