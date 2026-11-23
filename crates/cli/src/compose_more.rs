//! The rest of `docker compose`: `up`/`down`/`ps`/`logs` with all their options, and `stop`, `start`, `restart`, `kill`,
//! `pause`, `unpause`, `rm`, `pull`, `push`, `top`, `port`, `images`, `ls`, `exec`, `run`, `cp`, `events`, `create`,
//! `version`, `config`. (Child module of `compose`: it uses the loader and the planner from there.)
use super::*;
use crate::api;
use crate::extra::{self, flags};
use serde_json::json;

/// The services a command is about: the named ones (switching on their profiles), or every enabled one.
fn pick(p: &mut Project, names: &[String]) -> Res<Vec<String>> {
    for n in names {
        let s = p.file.services.get(n).ok_or_else(|| format!("no such service: {n}"))?;
        for pr in &s.profiles {
            if !p.profiles.contains(pr) {
                p.profiles.push(pr.clone());
            }
        }
    }
    if names.is_empty() {
        return Ok(p.file.services.iter().filter(|(_, s)| s.profiles.is_empty() || s.profiles.iter().any(|x| p.profiles.contains(x))).map(|(n, _)| n.clone()).collect());
    }
    Ok(names.to_vec())
}

/// Existing containers of the services.
fn containers(p: &Project, services: &[String]) -> Vec<(String, registry::Entry)> {
    let mut out = vec![];
    for svc in services {
        if let Some(s) = p.file.services.get(svc)
            && let Ok(e) = registry::find(&container_name(p, svc, s))
        {
            out.push((svc.clone(), e));
        }
    }
    out
}

/// `services` plus everything they depend on, dependencies first.
fn with_deps(plans: &BTreeMap<String, Plan>, services: &[String]) -> Res<Vec<String>> {
    let all = order(plans)?;
    let mut need: BTreeSet<String> = services.iter().cloned().collect();
    loop {
        let before = need.len();
        for s in need.clone() {
            if let Some(pl) = plans.get(&s) {
                need.extend(pl.deps.iter().map(|d| d.0.clone()));
            }
        }
        if need.len() == before {
            break;
        }
    }
    Ok(all.into_iter().filter(|s| need.contains(s)).collect())
}

pub(super) struct UpOpts {
    pub detach: bool,
    pub build: bool,
    pub no_build: bool,
    pub force_recreate: bool,
    pub no_recreate: bool,
    pub no_start: bool,
    pub no_deps: bool,
    pub wait: bool,
    pub remove_orphans: bool,
    pub abort_on_exit: bool,
    pub services: Vec<String>,
}

fn stop_entry(e: &registry::Entry, secs: u64) {
    if e.running() || e.restarting {
        let _ = commands::stop(e.label(), secs).or_else(|_| commands::kill(e.label()));
    }
}

fn is_project_container(e: &registry::Entry, project: &str) -> bool {
    e.config.as_ref().is_some_and(|c| c.labels.iter().any(|l| *l == format!("com.docker.compose.project={project}")))
}

pub(super) fn up(p: &mut Project, o: &UpOpts) -> Res<i32> {
    let wanted = pick(p, &o.services)?;
    let plans = plans(p)?;
    let order = if o.no_deps { wanted.clone() } else { with_deps(&plans, &wanted)? };
    let store = Store::open(registry::data_dir().join("store"));

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
    if o.remove_orphans {
        let known: BTreeSet<String> = p.file.services.iter().map(|(n, s)| container_name(p, n, s)).collect();
        for e in registry::list() {
            if is_project_container(&e, &p.name) && e.name.as_ref().is_some_and(|n| !known.contains(n)) {
                stop_entry(&e, 10);
                let _ = commands::rm(true, &e.id);
                println!("Container {} removed (orphan)", e.label());
            }
        }
    }
    for name in &order {
        let pl = &plans[name];
        if !o.no_build {
            ensure_image(&store, pl, o.build)?;
        }
        for (dep, healthy) in &pl.deps {
            if *healthy && order.contains(dep) && !o.no_start {
                println!("Waiting for {dep} to be healthy");
                wait_healthy(&plans[dep].container, 120)?;
            }
        }
        if let Ok(old) = registry::find(&pl.container) {
            if o.no_recreate || (!o.force_recreate && (old.running() || o.no_start)) {
                println!("Container {} is already {}", pl.container, if old.running() { "running" } else { "created" });
                continue;
            }
            stop_entry(&old, 10);
            registry::delete(&old.id);
        }
        if o.no_start {
            let e = registry::create(&pl.cfg)?;
            println!("Container {} created: {}", pl.container, e.id);
        } else {
            print!("Container {} started: ", pl.container);
            platform::run(&pl.cfg)?;
        }
    }
    if o.wait && !o.no_start {
        for name in &order {
            let c = &plans[name].container;
            if registry::find(c).is_ok_and(|e| e.health.is_some()) {
                wait_healthy(c, 300)?;
            }
        }
    }
    if o.detach || o.no_start {
        return Ok(0);
    }
    if o.abort_on_exit {
        println!("Attached. All containers stop as soon as one of them exits.");
        let mut offsets: HashMap<String, u64> = HashMap::new();
        let width = order.iter().map(String::len).max().unwrap_or(0);
        loop {
            let live = containers(p, &order);
            for (svc, e) in &live {
                let off = offsets.entry(e.id.clone()).or_insert(0);
                for line in read_new(&registry::log_path(&e.id), off).lines() {
                    println!("{svc:<width$} | {line}");
                }
            }
            if let Some((svc, e)) = live.iter().find(|(_, e)| !e.running() && !e.restarting && !e.created) {
                let code = e.exit_code.unwrap_or(0);
                println!("{svc} exited with code {code}; stopping the others");
                for (_, x) in &live {
                    stop_entry(x, 10);
                }
                return Ok(code);
            }
            thread::sleep(Duration::from_millis(300));
        }
    }
    println!("Attached. Press Ctrl-C to stop watching (containers keep running; `docker compose down` removes them).");
    follow(p, None, true)?;
    Ok(0)
}

fn down(p: &mut Project, f: &extra::Flags) -> Res<i32> {
    let secs = f.get(&["t", "timeout"]).and_then(|t| t.parse().ok()).unwrap_or(10);
    let plans = plans(p).ok();
    let mut entries = project_containers(p);
    // containers of the project made by `run`, or by services that have since left the file
    if f.has(&["remove-orphans"]) {
        for e in registry::list() {
            if is_project_container(&e, &p.name) && !entries.iter().any(|x| x.id == e.id) {
                entries.push(e);
            }
        }
    }
    for e in entries {
        stop_entry(&e, secs);
        let _ = commands::rm(true, &e.id);
        println!("Container {} removed", e.label());
    }
    for n in networks::list().into_iter().filter(|n| n.starts_with(&format!("{}_", p.name))) {
        if networks::remove(&n).is_ok() {
            println!("Network {n} removed");
        }
    }
    if f.has(&["v", "volumes"]) {
        for v in p.file.volumes.keys() {
            let name = format!("{}_{v}", p.name);
            if volumes::remove(&name).is_ok() {
                println!("Volume {name} removed");
            }
        }
        // anonymous volumes of the services
        for v in volumes::list() {
            if v.name.starts_with(&format!("{}_", p.name)) && v.name.contains("anon-") {
                let _ = volumes::remove(&v.name);
            }
        }
    }
    if let (Some(mode), Some(plans)) = (f.get(&["rmi"]), plans) {
        let store = Store::open(registry::data_dir().join("store"));
        for pl in plans.values() {
            // `local` = images this project built, `all` = every image the services use
            if (mode == "all" || pl.build.is_some())
                && let Ok(r) = Reference::parse(&pl.cfg.target)
                && store.remove(&r).is_ok()
            {
                println!("Image {} removed", pl.cfg.target);
            }
        }
    }
    Ok(0)
}

fn ps(p: &mut Project, args: &[String]) -> Res<i32> {
    let f = flags(args, &["a", "all", "q", "quiet", "services", "no-trunc"], &["format", "status", "filter"], false)?;
    let wanted = pick(p, &f.pos)?;
    if f.has(&["services"]) {
        for s in &wanted {
            println!("{s}");
        }
        return Ok(0);
    }
    let now = registry::now_unix();
    let all = f.has(&["a", "all"]) || f.get(&["status"]).is_some();
    let status = f.get(&["status"]);
    let mut rows = vec![];
    let mut ids = vec![];
    for (svc, e) in containers(p, &wanted) {
        if !(all || e.running() || e.restarting) || status.is_some_and(|s| api::state_of(&e) != s) {
            continue;
        }
        let mut row = extra::container_row(&e, now, f.has(&["no-trunc"]));
        row["Service"] = json!(svc);
        row["Name"] = json!(e.label());
        ids.push(extra::short(&e.id));
        rows.push(row);
    }
    if f.has(&["q", "quiet"]) {
        for i in ids {
            println!("{i}");
        }
        return Ok(0);
    }
    let template = match f.get(&["format"]).map(String::as_str) {
        Some("json") => "{{json .}}".to_string(),
        Some(t) => t.to_string(),
        None => "table {{.Name}}\t{{.Image}}\t{{.Command}}\t{{.Service}}\t{{.RunningFor}}\t{{.Status}}\t{{.Ports}}".to_string(),
    };
    extra::print_rows(&template, &rows, &[("RunningFor", "CREATED"), ("Name", "NAME")])?;
    Ok(0)
}

fn logs(p: &mut Project, args: &[String]) -> Res<i32> {
    let f = flags(args, &["f", "follow", "t", "timestamps", "no-color", "no-log-prefix"], &["n", "tail", "since", "until", "index"], false)?;
    let wanted = pick(p, &f.pos)?;
    let tail: Option<usize> = f.get(&["n", "tail"]).filter(|t| t.as_str() != "all").and_then(|t| t.parse().ok());
    let list = containers(p, &wanted);
    if !f.pos.is_empty() && list.is_empty() {
        return Err(format!("no container for service {}", f.pos.join(", ")).into());
    }
    let prefix = !f.has(&["no-log-prefix"]);
    let width = list.iter().map(|t| t.0.len()).max().unwrap_or(0);
    let mut targets: Vec<(String, registry::Entry, u64)> = vec![];
    for (svc, e) in list {
        let start = match (tail, fs::File::open(registry::log_path(&e.id))) {
            (Some(n), Ok(mut file)) => extra::tail_offset(&mut file, n).unwrap_or(0),
            _ => 0,
        };
        targets.push((svc, e, start));
    }
    let follow = f.has(&["f", "follow"]);
    let show = |svc: &str, line: &str| {
        if prefix {
            println!("{svc:<width$} | {line}")
        } else {
            println!("{line}")
        }
    };
    loop {
        for (svc, e, off) in &mut targets {
            for line in read_new(&registry::log_path(&e.id), off).lines() {
                show(svc, line);
            }
        }
        let alive = targets.iter().any(|(_, e, _)| registry::find(&e.id).is_ok_and(|x| x.running() || x.restarting));
        if !follow || !alive {
            for (svc, e, off) in &mut targets {
                for line in read_new(&registry::log_path(&e.id), off).lines() {
                    show(svc, line);
                }
            }
            return Ok(0);
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// `stop`, `start`, `restart`, `kill`, `pause`, `unpause`, `top` over the containers of some services.
fn each(p: &mut Project, verb: &str, args: &[String]) -> Res<i32> {
    let f = flags(args, &["d", "detach", "wait"], &["t", "timeout", "s", "signal"], false)?;
    let wanted = pick(p, &f.pos)?;
    let secs = f.get(&["t", "timeout"]).and_then(|t| t.parse().ok()).unwrap_or(10);
    let mut list = containers(p, &wanted);
    if matches!(verb, "stop" | "kill" | "pause") {
        list.reverse(); // dependents first
    }
    for (_, e) in &list {
        let name = e.label();
        match verb {
            "stop" => commands::stop(name, secs)?,
            "start" => commands::start(name)?,
            "restart" => commands::restart(name, secs)?,
            "kill" => commands::kill(name)?,
            "pause" => commands::pause(name, true)?,
            "unpause" => commands::pause(name, false)?,
            "top" => commands::top(name)?,
            _ => unreachable!("handled by the caller"),
        }
        if verb != "top" {
            println!("Container {name} {}", match verb { "stop" => "stopped", "start" => "started", "restart" => "restarted", "kill" => "killed", "pause" => "paused", _ => "unpaused" });
        }
    }
    Ok(0)
}

fn rm(p: &mut Project, args: &[String]) -> Res<i32> {
    let f = flags(args, &["f", "force", "s", "stop", "v", "volumes"], &[], false)?;
    let wanted = pick(p, &f.pos)?;
    for (_, e) in containers(p, &wanted) {
        if e.running() || e.restarting {
            if f.has(&["s", "stop"]) {
                stop_entry(&e, 10);
            } else {
                eprintln!("Container {} is running: stop it first (or use -s)", e.label());
                continue;
            }
        }
        commands::rm(true, &e.id)?;
        println!("Container {} removed", e.label());
    }
    Ok(0)
}

fn pull_push(p: &mut Project, verb: &str, args: &[String]) -> Res<i32> {
    let f = flags(args, &["q", "quiet", "ignore-pull-failures", "include-deps", "ignore-buildable", "parallel"], &["policy"], false)?;
    let wanted = pick(p, &f.pos)?;
    let plans = plans(p)?;
    for s in wanted {
        let pl = &plans[&s];
        if verb == "pull" {
            if pl.build.is_some() {
                continue; // built here, nothing to pull
            }
            println!("Pulling {}", pl.cfg.target);
            if let Err(e) = commands::pull(&pl.cfg.target) {
                if f.has(&["ignore-pull-failures"]) {
                    eprintln!("{e}");
                } else {
                    return Err(e);
                }
            }
        } else if pl.build.is_some() || p.file.services[&s].image.is_some() {
            let image = p.file.services[&s].image.clone().unwrap_or_else(|| pl.cfg.target.clone());
            if image != pl.cfg.target {
                commands::tag(&pl.cfg.target, &image)?;
            }
            println!("Pushing {image}");
            commands::push(&image)?;
        }
    }
    Ok(0)
}

fn port(p: &mut Project, args: &[String]) -> Res<i32> {
    let f = flags(args, &[], &["protocol", "index"], false)?;
    let [svc, number] = f.pos.as_slice() else { return Err("usage: docker compose port [--protocol tcp|udp] SERVICE PRIVATE_PORT".into()) };
    let plans = plans(p)?;
    let pl = plans.get(svc).ok_or_else(|| format!("no such service: {svc}"))?;
    let udp = f.get(&["protocol"]).is_some_and(|x| x == "udp");
    let hit = pl.cfg.ports.iter().find(|x| x.container.to_string() == *number && x.udp == udp).ok_or_else(|| format!("no published port {number} for service {svc}"))?;
    println!("{}:{}", hit.host_ip.map_or("0.0.0.0".to_string(), |i| i.to_string()), hit.host);
    Ok(0)
}

fn images(p: &mut Project, args: &[String]) -> Res<i32> {
    let f = flags(args, &["q", "quiet"], &["format"], false)?;
    let wanted = pick(p, &f.pos)?;
    let store = api::store();
    let mut rows = vec![];
    for (svc, e) in containers(p, &wanted) {
        let (repo, tag) = extra::split_repo_tag(&e.image);
        let size = Reference::parse(&e.image).ok().and_then(|r| store.get(&r).ok().flatten()).map_or(0, |m| m.size());
        rows.push(json!({ "ContainerName": e.label(), "Service": svc, "Repository": repo, "Tag": tag, "ID": extra::short(&api::fake_id(&e.image)), "Size": extra::human_size(size) }));
    }
    if f.has(&["q", "quiet"]) {
        let mut ids: Vec<String> = rows.iter().map(|r| r["ID"].as_str().unwrap_or("").to_string()).collect();
        ids.sort();
        ids.dedup();
        for i in ids {
            println!("{i}");
        }
        return Ok(0);
    }
    let template = f.get(&["format"]).cloned().unwrap_or_else(|| "table {{.ContainerName}}\t{{.Repository}}\t{{.Tag}}\t{{.ID}}\t{{.Size}}".into());
    extra::print_rows(&if template == "json" { "{{json .}}".to_string() } else { template }, &rows, &[("ContainerName", "CONTAINER"), ("ID", "IMAGE ID")])?;
    Ok(0)
}

fn ls(args: &[String]) -> Res<i32> {
    let f = flags(args, &["a", "all", "q", "quiet"], &["format", "filter"], false)?;
    let mut projects: BTreeMap<String, Vec<registry::Entry>> = BTreeMap::new();
    for e in registry::list() {
        if let Some(l) = e.config.as_ref().and_then(|c| c.labels.iter().find_map(|l| l.strip_prefix("com.docker.compose.project=").map(String::from))) {
            projects.entry(l).or_default().push(e);
        }
    }
    let rows: Vec<serde_json::Value> = projects
        .iter()
        .filter(|(_, es)| f.has(&["a", "all"]) || es.iter().any(|e| e.running()))
        .map(|(n, es)| {
            let running = es.iter().filter(|e| e.running()).count();
            let exited = es.len() - running;
            let status = [(running > 0).then(|| format!("running({running})")), (exited > 0).then(|| format!("exited({exited})"))].into_iter().flatten().collect::<Vec<_>>().join(", ");
            json!({ "Name": n, "Status": status, "ConfigFiles": "" })
        })
        .collect();
    if f.has(&["q", "quiet"]) {
        for r in &rows {
            println!("{}", r["Name"].as_str().unwrap_or(""));
        }
        return Ok(0);
    }
    let template = f.get(&["format"]).cloned().unwrap_or_else(|| "table {{.Name}}\t{{.Status}}\t{{.ConfigFiles}}".into());
    extra::print_rows(&if template == "json" { "{{json .}}".to_string() } else { template }, &rows, &[("ConfigFiles", "CONFIG FILES")])?;
    Ok(0)
}

fn compose_exec(p: &mut Project, args: &[String]) -> Res<i32> {
    // exec [-d] [-e K=V] [-w dir] [-u user] [-T] [--index n] service cmd...
    let f = flags(args, &["d", "detach", "T", "no-TTY", "privileged", "i", "t"], &["e", "env", "w", "workdir", "u", "user", "index"], true)?;
    let svc = f.pos.first().ok_or("usage: docker compose exec [options] SERVICE COMMAND [ARGS...]")?;
    let s = p.file.services.get(svc).ok_or_else(|| format!("no such service: {svc}"))?;
    let name = container_name(p, svc, s);
    let e = registry::find(&name).map_err(|_| format!("service {svc} is not running (no container {name})"))?;
    let mut a: Vec<String> = vec![];
    if f.has(&["d", "detach"]) {
        a.push("-d".into());
    }
    if !f.has(&["T", "no-TTY"]) && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        a.push("-t".into());
    }
    for x in f.all(&["e", "env"]) {
        a.extend(["-e".to_string(), x]);
    }
    for (flag, names) in [("-w", &["w", "workdir"][..]), ("-u", &["u", "user"][..])] {
        if let Some(v) = f.get(names) {
            a.extend([flag.to_string(), v.clone()]);
        }
    }
    a.push(e.id.clone());
    a.extend(f.pos[1..].iter().cloned());
    extra::exec(&a)
}

fn compose_run(p: &mut Project, args: &[String]) -> Res<i32> {
    let f = flags(
        args,
        &["rm", "d", "detach", "T", "no-TTY", "no-deps", "service-ports", "i", "t", "use-aliases", "build", "quiet-pull", "remove-orphans"],
        &["e", "env", "v", "volume", "p", "publish", "entrypoint", "w", "workdir", "u", "user", "name", "l", "label", "cap-add", "cap-drop", "pull"],
        true,
    )?;
    let svc = f.pos.first().ok_or("usage: docker compose run [options] SERVICE [COMMAND] [ARGS...]")?.clone();
    pick(p, std::slice::from_ref(&svc))?;
    let plans = plans(p)?;
    let pl = plans.get(&svc).ok_or_else(|| format!("no such service: {svc}"))?;
    let store = Store::open(registry::data_dir().join("store"));
    // networks, volumes and the services this one depends on
    let deps: Vec<String> = if f.has(&["no-deps"]) { vec![] } else { with_deps(&plans, std::slice::from_ref(&svc))?.into_iter().filter(|s| *s != svc).collect() };
    if deps.is_empty() {
        let net = format!("{}_default", p.name);
        if !networks::exists(&net) {
            networks::create(&net)?;
        }
        for v in p.file.volumes.keys() {
            volumes::create(&format!("{}_{v}", p.name))?;
        }
    } else {
        let o = UpOpts { detach: true, build: f.has(&["build"]), no_build: false, force_recreate: false, no_recreate: false, no_start: false, no_deps: true, wait: false, remove_orphans: false, abort_on_exit: false, services: deps };
        up(p, &o)?;
    }
    ensure_image(&store, pl, f.has(&["build"]))?;
    let mut cfg = pl.cfg.clone();
    cfg.name = Some(f.get(&["name"]).cloned().unwrap_or_else(|| format!("{}-{svc}-run-{}", p.name, &registry::new_id()[..12])));
    cfg.aliases.clear();
    if !f.has(&["service-ports"]) {
        cfg.ports.clear(); // like Docker: a one-off container does not publish the service's ports
    }
    for pp in f.all(&["p", "publish"]) {
        cfg.ports.extend(docker_core::cli::Port::parse_all(&pp)?);
    }
    if f.pos.len() > 1 {
        cfg.cmd = f.pos[1..].to_vec();
    }
    if let Some(ep) = f.get(&["entrypoint"]) {
        cfg.entrypoint = Some(ep.clone());
    }
    cfg.env.extend(f.all(&["e", "env"]));
    for v in f.all(&["v", "volume"]) {
        cfg.volumes.push(docker_core::cli::Volume::parse(&v)?);
    }
    if let Some(w) = f.get(&["w", "workdir"]) {
        cfg.workdir = Some(w.clone());
    }
    if let Some(u) = f.get(&["u", "user"]) {
        cfg.user = Some(u.clone());
    }
    cfg.labels.retain(|l| !l.starts_with("com.docker.compose.oneoff="));
    cfg.labels.push("com.docker.compose.oneoff=True".into());
    cfg.labels.extend(f.all(&["l", "label"]));
    cfg.auto_remove = f.has(&["rm"]);
    cfg.detach = f.has(&["d", "detach"]);
    let tty = !f.has(&["T", "no-TTY"]) && !cfg.detach && std::io::IsTerminal::is_terminal(&std::io::stdin());
    cfg.tty = tty;
    cfg.interactive = tty || f.has(&["i"]);
    cfg.restart = docker_core::cli::Restart::No;
    if cfg.detach {
        platform::run(&cfg)?; // prints the id
        return Ok(0);
    }
    Ok(platform::run(&cfg)?)
}

fn cp(p: &mut Project, args: &[String]) -> Res<i32> {
    let f = flags(args, &["a", "archive", "L", "follow-link", "all"], &["index"], false)?;
    let [src, dst] = f.pos.as_slice() else { return Err("usage: docker compose cp [options] SERVICE:SRC_PATH DEST_PATH | SRC_PATH SERVICE:DEST_PATH".into()) };
    let resolve = |x: &str| -> Res<String> {
        match x.split_once(':') {
            Some((svc, path)) if p.file.services.contains_key(svc) && !svc.contains(['/', '\\']) => {
                let s = &p.file.services[svc];
                let e = registry::find(&container_name(p, svc, s)).map_err(|_| format!("service {svc} has no container"))?;
                Ok(format!("{}:{path}", e.id))
            }
            _ => Ok(x.to_string()),
        }
    };
    commands::cp(&resolve(src)?, &resolve(dst)?)?;
    Ok(0)
}

fn events(p: &mut Project, args: &[String]) -> Res<i32> {
    let mut a = vec!["events".to_string(), "--filter".to_string(), format!("label=com.docker.compose.project={}", p.name)];
    a.extend(args.iter().cloned());
    extra::run(a)
}

fn config(p: &mut Project, args: &[String]) -> Res<i32> {
    let f = flags(args, &["q", "quiet", "services", "volumes", "networks", "images", "no-interpolate", "no-normalize", "resolve-image-digests", "hash"], &["format", "o", "output", "profiles", "environment"], false)?;
    let names = |it: Vec<String>| {
        for n in it {
            println!("{n}");
        }
    };
    if f.has(&["services"]) {
        names(pick(p, &[])?);
        return Ok(0);
    }
    if f.has(&["volumes"]) {
        names(p.file.volumes.keys().cloned().collect());
        return Ok(0);
    }
    if f.has(&["networks"]) {
        names(p.file.networks.keys().cloned().collect());
        return Ok(0);
    }
    if f.has(&["images"]) {
        let all = plans(p)?;
        names(all.values().map(|x| x.cfg.target.clone()).collect());
        return Ok(0);
    }
    if f.has(&["q", "quiet"]) {
        plans(p)?; // just validate
        return Ok(0);
    }
    let mut doc = p.raw.clone();
    if let Value::Mapping(m) = &mut doc {
        m.insert("name".into(), p.name.clone().into());
    }
    let text = if f.get(&["format"]).is_some_and(|x| x == "json") { serde_json::to_string_pretty(&doc)? } else { serde_yaml::to_string(&doc)? };
    match f.get(&["o", "output"]) {
        Some(path) => fs::write(path, text)?,
        None => print!("{text}"),
    }
    Ok(0)
}

pub(super) fn run(p: &mut Project, words: &[String]) -> Res<i32> {
    let (sub, rest) = words.split_first().ok_or("usage: docker compose [options] COMMAND")?;
    let rest = rest.to_vec();
    match sub.as_str() {
        "up" | "create" => {
            let f = flags(
                &rest,
                &["d", "detach", "build", "no-build", "force-recreate", "no-recreate", "no-start", "no-deps", "wait", "remove-orphans", "abort-on-container-exit", "no-color", "no-log-prefix", "quiet-pull", "attach-dependencies", "y", "yes", "renew-anon-volumes", "V", "always-recreate-deps", "watch", "no-attach-dependencies", "quiet-build", "menu", "w", "abort-on-container-failure"],
                &["pull", "scale", "t", "timeout", "attach", "no-attach", "exit-code-from", "wait-timeout", "progress"],
                false,
            )?;
            if f.has(&["watch", "w"]) {
                return Err("compose watch is not supported".into());
            }
            up(
                p,
                &UpOpts {
                    detach: f.has(&["d", "detach"]) || f.has(&["wait"]),
                    build: f.has(&["build"]),
                    no_build: f.has(&["no-build"]),
                    force_recreate: f.has(&["force-recreate"]),
                    no_recreate: f.has(&["no-recreate"]),
                    no_start: sub == "create" || f.has(&["no-start"]),
                    no_deps: f.has(&["no-deps"]),
                    wait: f.has(&["wait"]),
                    remove_orphans: f.has(&["remove-orphans"]),
                    abort_on_exit: f.has(&["abort-on-container-exit"]),
                    services: f.pos,
                },
            )
        }
        "down" => down(p, &flags(&rest, &["v", "volumes", "remove-orphans"], &["rmi", "t", "timeout"], false)?),
        "ps" => ps(p, &rest),
        "logs" => logs(p, &rest),
        "stop" | "start" | "restart" | "kill" | "pause" | "unpause" | "top" => each(p, sub, &rest),
        "rm" => rm(p, &rest),
        "pull" | "push" => pull_push(p, sub, &rest),
        "port" => port(p, &rest),
        "images" => images(p, &rest),
        "exec" => compose_exec(p, &rest),
        "run" => compose_run(p, &rest),
        "cp" => cp(p, &rest),
        "events" => events(p, &rest),
        "config" | "convert" => config(p, &rest),
        "build" => {
            let f = flags(&rest, &["q", "quiet", "no-cache", "pull", "push", "with-dependencies"], &["build-arg", "m", "memory", "progress", "ssh"], false)?;
            let wanted = pick(p, &f.pos)?;
            let all = plans(p)?;
            for s in wanted {
                if let Some(b) = all[&s].build.as_ref() {
                    let mut b = b.clone();
                    b.no_cache = f.has(&["no-cache"]);
                    println!("Building {s}");
                    platform::build(&b, &mut |l| println!("  {l}"))?;
                }
            }
            Ok(0)
        }
        "ls" => ls(&rest),
        "version" => {
            let f = flags(&rest, &["short"], &["f", "format"], false)?;
            if f.has(&["short"]) {
                println!("2.29.0");
            } else if f.get(&["f", "format"]).is_some_and(|t| t == "json") {
                println!("{{\"version\":\"v2.29.0\"}}");
            } else {
                println!("Docker Compose version v2.29.0 (Sasta Docker)");
            }
            Ok(0)
        }
        "watch" => Err("compose watch is not supported; use `docker compose up --build` after a change".into()),
        other => Err(format!("unknown compose command {other:?}").into()),
    }
}
