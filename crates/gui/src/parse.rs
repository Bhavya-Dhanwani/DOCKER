//! Parsers for the fixed-width tables the `docker` CLI prints (see crates/cli/src/commands.rs).
use crate::format::{parse_hms, parse_size};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Running,
    Paused,
    Restarting,
    Created,
    Exited,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub command: String,
    pub state: State,
    pub health: Option<String>,
    pub uptime: Option<u64>,
    pub exit_code: Option<i32>,
    pub restarts: Option<u32>,
    pub ip: String,
    pub ports: String,
}

impl Container {
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }
    pub fn is_live(&self) -> bool {
        matches!(self.state, State::Running | State::Paused | State::Restarting)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub name: String,
    pub size: u64,
    pub layers: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Network {
    pub name: String,
    pub builtin: bool,
    pub containers: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Df {
    pub images: usize,
    pub image_bytes: u64,
    pub containers: usize,
    pub running: usize,
    pub volumes: usize,
    pub volume_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatRow {
    pub cpu: f32,
    pub mem_used: u64,
    pub mem_limit: Option<u64>,
    pub mem_pct: Option<f32>,
    pub pids: u32,
}

fn slice(chars: &[char], a: usize, b: usize) -> String {
    let b = b.min(chars.len());
    if a >= b { String::new() } else { chars[a..b].iter().collect::<String>().trim().to_string() }
}

fn inner_parens(t: &str) -> Option<&str> {
    t.strip_prefix('(')?.strip_suffix(')')
}

#[cfg_attr(not(test), allow(dead_code))]
struct Status<'a> {
    state: State,
    health: Option<String>,
    uptime: Option<u64>,
    exit: Option<i32>,
    restarts: Option<u32>,
    rest: Vec<&'a str>,
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_status(rest: &str) -> Option<Status<'_>> {
    let mut t = rest.split_whitespace().peekable();
    let first = t.next()?;
    let (mut health, mut uptime, mut exit, mut restarts) = (None, None, None, None);
    let state = match first {
        "Up" => {
            uptime = t.next().and_then(parse_hms);
            if let Some(h) = t.next_if(|x| x.starts_with('(')) {
                health = inner_parens(h).map(str::to_string);
            }
            State::Running
        }
        "Restarting" => {
            if let Some(n) = t.next_if(|x| x.starts_with('(')) {
                restarts = inner_parens(n).and_then(|n| n.parse().ok());
            }
            State::Restarting
        }
        "Exited" => {
            if let Some(n) = t.next_if(|x| x.starts_with('(')) {
                exit = inner_parens(n).and_then(|n| n.parse().ok());
            }
            State::Exited
        }
        "Created" => State::Created,
        _ => return None,
    };
    Some(Status { state, health, uptime, exit, restarts, rest: t.collect() })
}

/// The old fixed-width table (kept for its tests; the app reads the JSON rows now).
#[cfg_attr(not(test), allow(dead_code))]
/// `docker ps -a`. Columns are fixed-width (id 18, name 12, image 14, command 24); the status column
/// can overflow its width, so everything after the command is parsed by token.
pub fn parse_ps(out: &str) -> Vec<Container> {
    out.lines()
        .filter(|l| !l.starts_with("CONTAINER") && !l.trim().is_empty())
        .filter_map(|line| {
            let c: Vec<char> = line.chars().collect();
            let tail = slice(&c, 72, usize::MAX);
            let s = parse_status(&tail)?;
            let is_ip = |t: &str| t.contains('.') && t.chars().all(|ch| ch.is_ascii_digit() || ch == '.');
            let (ip, ports) = match s.rest.first() {
                Some(t) if is_ip(t) => (t.to_string(), s.rest[1..].join(",")),
                _ => (String::new(), s.rest.join(",")),
            };
            Some(Container {
                id: slice(&c, 0, 18),
                name: slice(&c, 19, 31),
                image: slice(&c, 32, 46),
                command: slice(&c, 47, 71),
                state: s.state,
                health: s.health,
                uptime: s.uptime,
                exit_code: s.exit,
                restarts: s.restarts,
                ip,
                ports,
            })
        })
        .collect()
}

/// `docker ps -a --format '{{json .}}'`: one JSON object per line (Docker's fields plus a few the app needs).
pub fn parse_ps_json(out: &str) -> Vec<Container> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
            let state = match v["State"].as_str()? {
                "running" => State::Running,
                "paused" => State::Paused,
                "restarting" => State::Restarting,
                "created" => State::Created,
                _ => State::Exited,
            };
            let health = v["Health"].as_str().map(str::to_string);
            Some(Container {
                id: s("Id"),
                name: s("Names"),
                image: s("Image"),
                command: s("FullCommand"),
                state,
                health,
                uptime: (state == State::Running).then(|| v["UptimeSeconds"].as_u64()).flatten(),
                exit_code: v["ExitCode"].as_i64().map(|c| c as i32),
                restarts: (state == State::Restarting).then(|| v["Restarts"].as_u64().map(|n| n as u32)).flatten(),
                ip: s("IP"),
                ports: s("PortsRaw"),
            })
        })
        .collect()
}

/// `docker images --format '{{json .}}'`.
pub fn parse_images_json(out: &str) -> Vec<Image> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| Image { name: v["Name"].as_str().unwrap_or("").to_string(), size: v["SizeBytes"].as_u64().unwrap_or(0), layers: v["Layers"].as_u64().unwrap_or(0) as usize })
        .collect()
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_images(out: &str) -> Vec<Image> {
    out.lines()
        .skip(1)
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            (t.len() >= 3).then(|| Image { name: t[0].into(), size: parse_size(t[t.len() - 2]), layers: t[t.len() - 1].parse().unwrap_or(0) })
        })
        .collect()
}

pub fn parse_volumes(out: &str) -> Vec<Volume> {
    out.lines()
        .skip(1)
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            (t.len() >= 2).then(|| Volume { name: t[0].into(), size: parse_size(t[t.len() - 1]) })
        })
        .collect()
}

pub fn parse_networks(out: &str) -> Vec<Network> {
    out.lines()
        .skip(1)
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            let name = *t.first()?;
            let builtin = l.starts_with("bridge (default)");
            Some(Network { name: name.into(), builtin, containers: t.last().and_then(|n| n.parse().ok()).unwrap_or(0) })
        })
        .collect()
}

/// `docker system df`.
pub fn parse_df(out: &str) -> Df {
    let mut d = Df::default();
    for l in out.lines() {
        let t: Vec<&str> = l.split_whitespace().collect();
        let n = |i: usize| t.get(i).and_then(|x| x.parse::<usize>().ok()).unwrap_or(0);
        match t.first().copied() {
            Some("Images") => (d.images, d.image_bytes) = (n(1), t.get(3).map_or(0, |s| parse_size(s))),
            Some("Containers") => (d.containers, d.running) = (n(1), n(2)),
            Some("Volumes") => (d.volumes, d.volume_bytes) = (n(1), t.get(3).map_or(0, |s| parse_size(s))),
            _ => {}
        }
    }
    d
}

/// `docker stats --no-stream <id>`; the name column may be empty so columns are counted from the right.
pub fn parse_stats(out: &str) -> Option<StatRow> {
    let t: Vec<&str> = out.lines().nth(1)?.split_whitespace().collect();
    let n = t.len();
    if n < 6 {
        return None;
    }
    let pct = |s: &str| s.trim_end_matches('%').parse::<f32>().ok();
    let limit = parse_size(t[n - 3]);
    Some(StatRow {
        cpu: pct(t[n - 6])?,
        mem_used: parse_size(t[n - 5]),
        mem_limit: (limit > 0).then_some(limit),
        mem_pct: pct(t[n - 2]),
        pids: t[n - 1].parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str, image: &str, cmd: &str, status: &str) -> String {
        format!("{id:<18} {name:<12} {image:<14} {cmd:<24} {status}")
    }

    #[test]
    fn ps_rows() {
        let ps = [
            "CONTAINER ID       NAME         IMAGE          COMMAND                  STATUS".to_string(),
            row("0123456789abcdef", "web", "nginx:latest", "nginx", "Up 00:01:05 (healthy) 10.0.0.2 8080->80,443->443"),
            row("fedcba9876543210", "job", "alpine:latest", "echo hi", "Exited (0)"),
            row("1111111111111111", "", "alpine:latest", "sleep 1", "Created"),
            row("2222222222222222", "flaky", "busybox", "false", "Restarting (3)"),
        ]
        .join("\n");
        let v = parse_ps(&ps);
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].name, "web");
        assert_eq!(v[0].image, "nginx:latest");
        assert_eq!(v[0].state, State::Running);
        assert_eq!(v[0].health.as_deref(), Some("healthy"));
        assert_eq!(v[0].uptime, Some(65));
        assert_eq!(v[0].ip, "10.0.0.2");
        assert_eq!(v[0].ports, "8080->80,443->443");
        assert_eq!((v[1].state, v[1].exit_code), (State::Exited, Some(0)));
        assert_eq!(v[1].ip, "");
        assert_eq!(v[2].label(), "1111111111111111");
        assert_eq!(v[2].state, State::Created);
        assert_eq!((v[3].state, v[3].restarts), (State::Restarting, Some(3)));
        assert!(v[3].is_live());
    }

    #[test]
    fn ps_json_rows() {
        let rows = [
            r#"{"Id":"0123456789abcdef","Names":"web","Image":"nginx:latest","FullCommand":"nginx","State":"running","Health":"healthy","UptimeSeconds":65,"ExitCode":null,"Restarts":0,"IP":"10.0.0.2","PortsRaw":"8080->80"}"#,
            r#"{"Id":"fedcba9876543210","Names":"job","Image":"alpine:latest","FullCommand":"echo hi","State":"exited","ExitCode":0,"Restarts":0,"IP":"","PortsRaw":""}"#,
            r#"{"Id":"2222222222222222","Names":"flaky","Image":"busybox","State":"restarting","Restarts":3,"ExitCode":null}"#,
            "not json",
        ]
        .join("
");
        let v = parse_ps_json(&rows);
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].state, v[0].uptime, v[0].health.as_deref(), v[0].ip.as_str()), (State::Running, Some(65), Some("healthy"), "10.0.0.2"));
        assert_eq!((v[1].state, v[1].exit_code, v[1].uptime), (State::Exited, Some(0), None));
        assert_eq!((v[2].state, v[2].restarts), (State::Restarting, Some(3)));
        let img = parse_images_json(r#"{"Name":"alpine:latest","SizeBytes":4000000,"Layers":1}"#);
        assert_eq!(img, vec![Image { name: "alpine:latest".into(), size: 4_000_000, layers: 1 }]);
    }

    #[test]
    fn ps_empty_and_garbage() {
        assert!(parse_ps("CONTAINER ID  NAME\n").is_empty());
        assert!(parse_ps("docker: something odd\n").is_empty());
    }

    #[test]
    fn lists() {
        let imgs = parse_images("IMAGE SIZE LAYERS\nalpine:latest   4.0MB   1\n");
        assert_eq!(imgs, vec![Image { name: "alpine:latest".into(), size: 4 * 1024 * 1024, layers: 1 }]);
        let v = parse_volumes("VOLUME NAME   SIZE\ndata   1.5MB\n");
        assert_eq!(v[0].name, "data");
        let n = parse_networks("NETWORK CONTAINERS\nbridge (default)   -\nfront   2\n");
        assert!(n[0].builtin && n[0].name == "bridge");
        assert_eq!((n[1].name.as_str(), n[1].containers, n[1].builtin), ("front", 2, false));
    }

    #[test]
    fn df_and_stats() {
        let d = parse_df("TYPE TOTAL ACTIVE SIZE\nImages 3 - 10.0MB\nContainers 5 2 -\nVolumes 1 0 2.0MB\n");
        assert_eq!((d.images, d.containers, d.running, d.volumes), (3, 5, 2, 1));
        assert_eq!(d.image_bytes, 10 * 1024 * 1024);
        let s = parse_stats("CONTAINER ID NAME CPU % MEM USAGE / LIMIT MEM % PIDS\nabc   web   12.5%   8.0MB / 128.0MB   6.2%   3\n").unwrap();
        assert_eq!((s.cpu, s.pids, s.mem_limit), (12.5, 3, Some(128 * 1024 * 1024)));
        let s = parse_stats("H\nabc   0.0%   1.0MB / -   -   1\n").unwrap();
        assert_eq!((s.mem_limit, s.mem_pct), (None, None));
        assert!(parse_stats("H\n").is_none());
    }
}
