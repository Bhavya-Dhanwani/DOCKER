//! `docker compose`: a compose file -> networks, volumes and containers, using the same engine as `docker run`.
//! Supported service keys: image, build, command, entrypoint, environment, ports, volumes, depends_on
//! (including `condition: service_healthy`), networks, restart, container_name, healthcheck, cap_add, cap_drop,
//! privileged, mem_limit, working_dir, user. Anything else is reported and ignored.
use crate::commands;
use docker_core::{
    cli::{BuildArgs, ComposeArgs, ComposeSub, Config, Network, Port, Restart, Volume},
    networks, platform, registry, volumes,
};
use docker_image::{dockerfile::{parse_duration, subst, words}, Health, Reference, Store};
use serde::Deserialize;
use serde_yaml::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    error::Error,
    fs, io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

type Res<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Deserialize, Default)]
#[serde(default)]
struct File {
    name: Option<String>,
    services: BTreeMap<String, Service>,
    volumes: BTreeMap<String, Value>,
    networks: BTreeMap<String, Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Service {
    image: Option<String>,
    build: Option<Value>,
    command: Option<Value>,
    entrypoint: Option<Value>,
    environment: Option<Value>,
    env_file: Option<Value>,
    hostname: Option<String>,
    /// `["name:ip", ...]` or `{name: ip}`.
    extra_hosts: Option<Value>,
    ports: Vec<Value>,
    volumes: Vec<Value>,
    depends_on: Option<Value>,
    networks: Option<Value>,
    restart: Option<String>,
    container_name: Option<String>,
    healthcheck: Option<Value>,
    cap_add: Vec<String>,
    cap_drop: Vec<String>,
    privileged: bool,
    mem_limit: Option<Value>,
    working_dir: Option<String>,
    user: Option<String>,
    /// Only read by `docker stack deploy` (see `stack_services`).
    deploy: Option<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// A compose file loaded for a project.
struct Project {
    name: String,
    dir: PathBuf,
    file: File,
}

fn find_file(arg: &Option<String>) -> Res<PathBuf> {
    if let Some(f) = arg {
        return Ok(PathBuf::from(f));
    }
    ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| "no compose file found (compose.yaml, docker-compose.yml); use -f".into())
}

/// `${VAR}` / `${VAR:-default}` from the environment and a `.env` file next to the compose file.
fn interpolate(text: &str, dir: &Path) -> String {
    let mut vars: HashMap<String, String> = std::env::vars().collect();
    if let Ok(env) = fs::read_to_string(dir.join(".env")) {
        for l in env.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if let Some((k, v)) = l.split_once('=') {
                vars.entry(k.trim().to_string()).or_insert_with(|| v.trim().trim_matches(['"', '\'']).to_string());
            }
        }
    }
    // `$$` is an escaped `$` (`command: echo $$HOME` must reach the container as `$HOME`)
    text.lines()
        .map(|l| if l.trim_start().starts_with('#') { l.to_string() } else { subst(&l.replace("$$", "\u{0}"), &vars).replace('\u{0}', "$") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `dir.join(rel)` without the `.` components (`./site` -> `<dir>/site`).
fn clean_join(dir: &Path, rel: &Path) -> PathBuf {
    rel.components().filter(|c| !matches!(c, std::path::Component::CurDir)).fold(dir.to_path_buf(), |acc, c| acc.join(c))
}

fn sanitize(s: &str) -> String {
    let t: String = s.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || "_-".contains(*c)).collect();
    t.trim_start_matches(['_', '-']).to_string()
}

fn load(args: &ComposeArgs) -> Res<Project> {
    let path = find_file(&args.file)?;
    let abs = fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let dir = abs.parent().map(Path::to_path_buf).unwrap_or_default();
    let text = interpolate(&fs::read_to_string(&abs)?, &dir);
    // YAML anchors with merge keys (`<<: *defaults`) are common in real compose files
    let mut doc: Value = serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    doc.apply_merge().map_err(|e| format!("{}: {e}", path.display()))?;
    let file: File = serde_yaml::from_value(doc).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = args
        .project
        .clone()
        .or_else(|| file.name.clone())
        .or_else(|| dir.file_name().map(|n| sanitize(&n.to_string_lossy())))
        .filter(|n| !n.is_empty())
        .ok_or("cannot work out a project name; use -p")?;
    if file.services.is_empty() {
        return Err("the compose file has no services".into());
    }
    Ok(Project { name, dir, file })
}

fn string_list(v: &Value) -> Vec<String> {
    match v {
        Value::Sequence(s) => s.iter().filter_map(scalar).collect(),
        other => scalar(other).into_iter().collect(),
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `command:` / `entrypoint:` as an argv (a string is split like a shell would).
fn argv(v: &Value) -> Res<Vec<String>> {
    match v {
        Value::String(s) => Ok(words(s).map_err(|e| e.to_string())?),
        Value::Sequence(_) => Ok(string_list(v)),
        _ => Err("expected a string or a list".into()),
    }
}

fn environment(v: &Value) -> Vec<String> {
    match v {
        Value::Mapping(m) => m
            .iter()
            .filter_map(|(k, v)| {
                let k = scalar(k)?;
                Some(match v {
                    Value::Null => k, // pass the host's value through
                    other => format!("{k}={}", scalar(other).unwrap_or_default()),
                })
            })
            .collect(),
        other => string_list(other),
    }
}

/// `env_file:` as (path, required) pairs: a path, a list of paths, or a list of `{path, required}`.
fn env_files(v: &Value) -> Vec<(String, bool)> {
    let one = |x: &Value| match x {
        Value::Mapping(_) => x.get("path").and_then(scalar).map(|p| (p, x.get("required").and_then(Value::as_bool).unwrap_or(true))),
        other => scalar(other).map(|p| (p, true)),
    };
    match v {
        Value::Sequence(s) => s.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

/// `extra_hosts:` as `name:ip` entries: a list (`"name:ip"` or `"name=ip"`) or a mapping.
fn extra_hosts(v: &Value) -> Vec<String> {
    match v {
        Value::Mapping(m) => m.iter().filter_map(|(k, v)| Some(format!("{}:{}", scalar(k)?, scalar(v)?))).collect(),
        other => string_list(other).into_iter().map(|s| s.replacen('=', ":", 1)).collect(),
    }
}

/// A `volumes:` entry of a service as `source:target[:ro]`; the long form is `{type, source, target, read_only}`.
fn volume_str(v: &Value) -> Res<String> {
    match v {
        Value::Mapping(_) => {
            let get = |k: &str| v.get(k).and_then(scalar);
            let target = get("target").ok_or("a volume needs a target")?;
            let kind = get("type").unwrap_or_else(|| "volume".into());
            if kind == "tmpfs" {
                return Err("tmpfs volumes are not supported yet".into());
            }
            let source = get("source").ok_or("a volume needs a source")?;
            Ok(format!("{source}:{target}{}", if v.get("read_only").and_then(Value::as_bool).unwrap_or(false) { ":ro" } else { "" }))
        }
        other => scalar(other).ok_or_else(|| "unsupported volume entry".into()),
    }
}

fn mem_mb(v: &Value) -> Res<u64> {
    let s = scalar(v).ok_or("mem_limit must be a number or like 512m")?.to_lowercase();
    let (num, mult) = match s.trim_end_matches('b').chars().last() {
        Some('k') => (&s[..s.trim_end_matches('b').len() - 1], 1.0 / 1024.0),
        Some('m') => (&s[..s.trim_end_matches('b').len() - 1], 1.0),
        Some('g') => (&s[..s.trim_end_matches('b').len() - 1], 1024.0),
        _ => (s.trim_end_matches('b'), 1.0 / (1024.0 * 1024.0)),
    };
    let mb = (num.trim().parse::<f64>().map_err(|_| format!("bad mem_limit {s:?}"))? * mult).ceil() as u64;
    Ok(mb.max(1))
}

fn port(v: &Value) -> Res<Option<Port>> {
    let s = scalar(v).ok_or("unsupported port entry")?;
    let s = s.split('/').next().unwrap_or("");
    if v.as_str().is_some_and(|x| x.contains("/udp")) {
        return Err("UDP ports are not supported".into());
    }
    let parts: Vec<&str> = s.split(':').collect();
    Ok(match parts.as_slice() {
        [c] if c.parse::<u16>().is_ok() => None, // "80": a random host port, not supported
        [h, c] | [_, h, c] => Some(Port::parse(&format!("{h}:{c}"))?),
        _ => return Err(format!("unsupported port {s:?}").into()),
    })
}

fn health(v: &Value) -> Res<Option<Health>> {
    if v.get("disable").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let test = match v.get("test") {
        Some(Value::String(s)) => vec!["/bin/sh".into(), "-c".into(), s.clone()],
        Some(t @ Value::Sequence(_)) => {
            let l = string_list(t);
            match l.first().map(String::as_str) {
                Some("CMD-SHELL") => vec!["/bin/sh".into(), "-c".into(), l[1..].join(" ")],
                Some("CMD") => l[1..].to_vec(),
                Some("NONE") => return Ok(None),
                _ => l,
            }
        }
        _ => return Err("healthcheck needs a test".into()),
    };
    let mut h = Health::new(test);
    let secs = |k: &str, d: u64| v.get(k).and_then(scalar).map(|s| parse_duration(&s).ok_or(format!("bad {k}"))).transpose().map(|o| o.unwrap_or(d));
    h.interval_s = secs("interval", h.interval_s)?;
    h.timeout_s = secs("timeout", h.timeout_s)?;
    h.start_period_s = secs("start_period", 0)?;
    h.retries = v.get("retries").and_then(Value::as_u64).map_or(h.retries, |n| n as u32);
    Ok(Some(h))
}

/// The container a service becomes, plus the image it needs and what must be running first.
struct Plan {
    service: String,
    container: String,
    cfg: Config,
    build: Option<BuildArgs>,
    deps: Vec<(String, bool)>, // (service, wait until healthy)
}

fn container_name(p: &Project, svc: &str, s: &Service) -> String {
    s.container_name.clone().unwrap_or_else(|| format!("{}-{svc}-1", p.name))
}

fn plan(p: &Project, name: &str, s: &Service) -> Res<Plan> {
    let net_of = |n: &str| if n == "default" || !p.file.networks.contains_key(n) { format!("{}_default", p.name) } else { format!("{}_{n}", p.name) };
    let network = match &s.networks {
        Some(Value::Mapping(m)) => m.keys().filter_map(scalar).next().map(|n| net_of(&n)),
        Some(v) => string_list(v).first().map(|n| net_of(n)),
        None => None,
    }
    .unwrap_or_else(|| net_of("default"));

    let mut cfg = Config {
        detach: true,
        name: Some(container_name(p, name, s)),
        network: Some(Network::Bridge),
        network_name: Some(network),
        aliases: vec![name.to_string()],
        cap_add: s.cap_add.clone(),
        cap_drop: s.cap_drop.clone(),
        privileged: s.privileged,
        workdir: s.working_dir.clone(),
        user: s.user.clone(),
        hostname: s.hostname.clone(),
        extra_hosts: s.extra_hosts.as_ref().map(extra_hosts).unwrap_or_default(),
        ..Config::default()
    };
    if let Some(r) = &s.restart {
        cfg.restart = Restart::parse(r)?;
    }
    if let Some(m) = &s.mem_limit {
        cfg.mem_bytes = Some(mem_mb(m)? << 20);
    }
    // env_file first: `environment:` entries win over it
    for (path, required) in s.env_file.as_ref().map(env_files).unwrap_or_default() {
        let full = clean_join(&p.dir, Path::new(&path));
        match fs::read_to_string(&full) {
            Ok(t) => cfg.env.extend(docker_core::env::parse_env_file(&t)),
            Err(_) if !required => {}
            Err(e) => return Err(format!("env_file {}: {e}", full.display()).into()),
        }
    }
    if let Some(e) = &s.environment {
        cfg.env.extend(environment(e));
    }
    for pv in &s.ports {
        cfg.ports.extend(port(pv)?);
    }
    for v in &s.volumes {
        let v = &volume_str(v).map_err(|e| format!("service {name}: {e}"))?;
        // `- /app/node_modules`: an anonymous volume, kept per service and path
        let anon;
        let v: &str = if v.starts_with('/') && !v.contains(':') {
            anon = format!("{name}_{}:{v}", docker_core::cli::anon_volume_name(v));
            &anon
        } else {
            v
        };
        let (src, rest) = v.split_once(':').ok_or_else(|| format!("service {name}: volume {v:?} needs a destination"))?;
        let host = if src.starts_with('.') || src.starts_with('/') {
            let q = Path::new(src);
            if src.starts_with('/') || q.is_absolute() { src.to_string() } else { clean_join(&p.dir, q).to_string_lossy().into_owned() }
        } else {
            format!("{}_{src}", p.name) // a named volume of this project
        };
        cfg.volumes.push(Volume::parse(&format!("{host}:{rest}"))?);
    }
    if let Some(h) = &s.healthcheck {
        cfg.health = health(h)?;
        cfg.no_healthcheck = cfg.health.is_none();
    }
    let mut command = s.command.as_ref().map(argv).transpose()?.unwrap_or_default();
    if let Some(ep) = &s.entrypoint {
        let mut e = argv(ep)?;
        if !e.is_empty() {
            cfg.entrypoint = Some(e.remove(0));
            e.extend(command);
            command = e; // the rest of the entrypoint list becomes leading arguments
        }
    }
    cfg.cmd = command;

    let build = match &s.build {
        None => None,
        Some(b) => {
            let (context, dockerfile, args) = match b {
                Value::String(c) => (c.clone(), None, vec![]),
                m => (
                    m.get("context").and_then(scalar).unwrap_or_else(|| ".".into()),
                    m.get("dockerfile").and_then(scalar),
                    m.get("args").map(environment).unwrap_or_default().iter().filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect(),
                ),
            };
            let ctx = clean_join(&p.dir, Path::new(&context));
            Some(BuildArgs {
                tag: format!("{}-{name}", p.name),
                file: dockerfile.map(|d| ctx.join(d).to_string_lossy().into_owned()),
                args,
                context: ctx.to_string_lossy().into_owned(),
                ..BuildArgs::default()
            })
        }
    };
    cfg.target = match (&build, &s.image) {
        (Some(b), _) => b.tag.clone(),
        (None, Some(i)) => i.clone(),
        (None, None) => return Err(format!("service {name} needs an image or a build").into()),
    };
    let deps = match &s.depends_on {
        None => vec![],
        Some(Value::Mapping(m)) => m
            .iter()
            .filter_map(|(k, v)| Some((scalar(k)?, v.get("condition").and_then(Value::as_str) == Some("service_healthy"))))
            .collect(),
        Some(v) => string_list(v).into_iter().map(|d| (d, false)).collect(),
    };
    Ok(Plan { service: name.to_string(), container: container_name(p, name, s), cfg, build, deps })
}

/// Services in dependency order (dependencies first); errors on unknown names and cycles.
fn order(plans: &BTreeMap<String, Plan>) -> Res<Vec<String>> {
    let mut done: Vec<String> = vec![];
    let mut visiting = BTreeSet::new();
    fn visit(n: &str, plans: &BTreeMap<String, Plan>, done: &mut Vec<String>, visiting: &mut BTreeSet<String>) -> Res {
        if done.iter().any(|d| d == n) {
            return Ok(());
        }
        if !visiting.insert(n.to_string()) {
            return Err(format!("dependency cycle involving service {n}").into());
        }
        let p = plans.get(n).ok_or_else(|| format!("depends_on: unknown service {n}"))?;
        for (d, _) in &p.deps {
            visit(d, plans, done, visiting)?;
        }
        visiting.remove(n);
        done.push(n.to_string());
        Ok(())
    }
    for n in plans.keys() {
        visit(n, plans, &mut done, &mut visiting)?;
    }
    Ok(done)
}

fn plans(p: &Project) -> Res<BTreeMap<String, Plan>> {
    for (name, s) in &p.file.services {
        // harmless for a local run: metadata, or behaviour that only matters on a real orchestrator
        const IGNORED: &[&str] = &["labels", "stdin_open", "tty", "expose", "logging", "deploy", "init", "stop_grace_period", "platform", "pull_policy", "profiles", "container_labels"];
        let unknown: Vec<&String> = s.extra.keys().filter(|k| !IGNORED.contains(&k.as_str()) && !k.starts_with("x-")).collect();
        if !unknown.is_empty() {
            eprintln!("warning: service {name}: ignoring unsupported keys: {}", unknown.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "));
        }
    }
    p.file.services.iter().map(|(n, s)| Ok((n.clone(), plan(p, n, s).map_err(|e| format!("service {n}: {e}"))?))).collect()
}

fn project_containers(p: &Project) -> Vec<docker_core::registry::Entry> {
    let names: BTreeSet<String> = p.file.services.iter().map(|(n, s)| container_name(p, n, s)).collect();
    registry::list().into_iter().filter(|e| e.name.as_ref().is_some_and(|n| names.contains(n))).collect()
}

fn wait_healthy(container: &str, secs: u64) -> Res {
    let start = Instant::now();
    loop {
        let e = registry::find(container)?;
        match e.health.as_deref() {
            Some("healthy") => return Ok(()),
            Some("unhealthy") => return Err(format!("{container} is unhealthy").into()),
            None => return Err(format!("{container} has no healthcheck, so `condition: service_healthy` cannot be satisfied").into()),
            _ => {}
        }
        if !e.running() {
            return Err(format!("{container} exited before becoming healthy").into());
        }
        if start.elapsed() > Duration::from_secs(secs) {
            return Err(format!("{container} did not become healthy within {secs}s").into());
        }
        thread::sleep(Duration::from_millis(300));
    }
}

fn ensure_image(store: &Store, plan: &Plan, build: bool) -> Res {
    if let Some(b) = &plan.build {
        if build || store.get(&Reference::parse(&b.tag)?)?.is_none() {
            println!("Building {}", plan.service);
            platform::build(b, &mut |l| println!("  {l}"))?;
        }
    } else if Path::new(&plan.cfg.target).is_dir() || store.get(&Reference::parse(&plan.cfg.target)?)?.is_some() {
        // already here
    } else {
        println!("Pulling {}", plan.cfg.target);
        commands::pull(&plan.cfg.target)?;
    }
    Ok(())
}

fn up(p: &Project, detach: bool, build: bool) -> Res {
    let plans = plans(p)?;
    let order = order(&plans)?;
    let store = Store::open(registry::data_dir().join("store"));

    // networks and volumes first
    let mut nets: BTreeSet<String> = BTreeSet::from([format!("{}_default", p.name)]);
    nets.extend(p.file.networks.keys().map(|n| format!("{}_{n}", p.name)));
    for n in nets {
        if !networks::exists(&n) {
            networks::create(&n)?;
            println!("Network {n} created");
        }
    }
    for v in p.file.volumes.keys() {
        volumes::create(&format!("{}_{v}", p.name))?;
    }

    for name in &order {
        let pl = &plans[name];
        ensure_image(&store, pl, build)?;
        for (dep, healthy) in &pl.deps {
            if *healthy {
                println!("Waiting for {dep} to be healthy");
                wait_healthy(&plans[dep].container, 120)?;
            }
        }
        // replace an old container of the same name (like `up` does when the config changed)
        if let Ok(old) = registry::find(&pl.container) {
            if old.running() {
                println!("Container {} is already running", pl.container);
                continue;
            }
            registry::delete(&old.id);
        }
        print!("Container {} started: ", pl.container);
        platform::run(&pl.cfg)?; // prints the container id
    }
    if detach {
        return Ok(());
    }
    println!("Attached. Press Ctrl-C to stop watching (containers keep running; `docker compose down` removes them).");
    follow(p, None, true)
}

fn read_new(path: &Path, offset: &mut u64) -> String {
    let Ok(mut f) = fs::File::open(path) else { return String::new() };
    if f.seek(SeekFrom::Start(*offset)).is_err() {
        return String::new();
    }
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    *offset += s.len() as u64;
    s
}

/// Print the project's container logs with a `service | ` prefix; with `follow`, until all of them have exited.
fn follow(p: &Project, only: Option<&str>, follow: bool) -> Res {
    let mut targets: Vec<(String, registry::Entry, u64)> = vec![];
    for (svc, s) in &p.file.services {
        if only.is_some_and(|o| o != svc) {
            continue;
        }
        if let Ok(e) = registry::find(&container_name(p, svc, s)) {
            targets.push((svc.clone(), e, 0));
        }
    }
    if let Some(o) = only
        && targets.is_empty()
    {
        return Err(format!("no container for service {o}").into());
    }
    let width = targets.iter().map(|t| t.0.len()).max().unwrap_or(0);
    loop {
        for (svc, e, off) in &mut targets {
            for line in read_new(&registry::log_path(&e.id), off).lines() {
                println!("{svc:<width$} | {line}");
            }
        }
        let alive = targets.iter().any(|(_, e, _)| registry::find(&e.id).is_ok_and(|x| x.running() || x.restarting));
        if !follow || !alive {
            if !follow {
                return Ok(());
            }
            for (svc, e, off) in &mut targets {
                for line in read_new(&registry::log_path(&e.id), off).lines() {
                    println!("{svc:<width$} | {line}");
                }
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn down(p: &Project, remove_volumes: bool) -> Res {
    for e in project_containers(p) {
        if e.running() || e.restarting {
            commands::stop(e.label(), 10).or_else(|_| commands::kill(e.label()))?;
        }
        let _ = commands::rm(true, &e.id);
        println!("Container {} removed", e.label());
    }
    for n in networks::list().into_iter().filter(|n| n.starts_with(&format!("{}_", p.name))) {
        if networks::remove(&n).is_ok() {
            println!("Network {n} removed");
        }
    }
    if remove_volumes {
        for v in p.file.volumes.keys() {
            let name = format!("{}_{v}", p.name);
            if volumes::remove(&name).is_ok() {
                println!("Volume {name} removed");
            }
        }
    }
    Ok(())
}

fn ps(p: &Project) -> Res {
    println!("{:<28} {:<14} {:<22} {:<24}", "NAME", "SERVICE", "STATUS", "PORTS");
    let now = registry::now_unix();
    for (svc, s) in &p.file.services {
        let name = container_name(p, svc, s);
        match registry::find(&name) {
            Ok(e) => println!("{:<28} {:<14} {:<22} {:<24}", name, svc, commands::status_of(&e, now), e.ports.join(",")),
            Err(_) => println!("{name:<28} {svc:<14} {:<22}", "not created"),
        }
    }
    Ok(())
}

pub fn run(args: &ComposeArgs) -> Res {
    let p = load(args)?;
    match &args.sub {
        ComposeSub::Up { detach, build } => up(&p, *detach, *build),
        ComposeSub::Down { volumes } => down(&p, *volumes),
        ComposeSub::Ps => ps(&p),
        ComposeSub::Logs { follow: f, service } => follow(&p, service.as_deref(), *f),
        ComposeSub::Build => {
            let pl = plans(&p)?;
            for pl in pl.values().filter(|x| x.build.is_some()) {
                println!("Building {}", pl.service);
                platform::build(pl.build.as_ref().expect("filtered"), &mut |l| println!("  {l}"))?;
            }
            Ok(())
        }
        ComposeSub::Config => {
            let pl = plans(&p)?;
            let out: BTreeMap<_, _> = pl
                .iter()
                .map(|(n, x)| {
                    (
                        n.clone(),
                        serde_json::json!({
                            "container": x.container, "image": x.cfg.target, "command": x.cfg.cmd, "entrypoint": x.cfg.entrypoint,
                            "network": x.cfg.network_name, "ports": x.cfg.ports.iter().map(|p| format!("{}:{}", p.host, p.container)).collect::<Vec<_>>(),
                            "environment": x.cfg.env, "volumes": x.cfg.volumes.iter().map(|v| format!("{}:{}", v.host, v.container)).collect::<Vec<_>>(),
                            "restart": x.cfg.restart.to_arg(), "depends_on": x.deps.iter().map(|d| d.0.clone()).collect::<Vec<_>>(),
                            "build": x.build.as_ref().map(|b| b.context.clone()),
                        }),
                    )
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "project": p.name, "services": out }))?);
            Ok(())
        }
    }
}

/// The `deploy:` section of a service, as far as single-node swarm uses it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Deploy {
    pub replicas: Option<u32>,
    /// `restart_policy.condition`: any | on-failure | none.
    pub restart_condition: Option<String>,
    pub parallelism: Option<u32>,
    pub delay_s: Option<u64>,
    pub mem_mb: Option<u64>,
    pub cpu_percent: Option<u32>,
}

/// `deploy: {replicas, restart_policy, update_config, resources.limits}`.
pub fn deploy(v: &Value) -> Res<Deploy> {
    let mut d = Deploy {
        replicas: v.get("replicas").and_then(Value::as_u64).map(|n| n as u32),
        restart_condition: v.get("restart_policy").and_then(|r| r.get("condition")).and_then(scalar),
        ..Deploy::default()
    };
    if let Some(u) = v.get("update_config") {
        d.parallelism = u.get("parallelism").and_then(Value::as_u64).map(|n| n as u32);
        d.delay_s = u.get("delay").and_then(scalar).map(|s| parse_duration(&s).ok_or(format!("bad update_config.delay {s:?}"))).transpose()?;
    }
    if let Some(l) = v.get("resources").and_then(|r| r.get("limits")) {
        d.mem_mb = l.get("memory").map(mem_mb).transpose()?;
        // ponytail: `cpus` counts cores in Docker but cpu_percent is a share of the whole machine; cpus*100 clamped is a close-enough cap
        d.cpu_percent = l
            .get("cpus")
            .and_then(scalar)
            .map(|c| c.parse::<f64>().map(|f| (f * 100.0).round().clamp(1.0, 100.0) as u32).map_err(|_| format!("bad cpus {c:?}")))
            .transpose()?;
    }
    Ok(d)
}

/// One service of a stack file: its container settings and its `deploy:` section.
pub struct StackService {
    pub name: String,
    pub cfg: Config,
    pub deploy: Deploy,
}

/// Load a compose file for `docker stack deploy`. `build:` is not supported (swarm services need an image).
pub fn stack_services(file: &str, stack: &str) -> Res<Vec<StackService>> {
    let p = load(&ComposeArgs { file: Some(file.into()), project: Some(stack.into()), sub: ComposeSub::Config })?;
    let mut out = vec![];
    for (name, s) in &p.file.services {
        let pl = plan(&p, name, s)?;
        if pl.build.is_some() {
            return Err(format!("service {name}: build is not supported by stack deploy; use an image").into());
        }
        let deploy = s.deploy.as_ref().map(deploy).transpose()?.unwrap_or_default();
        out.push(StackService { name: name.clone(), cfg: pl.cfg, deploy });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_dollar_is_a_literal_dollar() {
        let out = interpolate("cmd: echo $$HOME ${SASTA_UNSET_VAR_X:-dflt}\n", Path::new("/nonexistent"));
        assert_eq!(out, "cmd: echo $HOME dflt");
    }

    #[test]
    fn env_file_forms_and_long_volume_syntax() {
        let y = |s: &str| serde_yaml::from_str::<Value>(s).unwrap();
        assert_eq!(env_files(&y("./a.env")), [("./a.env".to_string(), true)]);
        assert_eq!(env_files(&y("[a.env, {path: b.env, required: false}]")), [("a.env".to_string(), true), ("b.env".to_string(), false)]);
        assert_eq!(volume_str(&y("./x:/y:ro")).unwrap(), "./x:/y:ro");
        assert_eq!(volume_str(&y("{type: bind, source: ./x, target: /y, read_only: true}")).unwrap(), "./x:/y:ro");
        assert_eq!(volume_str(&y("{type: volume, source: data, target: /d}")).unwrap(), "data:/d");
        assert!(volume_str(&y("{type: tmpfs, target: /t}")).is_err());
    }

    #[test]
    fn yaml_merge_keys_apply() {
        let mut d: Value = serde_yaml::from_str("x: &d {image: redis, restart: always}\nservices:\n  a:\n    <<: *d\n    restart: no\n").unwrap();
        d.apply_merge().unwrap();
        let f: File = serde_yaml::from_value(d).unwrap();
        assert_eq!(f.services["a"].image.as_deref(), Some("redis"));
        assert_eq!(f.services["a"].restart.as_deref(), Some("no"));
    }

    #[test]
    fn deploy_section_maps_to_swarm_settings() {
        let y = "replicas: 3\nrestart_policy: {condition: on-failure}\nupdate_config: {parallelism: 2, delay: 10s}\nresources: {limits: {cpus: '0.5', memory: 128M}}";
        let d = deploy(&serde_yaml::from_str(y).unwrap()).unwrap();
        assert_eq!(
            d,
            Deploy { replicas: Some(3), restart_condition: Some("on-failure".into()), parallelism: Some(2), delay_s: Some(10), mem_mb: Some(128), cpu_percent: Some(50) }
        );
        assert_eq!(deploy(&serde_yaml::from_str("{}").unwrap()).unwrap(), Deploy::default());
        assert!(deploy(&serde_yaml::from_str("update_config: {delay: soon}").unwrap()).is_err());
    }

    fn project(yaml: &str) -> Project {
        Project { name: "demo".into(), dir: PathBuf::from("/proj"), file: serde_yaml::from_str(yaml).unwrap() }
    }

    fn one(yaml: &str, svc: &str) -> Plan {
        let p = project(yaml);
        plan(&p, svc, &p.file.services[svc]).unwrap()
    }

    #[test]
    fn maps_a_service_to_a_container_config() {
        let y = r#"
services:
  web:
    image: nginx
    command: ["nginx", "-g", "daemon off;"]
    environment: {A: "1", B: null}
    ports: ["8080:80", "127.0.0.1:9090:90", 7000]
    volumes: ["data:/var/lib/x", "./site:/site:ro", "/abs:/abs"]
    restart: unless-stopped
    mem_limit: 256m
    working_dir: /srv
    user: "1000"
    cap_drop: [ALL]
    depends_on: {db: {condition: service_healthy}, cache: {condition: service_started}}
    healthcheck: {test: ["CMD", "curl", "-f", "x"], interval: 5s, retries: 2}
volumes: {data: {}}
"#;
        let p = one(y, "web");
        assert_eq!(p.container, "demo-web-1");
        let c = &p.cfg;
        assert_eq!((c.target.as_str(), c.cmd.as_slice(), c.detach), ("nginx", ["nginx", "-g", "daemon off;"].map(String::from).as_slice(), true));
        assert_eq!(c.network_name.as_deref(), Some("demo_default"));
        assert_eq!(c.aliases, ["web"]);
        assert_eq!(c.env, ["A=1", "B"]);
        assert_eq!(c.ports.iter().map(|p| (p.host, p.container)).collect::<Vec<_>>(), [(8080, 80), (9090, 90)], "a bare port is dropped");
        assert_eq!(c.volumes[0], Volume::parse("demo_data:/var/lib/x").unwrap());
        assert_eq!(Path::new(&c.volumes[1].host), Path::new("/proj").join("site"));
        assert!(c.volumes[1].ro && c.volumes[2].host == "/abs");
        assert_eq!((c.restart, c.mem_bytes, c.workdir.as_deref(), c.user.as_deref()), (Restart::UnlessStopped, Some(256 << 20), Some("/srv"), Some("1000")));
        assert_eq!(c.cap_drop, ["ALL"]);
        let h = c.health.as_ref().unwrap();
        assert_eq!((h.test.as_slice(), h.interval_s, h.retries), (["curl", "-f", "x"].map(String::from).as_slice(), 5, 2));
        let mut deps = p.deps.clone();
        deps.sort();
        assert_eq!(deps, [("cache".to_string(), false), ("db".to_string(), true)]);
    }

    #[test]
    fn build_entrypoint_and_shell_style_values() {
        let y = "services:\n  app:\n    build: {context: ./app, dockerfile: Df, args: {V: \"2\"}}\n    entrypoint: [\"/bin/sh\", \"-c\"]\n    command: echo hi\n    healthcheck: {test: \"true\"}\n";
        let p = one(y, "app");
        let b = p.build.as_ref().unwrap();
        assert_eq!((b.tag.as_str(), b.args.as_slice()), ("demo-app", [("V".to_string(), "2".to_string())].as_slice()));
        assert!(b.context.ends_with("app") && b.file.as_ref().unwrap().ends_with("Df"));
        assert_eq!(p.cfg.target, "demo-app");
        assert_eq!(p.cfg.entrypoint.as_deref(), Some("/bin/sh"));
        assert_eq!(p.cfg.cmd, ["-c", "echo", "hi"], "the rest of the entrypoint list leads the command");
        assert_eq!(p.cfg.health.as_ref().unwrap().test, ["/bin/sh", "-c", "true"]);
    }

    #[test]
    fn named_networks_and_errors() {
        let y = "services:\n  a:\n    image: x\n    networks: [back]\n  b:\n    image: x\n    ports: [\"53:53/udp\"]\n  c: {}\nnetworks: {back: {}}\n";
        let p = project(y);
        assert_eq!(plan(&p, "a", &p.file.services["a"]).unwrap().cfg.network_name.as_deref(), Some("demo_back"));
        assert!(plan(&p, "b", &p.file.services["b"]).is_err(), "udp ports are rejected");
        assert!(plan(&p, "c", &p.file.services["c"]).is_err(), "no image and no build");
    }

    #[test]
    fn dependency_order_and_cycles() {
        let y = "services:\n  web: {image: x, depends_on: [api]}\n  api: {image: x, depends_on: [db]}\n  db: {image: x}\n";
        let pl = plans(&project(y)).unwrap();
        assert_eq!(order(&pl).unwrap(), ["db", "api", "web"]);
        let cyc = plans(&project("services:\n  a: {image: x, depends_on: [b]}\n  b: {image: x, depends_on: [a]}\n")).unwrap();
        assert!(order(&cyc).unwrap_err().to_string().contains("cycle"));
        let unknown = plans(&project("services:\n  a: {image: x, depends_on: [nope]}\n")).unwrap();
        assert!(order(&unknown).is_err());
    }

    #[test]
    fn interpolation_and_helpers() {
        let dir = std::env::temp_dir().join(format!("rd-compose-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // unlikely names: a variable that is already set in the environment wins over .env (as in Docker)
        fs::write(dir.join(".env"), "RD_TEST_TAG=1.25\n# c\nRD_TEST_NAME=\"quoted\"\n").unwrap();
        let out = interpolate("image: nginx:${RD_TEST_TAG}\nn: ${RD_TEST_NAME}\nd: ${RD_TEST_MISSING:-dflt}\n# ${RD_TEST_TAG} stays in comments", &dir);
        assert_eq!(out, "image: nginx:1.25\nn: quoted\nd: dflt\n# ${RD_TEST_TAG} stays in comments");
        assert_eq!(sanitize("My App_2!"), "myapp_2");
        assert_eq!(mem_mb(&Value::String("512m".into())).unwrap(), 512);
        assert_eq!(mem_mb(&Value::String("1g".into())).unwrap(), 1024);
        assert_eq!(mem_mb(&serde_yaml::from_str("1048576").unwrap()).unwrap(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
