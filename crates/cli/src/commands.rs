use docker_core::{cli::split_container_ref, platform, registry::{self, Entry}, stats::CpDir, volumes};
use docker_image::{Reference, Store};
use std::{error::Error, fs, io::{Read, Seek, SeekFrom, Write}, thread, time::Duration};

type Res = Result<(), Box<dyn Error>>;

fn store() -> Store {
    Store::open(registry::data_dir().join("store"))
}

fn mb(bytes: u64) -> String {
    format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
}

fn hms(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn clip(s: String, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n - 1).collect::<String>() + "…" } else { s }
}

pub fn pull(name: &str) -> Res {
    pull_platform(name, None)
}

pub fn pull_platform(name: &str, platform: Option<&str>) -> Res {
    let r = Reference::parse(name)?;
    let store = store();
    println!("Pulling {}", r.display());
    let meta = docker_image::pull_for(&store, &r, platform, &mut |line| println!("{line}"))?;
    #[cfg(target_os = "linux")]
    for l in &meta.layers {
        store.ensure_layer(l)?; // unpack now so `run` starts instantly
    }
    #[cfg(not(target_os = "linux"))]
    println!("Layers downloaded; unpacking and running need a Linux host.");
    println!("Status: pulled {} ({})", meta.name, mb(meta.size()));
    Ok(())
}

pub fn images() -> Res {
    println!("{:<40} {:>10} {:>7}", "IMAGE", "SIZE", "LAYERS");
    for m in store().list() {
        println!("{:<40} {:>10} {:>7}", m.name, mb(m.size()), m.layers.len());
    }
    Ok(())
}

pub fn rmi(name: &str) -> Res {
    let r = Reference::parse(name)?;
    let freed = store().remove(&r)?;
    println!("Untagged: {}\nFreed: {}", r.display(), mb(freed));
    Ok(())
}

pub fn status_of(e: &Entry, now: u64) -> String {
    status(e, now)
}

fn status(e: &Entry, now: u64) -> String {
    if e.created && !e.running() {
        return "Created".into();
    }
    if e.restarting && e.exit_code.is_none() {
        return format!("Restarting ({})", e.restarts);
    }
    match e.exit_code {
        None if e.running() => {
            let health = e.health.as_deref().map(|h| format!(" ({h})")).unwrap_or_default();
            format!("Up {}{health}", hms(now.saturating_sub(e.started_unix)))
        }
        Some(c) if c >= 0 => format!("Exited ({c})"),
        _ => "Exited".into(),
    }
}

pub fn ps(all: bool) -> Res {
    println!("{:<18} {:<12} {:<14} {:<24} {:<12} {:<15} {:<12}", "CONTAINER ID", "NAME", "IMAGE", "COMMAND", "STATUS", "IP", "PORTS");
    let now = registry::now_unix();
    for e in registry::list() {
        let live = e.running() || (e.restarting && e.exit_code.is_none());
        if !live && !all {
            continue;
        }
        println!(
            "{:<18} {:<12} {:<14} {:<24} {:<12} {:<15} {:<12}",
            e.id,
            clip(e.name.clone().unwrap_or_default(), 11),
            clip(e.image.clone(), 13),
            clip(e.cmd.join(" "), 23),
            status(&e, now),
            e.ip.as_deref().unwrap_or(""),
            e.ports.join(",")
        );
    }
    Ok(())
}


pub fn rm(force: bool, q: &str) -> Res {
    let e = registry::find(q)?;
    let restarting = e.restarting && e.exit_code.is_none();
    if e.running() || restarting {
        if !force {
            return Err(format!("container {} is running; stop it first or use rm -f", e.label()).into());
        }
        registry::request_stop(&e.id)?; // a restart policy must not bring it back
        if e.running() {
            platform::stop(&e.id)?;
        }
        for _ in 0..100 {
            let now = registry::find(&e.id);
            if !e.running() && now.map_or(true, |x| x.exit_code.is_some() || !x.restarting) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    registry::delete(&e.id);
    println!("{}", e.label());
    Ok(())
}

pub fn logs(follow: bool, q: &str) -> Res {
    let e = registry::find(q)?;
    let path = registry::log_path(&e.id);
    let mut f = fs::File::open(&path).map_err(|_| format!("no logs for {} (only detached containers keep logs)", e.label()))?;
    let mut out = std::io::stdout();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf)?;
        if n > 0 {
            out.write_all(&buf[..n])?;
            out.flush()?;
            continue;
        }
        // caught up: stop unless we're following a container that is still running
        if !follow || !registry::find(&e.id).is_ok_and(|x| x.running()) {
            // one last read in case it wrote between our read and the liveness check
            let pos = f.stream_position()?;
            if f.metadata()?.len() > pos {
                f.seek(SeekFrom::Start(pos))?;
                continue;
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(150));
    }
}

pub fn inspect(q: &str) -> Res {
    // like Docker, `inspect` also takes an image when no container has that name
    let e = match registry::find(q) {
        Ok(e) => e,
        Err(err) => return image_inspect(q).map_err(|_| err.into()),
    };
    let mut v = serde_json::to_value(&e)?;
    v["running"] = serde_json::json!(e.running());
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

pub fn exec(q: &str, cmd: &[String], tty: bool) -> Result<i32, Box<dyn Error>> {
    let e = registry::find(q)?;
    Ok(platform::exec(&e, cmd, tty)?)
}

pub fn build(args: &docker_core::cli::BuildArgs) -> Res {
    let meta = platform::build(args, &mut |line| {
        if !args.quiet {
            println!("{line}");
        }
    })?;
    if args.quiet {
        // no separate image ids here: the top layer digest identifies the result
        println!("{}", meta.layers.last().map_or(meta.name.as_str(), |l| l.digest.as_str()));
    }
    Ok(())
}

pub fn prune() -> Res {
    let freed = store().prune()?;
    println!("Total reclaimed space: {}", mb(freed));
    Ok(())
}

pub fn login(server: Option<&str>, user: &str, password: docker_core::cli::Password) -> Res {
    use docker_core::cli::Password;
    let password = match password {
        Password::Arg(p) => {
            eprintln!("WARNING: using --password on the command line is insecure; prefer --password-stdin");
            p
        }
        Password::Stdin => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim_end_matches(['\r', '\n']).to_string()
        }
    };
    let reg = docker_image::login(store().root(), server.unwrap_or(""), user, &password)?;
    println!("Login Succeeded ({reg})");
    Ok(())
}

pub fn logout(server: Option<&str>) -> Res {
    let reg = docker_image::normalize_registry(server.unwrap_or(""))?;
    if docker_image::logout(store().root(), &reg)? {
        println!("Removed login credentials for {reg}");
    } else {
        println!("Not logged in to {reg}");
    }
    Ok(())
}

pub fn tag(src: &str, dst: &str) -> Res {
    store().tag(&Reference::parse(src)?, &Reference::parse(dst)?)?;
    Ok(())
}

pub fn push(name: &str) -> Res {
    let r = Reference::parse(name)?;
    println!("The push refers to repository [{}/{}]", r.registry, r.repo);
    docker_image::push(&store(), &r, &mut |line| println!("{line}"))?;
    Ok(())
}

pub fn version() -> Res {
    let line = format!("Sasta Docker {} {}/{}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    if cfg!(target_os = "linux") {
        println!("{line}");
    } else {
        println!("Client: {line}");
        match crate::vm::native_reason() {
            Some(why) => println!("Engine: native Windows mode, no Linux VM ({why})"),
            None => println!("VM:     {}", crate::vm::vm_version().unwrap_or_else(|| "not running (run `docker vm init`)".into())),
        }
    }
    Ok(())
}

fn running(q: &str) -> Result<Entry, Box<dyn Error>> {
    let e = registry::find(q)?;
    if e.running() { Ok(e) } else { Err(format!("container {} is not running", e.label()).into()) }
}

fn wait_until_gone(e: &Entry, tenths: u64) {
    for _ in 0..tenths {
        if !e.running() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// SIGTERM, wait `timeout` seconds, then SIGKILL: the same contract as Docker's.
/// A container between two runs of its restart policy: stopping it just cancels the next run.
fn cancel_restart(q: &str) -> Result<Option<Entry>, Box<dyn Error>> {
    let e = registry::find(q)?;
    if e.restarting && !e.running() && e.exit_code.is_none() {
        registry::request_stop(&e.id)?;
        println!("{}", e.label());
        return Ok(Some(e));
    }
    Ok(None)
}

/// The image's `STOPSIGNAL`, SIGTERM when it has none.
fn stop_signal_of(e: &Entry) -> i32 {
    let meta = Reference::parse(&e.image).ok().and_then(|r| store().get(&r).ok().flatten());
    meta.and_then(|m| docker_image::dockerfile::parse_signal(&m.stop_signal)).unwrap_or(15)
}

pub fn stop(q: &str, timeout: u64) -> Res {
    if cancel_restart(q)?.is_some() {
        return Ok(());
    }
    let e = running(q)?;
    registry::request_stop(&e.id)?; // a restart policy must not revive it
    let _ = platform::signal(&e, stop_signal_of(&e));
    wait_until_gone(&e, timeout * 10);
    if e.running() {
        platform::stop(&e.id)?;
        wait_until_gone(&e, 50);
    }
    println!("{}", e.label());
    Ok(())
}

pub fn kill(q: &str) -> Res {
    if cancel_restart(q)?.is_some() {
        return Ok(());
    }
    let e = running(q)?;
    registry::request_stop(&e.id)?;
    platform::stop(&e.id)?;
    println!("{}", e.label());
    Ok(())
}

/// Run an exited container again with the settings it was created with (it gets a fresh filesystem:
/// container writes live in RAM by design, which is what keeps the SSD untouched).
pub fn start(q: &str) -> Res {
    let e = registry::find(q)?;
    if e.running() {
        return Err(format!("container {} is already running", e.label()).into());
    }
    let mut cfg = e.config.clone().ok_or("this container has no saved settings; run it again with `docker run`")?;
    cfg.detach = true;
    cfg.name = e.name.clone();
    cfg.preset_id = Some(e.id.clone()); // same id, same name, same log
    platform::run(&cfg)?;
    Ok(())
}

pub fn restart(q: &str, timeout: u64) -> Res {
    if registry::find(q)?.running() {
        stop(q, timeout)?;
    }
    start(q)
}

pub fn update(q: &str, mem_mb: Option<u64>, cpu: Option<u32>, procs: Option<u32>) -> Res {
    let e = running(q)?;
    platform::update(&e.id, mem_mb.map(|m| m << 20), cpu, procs)?;
    // keep `inspect`/the API in step with what the kernel now enforces
    registry::update(&e.id, |x| {
        if let Some(c) = x.config.as_mut() {
            if let Some(m) = mem_mb {
                c.mem_bytes = (m > 0).then_some(m << 20);
            }
            if let Some(p) = cpu {
                c.cpu_percent = (p > 0).then_some(p);
            }
            if let Some(n) = procs {
                c.procs = (n > 0).then_some(n);
            }
        }
    })?;
    println!("{}", e.label());
    Ok(())
}

pub fn pause(q: &str, on: bool) -> Res {
    let e = running(q)?;
    platform::freeze(&e.id, on)?;
    println!("{}", e.label());
    Ok(())
}

pub fn wait(q: &str) -> Res {
    let id = registry::find(q)?.id;
    let mut gone_since: Option<std::time::Instant> = None;
    loop {
        let e = registry::find(&id)?;
        if let Some(c) = e.exit_code {
            println!("{c}");
            return Ok(());
        }
        if !e.running() {
            // the process can end a moment before its supervisor records the exit code: give it 5s
            if gone_since.get_or_insert_with(std::time::Instant::now).elapsed() > Duration::from_secs(5) {
                println!("-1"); // supervisor vanished without recording an exit
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub fn rename(old: &str, new: &str) -> Res {
    let e = registry::find(old)?;
    if !docker_core::cli::is_volume_name(new) {
        return Err("a container name must match [a-zA-Z0-9][a-zA-Z0-9_.-]*".into());
    }
    registry::rename(&e.id, new)?;
    Ok(())
}

pub fn top(q: &str) -> Res {
    let e = running(q)?;
    println!("{:<8} {:<8} {:<8} COMMAND", "PID", "PPID", "UID");
    for p in platform::top(&e.id)? {
        println!("{:<8} {:<8} {:<8} {}", p.pid, p.ppid, p.uid, p.cmd);
    }
    Ok(())
}

fn host_memory() -> Option<u64> {
    let t = fs::read_to_string("/proc/meminfo").ok()?;
    t.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.split_whitespace().next()?.parse::<u64>().ok().map(|kb| kb * 1024)
}

pub fn stats(no_stream: bool, json: bool, ids: &[String]) -> Res {
    let pick = || -> Result<Vec<Entry>, Box<dyn Error>> {
        if ids.is_empty() {
            Ok(registry::list().into_iter().filter(Entry::running).collect())
        } else {
            ids.iter().map(|i| running(i)).collect()
        }
    };
    loop {
        let list = pick()?;
        let t0 = std::time::Instant::now();
        let before: Vec<_> = list.iter().map(|e| platform::stats(&e.id)).collect();
        thread::sleep(Duration::from_millis(if no_stream { 500 } else { 1000 }));
        let wall_ms = t0.elapsed().as_millis().max(1) as f64;
        if !no_stream && !json {
            print!("\x1b[2J\x1b[H"); // clear, like `docker stats`
        }
        if !json {
            println!("{:<18} {:<12} {:>8} {:>24} {:>7} {:>5}", "CONTAINER ID", "NAME", "CPU %", "MEM USAGE / LIMIT", "MEM %", "PIDS");
        }
        for (e, b) in list.iter().zip(before) {
            let (Some(b), Some(a)) = (b, platform::stats(&e.id)) else { continue };
            let cpu = a.cpu_ms.saturating_sub(b.cpu_ms) as f64 / wall_ms * 100.0;
            let limit = e.mem_bytes.or_else(host_memory);
            if json {
                // one object per line: the desktop app and scripts read this
                let mem_limit = limit.unwrap_or(0);
                println!(
                    "{}",
                    serde_json::json!({
                        "id": e.id, "name": e.name, "image": e.image, "cpu_percent": (cpu * 10.0).round() / 10.0,
                        "mem_bytes": a.mem_bytes, "mem_limit": mem_limit,
                        "mem_percent": if mem_limit > 0 { (a.mem_bytes as f64 / mem_limit as f64 * 1000.0).round() / 10.0 } else { 0.0 },
                        "pids": a.procs, "disk_bytes": a.disk_bytes,
                    })
                );
                continue;
            }
            let (lim, pct) = match limit {
                Some(l) => (mb(l), format!("{:.1}%", a.mem_bytes as f64 / l as f64 * 100.0)),
                None => ("-".into(), "-".into()),
            };
            println!("{:<18} {:<12} {:>7.1}% {:>24} {:>7} {:>5}", e.id, clip(e.name.clone().unwrap_or_default(), 11), cpu, format!("{} / {lim}", mb(a.mem_bytes)), pct, a.procs);
        }
        if no_stream {
            return Ok(());
        }
    }
}

pub fn cp(src: &str, dst: &str) -> Res {
    match (split_container_ref(src), split_container_ref(dst)) {
        (Some((c, p)), None) => platform::cp(&running(c)?, CpDir::FromContainer, p, dst)?,
        (None, Some((c, p))) => platform::cp(&running(c)?, CpDir::ToContainer, p, src)?,
        _ => return Err("exactly one of source and destination must be <container>:<path>".into()),
    }
    Ok(())
}

pub fn volume(cmd: &docker_core::cli::VolumeCmd) -> Res {
    use docker_core::cli::VolumeCmd::*;
    match cmd {
        Create(n) => {
            volumes::create(n)?;
            println!("{n}");
        }
        Ls => {
            println!("{:<30} {:>10}", "VOLUME NAME", "SIZE");
            for v in volumes::list() {
                println!("{:<30} {:>10}", v.name, mb(v.size));
            }
        }
        Rm(n) => {
            volumes::remove(n)?;
            println!("{n}");
        }
        Inspect(n) => {
            if !volumes::exists(n) {
                return Err(format!("no such volume: {n}").into());
            }
            println!("{{\n  \"name\": {n:?},\n  \"mountpoint\": {:?},\n  \"in_use_by\": {:?}\n}}", volumes::path(n).to_string_lossy(), volumes::used_by(n));
        }
        Prune => {
            let (names, freed) = volumes::prune();
            for n in &names {
                println!("{n}");
            }
            println!("Total reclaimed space: {}", mb(freed));
        }
    }
    Ok(())
}

pub fn system_df() -> Res {
    let images = store().list();
    let all = registry::list();
    let running = all.iter().filter(|e| e.running()).count();
    let vols = volumes::list();
    println!("{:<12} {:>6} {:>8} {:>10}", "TYPE", "TOTAL", "ACTIVE", "SIZE");
    println!("{:<12} {:>6} {:>8} {:>10}", "Images", images.len(), "-", mb(images.iter().map(|m| m.size()).sum()));
    println!("{:<12} {:>6} {:>8} {:>10}", "Containers", all.len(), running, "-");
    println!("{:<12} {:>6} {:>8} {:>10}", "Volumes", vols.len(), vols.iter().filter(|v| !volumes::used_by(&v.name).is_empty()).count(), mb(vols.iter().map(|v| v.size).sum()));
    Ok(())
}

pub fn container_prune() -> Res {
    let mut n = 0;
    for e in registry::list().into_iter().filter(|e| !e.running()) {
        registry::delete(&e.id);
        println!("{}", e.label());
        n += 1;
    }
    println!("Deleted containers: {n}");
    Ok(())
}

pub fn save(out: &str, images: &[String]) -> Res {
    let refs: Vec<Reference> = images.iter().map(|i| Reference::parse(i)).collect::<Result<_, _>>()?;
    docker_image::save(&store(), &refs, std::path::Path::new(out), &mut |l| eprintln!("{l}"))?;
    Ok(())
}

pub fn load(input: &str) -> Res {
    for name in docker_image::load(&store(), std::path::Path::new(input), &mut |l| eprintln!("{l}"))? {
        println!("Loaded image: {name}");
    }
    Ok(())
}

pub fn network(cmd: &docker_core::cli::NetCmd) -> Res {
    use docker_core::{cli::NetCmd::*, networks};
    match cmd {
        Create(n, subnet) => {
            networks::create_with(n, subnet.as_deref())?;
            println!("{n}");
        }
        Connect(n, c) => networks::connect(n, c)?,
        Disconnect(n, c) => networks::disconnect(n, c)?,
        Ls => {
            println!("{:<24} {:>10}", "NETWORK", "CONTAINERS");
            println!("{:<24} {:>10}", "bridge (default)", "-");
            for n in networks::list() {
                println!("{:<24} {:>10}", n, networks::members(Some(&n), "").len());
            }
        }
        Rm(n) => {
            networks::remove(n)?;
            println!("{n}");
        }
        Inspect(n) => {
            if !networks::exists(n) {
                return Err(format!("no such network: {n}").into());
            }
            let members: Vec<_> = networks::members(Some(n), "")
                .iter()
                .map(|e| {
                    let ip6 = e.ip.as_deref().and_then(|i| i.parse().ok()).map(networks::ip6_of);
                    serde_json::json!({ "name": e.label(), "ip": e.ip, "ip6": ip6, "id": e.id })
                })
                .collect();
            let sub = networks::subnet_of(Some(n));
            let info = serde_json::json!({
                "name": n, "driver": "bridge", "subnet": sub.cidr(), "gateway": sub.gateway(),
                "ipv6_subnet": "fd00:5a57::/64", "ipv6_gateway": networks::GATEWAY6, "containers": members,
            });
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
    }
    Ok(())
}

pub fn image_inspect(name: &str) -> Res {
    let v = docker_image::inspect(&store(), &Reference::parse(name)?)?;
    println!("{}", serde_json::to_string_pretty(&[v])?);
    Ok(())
}

pub fn history(name: &str) -> Res {
    let rows = docker_image::history(&store(), &Reference::parse(name)?)?;
    println!("{:<22} {:<40} {:>10}", "CREATED", "CREATED BY", "SIZE");
    for r in rows {
        let created = if r.created.is_empty() { "<missing>".to_string() } else { clip(r.created, 20) };
        println!("{:<22} {:<40} {:>10}", created, clip(r.created_by.replace('\t', " "), 40), mb(r.size));
    }
    Ok(())
}

pub fn search(term: &str) -> Res {
    let hits = docker_image::search(&store().tmp_dir(), term, 25)?;
    println!("{:<36} {:<50} {:>6} {:>8}", "NAME", "DESCRIPTION", "STARS", "OFFICIAL");
    for h in hits {
        let desc = h.short_description.unwrap_or_default().replace('\n', " ");
        println!("{:<36} {:<50} {:>6} {:>8}", clip(h.repo_name, 35), clip(desc, 49), h.star_count, if h.is_official { "[OK]" } else { "" });
    }
    Ok(())
}
