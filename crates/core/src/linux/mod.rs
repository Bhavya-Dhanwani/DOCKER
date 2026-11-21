//! Linux backend: namespaces + cgroup v2 + overlayfs + pivot_root.
mod build;
mod cgroup;
mod cp;
mod exec;
mod harden;
mod health;
mod net;
mod netlink;
mod pty;
mod rootfs;
mod stdin;
pub mod rootless;
mod sys;

use crate::{
    cli::{Config, Network, Volume},
    env,
    error::{Error, Result},
    registry::{self, Entry},
    security::{self, Security},
    stats::{ProcInfo, Stats},
};
use cgroup::Cgroup;
use docker_image::{Reference, Store};
use rootfs::Source;
use std::{fs, io::{Read, Write}, os::fd::{AsRawFd, OwnedFd}, path::Path, process};

pub use build::build;
pub use cp::{cp, secure_join};
pub use stdin::stdin_sock;
pub use exec::exec;

const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Everything resolved before forking, so the child only has to apply it.
struct Plan {
    source: Source,
    cmd: Vec<String>,
    env: Vec<String>,
    workdir: String,
    volumes: Vec<Volume>,
    /// Image/Dockerfile USER; empty = root.
    user: String,
    security: Security,
    health: Option<docker_image::Health>,
}

fn img(e: docker_image::Error) -> Error {
    Error::Usage(e.to_string())
}

/// A terminal needs TERM; pass the host's (or a safe default).
fn with_term(mut env: Vec<String>, tty: bool) -> Vec<String> {
    if tty && !env.iter().any(|e| e.starts_with("TERM=")) {
        env.push(format!("TERM={}", std::env::var("TERM").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| "xterm".into())));
    }
    env
}

fn with_default_path(mut env: Vec<String>) -> Vec<String> {
    if !env.iter().any(|e| e.starts_with("PATH=")) {
        env.push(DEFAULT_PATH.into());
    }
    env
}

/// Named volumes become directories under the data dir (created on first use).
fn resolve_volumes(cfg: &Config) -> Result<Vec<Volume>> {
    cfg.volumes
        .iter()
        .map(|v| {
            if v.is_named() {
                Ok(Volume { host: crate::volumes::ensure(&v.host)?.to_string_lossy().into_owned(), ..v.clone() })
            } else {
                Ok(v.clone())
            }
        })
        .collect()
}

fn plan(cfg: &Config) -> Result<Plan> {
    let host = |k: &str| std::env::var(k).ok();
    if Path::new(&cfg.target).is_dir() {
        if cfg.cmd.is_empty() {
            return Err(Error::Usage("missing <command>".into()));
        }
        let root = fs::canonicalize(&cfg.target).map_err(|e| Error::io(format!("rootfs {}", cfg.target), e))?;
        let mut cmd = cfg.cmd.clone();
        if let Some(ep) = &cfg.entrypoint {
            cmd.insert(0, ep.clone());
        }
        return Ok(Plan {
            source: Source::Dir(root),
            cmd,
            env: with_term(env::merge(&with_default_path(vec![]), &cfg.env, host), cfg.tty),
            workdir: cfg.workdir.clone().unwrap_or_else(|| "/".into()),
            volumes: resolve_volumes(cfg)?,
            user: cfg.user.clone().unwrap_or_default(),
            security: security::resolve(cfg)?,
            health: cfg.health.clone(),
        });
    }

    let store = Store::open(registry::data_dir().join("store"));
    let r = Reference::parse(&cfg.target).map_err(img)?;
    let meta = store.get(&r).map_err(img)?.ok_or_else(|| {
        Error::Usage(format!("image {} not found locally; run `docker pull {}`", r.display(), cfg.target))
    })?;
    let layers = if registry::rootless() {
        rootless::ensure_layers(&store, &meta.layers)?
    } else {
        meta.layers.iter().map(|l| store.ensure_layer(l)).collect::<docker_image::Result<Vec<_>>>().map_err(img)?
    };

    // Docker semantics: ENTRYPOINT stays, arguments replace CMD
    let mut cmd = match &cfg.entrypoint {
        // --entrypoint replaces the image's entrypoint and its default CMD (only explicit arguments remain)
        Some(ep) => {
            let mut c = vec![ep.clone()];
            c.extend(cfg.cmd.clone());
            c
        }
        None => {
            let mut c = meta.entrypoint.clone();
            c.extend(if cfg.cmd.is_empty() { meta.cmd.clone() } else { cfg.cmd.clone() });
            c
        }
    };
    cmd.retain(|a| !a.is_empty() || cfg.entrypoint.is_none());
    if cmd.is_empty() {
        return Err(Error::Usage(format!("image {} has no default command; pass one", meta.name)));
    }
    let workdir = cfg.workdir.clone().unwrap_or_else(|| if meta.working_dir.is_empty() { "/".into() } else { meta.working_dir.clone() });
    let mut volumes = resolve_volumes(cfg)?;
    // `VOLUME /x` in the image: an anonymous volume (seeded from the image's content) unless -v already covers the path
    for path in &meta.volumes {
        if !volumes.iter().any(|v| v.container.trim_end_matches('/') == path.trim_end_matches('/')) {
            let dir = crate::volumes::ensure(&registry::new_id())?;
            build::seed_volume(&layers, path, &dir);
            volumes.push(Volume { host: dir.to_string_lossy().into_owned(), container: path.clone(), ro: false });
        }
    }
    Ok(Plan {
        source: Source::Image(layers),
        cmd,
        env: with_term(env::merge(&with_default_path(meta.env.clone()), &cfg.env, host), cfg.tty),
        workdir,
        volumes,
        user: cfg.user.clone().unwrap_or_else(|| meta.user.clone()),
        security: security::resolve(cfg)?,
        health: if cfg.no_healthcheck { None } else { cfg.health.clone().or_else(|| meta.healthcheck.clone()) },
    })
}

/// Set in the background supervisor created by `detach`: it is a *copy of the calling process*, so when its
/// container is done it must exit, not return into whatever code called `run` (`docker compose up` loops over
/// services, for instance, and would otherwise start them all again from every supervisor).
static SUPERVISOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Detach from the terminal. Returns `Some(ready)` in the background supervisor (write a byte to it
/// once registered) and `None` in the launcher, which should print the id and exit.
fn detach(id: &str) -> Result<Option<OwnedFd>> {
    let (rd, wr) = sys::pipe()?;
    match sys::fork()? {
        sys::Fork::Child => {
            drop(rd);
            sys::setsid()?;
            sys::redirect_stdio(&registry::log_path(id))?;
            SUPERVISOR.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(Some(wr))
        }
        sys::Fork::Parent(_) => {
            drop(wr);
            let mut b = [0u8; 1];
            if fs::File::from(rd).read(&mut b).unwrap_or(0) == 0 {
                let log = registry::log_path(id);
                let why = fs::read_to_string(&log).unwrap_or_default();
                return Err(Error::Usage(format!("container failed to start: {}", why.trim())));
            }
            Ok(None)
        }
    }
}

/// Run the command in fresh pid/mount/uts/ipc/net namespaces. Returns its exit code.
pub fn run(cfg: &Config) -> Result<i32> {
    let plan = plan(cfg)?;
    run_with_plan(cfg, plan)
}

/// One build step (`RUN`): run `cmd` over `layers`, collecting every change in the `upper` directory.
pub struct Step<'a> {
    pub layers: &'a [std::path::PathBuf],
    pub upper: &'a Path,
    pub work: &'a Path,
    pub cmd: &'a [String],
    pub env: &'a [String],
    pub workdir: &'a str,
    pub user: &'a str,
    /// Extra bind mounts (`RUN --mount`).
    pub volumes: &'a [Volume],
}

pub fn run_step(s: &Step) -> Result<i32> {
    let cfg = Config { ephemeral: true, cmd: s.cmd.to_vec(), ..Config::default() };
    let plan = Plan {
        source: Source::Step { layers: s.layers.to_vec(), upper: s.upper.into(), work: s.work.into() },
        cmd: s.cmd.to_vec(),
        env: with_default_path(s.env.to_vec()),
        workdir: s.workdir.into(),
        volumes: s.volumes.to_vec(),
        user: s.user.into(),
        security: Security::default(),
        health: None,
    };
    run_with_plan(&cfg, plan)
}

fn run_with_plan(cfg: &Config, plan: Plan) -> Result<i32> {
    let result = run_container(cfg, plan);
    if SUPERVISOR.load(std::sync::atomic::Ordering::Relaxed) {
        // everything has been cleaned up by now (destructors ran when run_container returned)
        match &result {
            Ok(code) => process::exit(*code),
            Err(e) => {
                eprintln!("docker: {e}");
                process::exit(1);
            }
        }
    }
    result
}

fn run_container(cfg: &Config, mut plan: Plan) -> Result<i32> {
    let id = cfg.preset_id.clone().unwrap_or_else(registry::new_id);
    if cfg.name.as_deref().is_some_and(|n| registry::name_taken_by_other(n, &id)) {
        return Err(Error::Usage("container name is already in use".into()));
    }
    if cfg.preset_id.is_some() {
        registry::clear_stop(&id); // a stop requested for the previous run must not stop this one
    }
    let ready = if cfg.detach {
        match detach(&id)? {
            Some(w) => Some(w),
            None => {
                println!("{id}");
                return Ok(0);
            }
        }
    } else {
        None
    };

    let mut mode = cfg.network.unwrap_or(Network::Bridge);
    if mode != Network::Bridge && !cfg.ports.is_empty() {
        return Err(Error::Usage("-p needs --network bridge".into()));
    }
    // rootless: no host bridge. A user-space helper (slirp4netns/pasta) serves the default network;
    // without one the container shares the host's network namespace.
    let rl = registry::rootless();
    let mut rl_helper = None;
    let mut rl_map = None;
    if rl {
        let m = rootless::detect();
        rootless::describe(&m);
        rl_map = Some(m);
        if mode == Network::Bridge {
            rl_helper = rootless::net::detect();
            if cfg.network_name.is_some() {
                rootless::warn("user-defined networks are not available rootless; using the default rootless network");
            }
            if rl_helper.is_none() {
                rootless::warn("neither slirp4netns nor pasta found: sharing the HOST network namespace (like --network host)");
                if let Some(p) = cfg.ports.iter().find(|p| p.host != p.container) {
                    return Err(Error::Usage(format!(
                        "-p {}:{} needs slirp4netns or pasta (with the host network only identical ports like -p {}:{} work)",
                        p.host, p.container, p.container, p.container
                    )));
                }
                mode = Network::Host;
            }
        }
        let _ = fs::create_dir_all(rootless::run_root());
    }
    let _cleanup = RunDirs(id.clone());
    let listeners = if cfg.ports.is_empty() || rl { None } else { Some(net::bind_ports(&cfg.ports)?) };
    let lease = if mode == Network::Bridge && !rl { Some(net::Lease::acquire()?) } else { None };
    let ip = lease.as_ref().map(|l| l.ip);
    if mode != Network::None {
        // DNS: a generated resolv.conf bind-mounted into the container
        let dir = RunDirs::etc(&id);
        fs::create_dir_all(&dir).map_err(|e| Error::io("mkdir run dir", e))?;
        let host = fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        let file = dir.join("resolv.conf");
        let text = if rl_helper.is_some() { format!("nameserver {}\n", rootless::net::DNS) } else { net::resolv_conf(&host, mode == Network::Bridge) };
        fs::write(&file, text).map_err(|e| Error::io("write resolv.conf", e))?;
        plan.volumes.push(Volume { host: file.to_string_lossy().into_owned(), container: "/etc/resolv.conf".into(), ro: true });
    }
    if let (Network::Bridge, Some(ip)) = (mode, ip) {
        // /etc/hosts: the container's own name, plus its peers on a user-defined network
        if let Some(n) = &cfg.network_name
            && !crate::networks::exists(n)
        {
            return Err(Error::Usage(format!("network {n:?} does not exist: run `docker network create {n}`")));
        }
        let mut own_names: Vec<String> = cfg.name.iter().cloned().collect();
        own_names.push(id.chars().take(12).collect());
        own_names.extend(cfg.aliases.iter().cloned());
        let own = crate::networks::Host { ip: ip.to_string(), names: own_names };
        let peers: Vec<_> = crate::networks::members(cfg.network_name.as_deref(), &id).iter().filter_map(crate::networks::Host::of).collect();
        let file = RunDirs::etc(&id).join("hosts");
        fs::write(&file, crate::networks::hosts_file(&own, &peers)).map_err(|e| Error::io("write hosts", e))?;
        plan.volumes.push(Volume { host: file.to_string_lossy().into_owned(), container: "/etc/hosts".into(), ro: true });
    }

    let health = plan.health.clone();
    let mut lease = lease;
    let mut ready = ready;
    let mut listeners = listeners;
    let mut reg: Option<registry::Registration> = None;
    let mut helpers: Vec<i32> = vec![];
    let mut restarts = 0u32;
    // stdin for a detached container without a terminal: a pipe a helper feeds from the `<id>.in` socket
    let (stdin_rd, mut stdin_wr) = if cfg.detach && cfg.interactive && !cfg.tty && !cfg.ephemeral {
        let (r, w) = sys::pipe()?;
        (Some(r), Some(w))
    } else {
        (None, None)
    };
    let code = loop {
        let cgroup = Cgroup::create(&id, cfg)?;
        let helper_net = rl_helper.is_some();
        let (rd, wr) = sys::pipe()?; // parent -> child: "you are in the cgroup, go"
        let (nr_r, nr_w) = sys::pipe()?; // child -> parent: "my network namespace exists"
        let (nd_r, nd_w) = sys::pipe()?; // parent -> child: "your veth is in place"
        let tty = if cfg.tty { Some(pty::link()?) } else { None }; // (supervisor end, container end)

        // The child gets its own pid namespace at birth and unshares the rest itself, so this process
        // stays in the host namespaces. It also stays single-threaded (the -p forwarder and the health
        // monitor are helper *processes*), which makes forking the next run after a restart safe.
        let forked = if rl { sys::fork_ns(libc::CLONE_NEWUSER | libc::CLONE_NEWPID)? } else { sys::fork_newpid()? };
        let code = match forked {
            sys::Fork::Child => {
                drop((wr, nr_r, nd_w, ready));
                if let Some(r) = &stdin_rd
                    && sys::dup2(r.as_raw_fd(), 0).is_err()
                {
                    process::exit(127);
                }
                let link = tty.map(|(_, c)| c);
                let net = ChildNet { mode, ip, helper: helper_net, ready: nr_w, go: nd_r };
                let err = child(&plan, &id, cfg.rw, rd, net, link).unwrap_err();
                eprintln!("docker: {err}");
                process::exit(127);
            }
            sys::Fork::Parent(pid) => {
                drop((rd, nr_w, nd_r));
                let link = tty.map(|(p, _)| p);
                if let Some(m) = &rl_map {
                    // the child is a stranger in its new user namespace until its id maps exist
                    if let Err(e) = rootless::write_maps(pid, m) {
                        sys::kill(pid);
                        let _ = sys::wait(pid);
                        return Err(e);
                    }
                }
                cgroup.add_pid(pid)?;
                drop(wr); // child is released once it is inside the cgroup
                if ip.is_some() || helper_net {
                    let mut b = [0u8; 1];
                    // the child reports once its network namespace exists; EOF means it already died
                    if fs::File::from(nr_r).read(&mut b).unwrap_or(0) == 1 {
                        let attached = match rl_helper {
                            Some(h) => rootless::net::start(h, pid as u32, &cfg.ports, &RunDirs::etc(&id)).map(|hp| helpers.push(hp)),
                            None => net::attach(pid as u32, &id),
                        };
                        if let Err(e) = attached {
                            sys::kill(pid);
                            let _ = sys::wait(pid);
                            return Err(e);
                        }
                        let _ = fs::File::from(nd_w).write_all(&[1]); // dropping it instead tells the child "abort"
                    }
                }
                if restarts == 0 {
                    let mut entry = Entry::new(&id, cfg, pid as u32);
                    entry.env = plan.env.clone();
                    entry.workdir = plan.workdir.clone();
                    entry.user = plan.user.clone();
                    entry.security = Some(plan.security.clone());
                    entry.ip = ip.map(|i| i.to_string());
                    entry.health = health.as_ref().map(|_| "starting".to_string());
                    reg = if cfg.ephemeral { None } else { Some(registry::register(&entry)?) };
                    drop(lease.take());
                    if let Some(n) = &cfg.network_name {
                        net::refresh_hosts(n); // tell the other members about us
                    }
                    if let Some(w) = ready.take() {
                        let _ = fs::File::from(w).write_all(&[1]); // launcher may now print the id
                    }
                    if let (Some(l), Some(ip)) = (listeners.take(), ip) {
                        helpers.push(sys::spawn_helper(move || l.serve_forever(ip))?);
                    }
                    if let Some(w) = stdin_wr.take() {
                        let sid = id.clone();
                        helpers.push(sys::spawn_helper(move || stdin::serve(&sid, w))?);
                    }
                    if let (Some(h), false) = (&health, cfg.ephemeral) {
                        let (hid, hc) = (id.clone(), h.clone());
                        helpers.push(sys::spawn_helper(move || health::monitor(&hid, &hc))?);
                    }
                } else {
                    let n = restarts;
                    registry::update(&id, |e| {
                        e.pid = pid as u32;
                        e.restarting = false;
                        e.exit_code = None;
                        e.finished_unix = None;
                        e.restarts = n;
                    })?;
                }
                // the container creates its terminal in its own /dev/pts and sends us the master end
                match link.and_then(pty::receive_master) {
                    Some(m) => pty::relay(m, pid, !cfg.detach, &id),
                    None => sys::wait(pid),
                }
            }
        };
        drop(cgroup);
        let exit = *code.as_ref().unwrap_or(&-1);
        if cfg.ephemeral || !cfg.restart.should_restart(exit, restarts, registry::stop_requested(&id)) {
            break code;
        }
        // restart policy: back off (100ms, 200ms, ... capped at 10s), but give up the moment someone says stop
        restarts += 1;
        let n = restarts;
        let _ = registry::update(&id, |e| {
            e.restarting = true;
            e.exit_code = None;
            e.restarts = n;
        });
        for _ in 0..(100u64 << restarts.min(7)).min(10_000) / 100 {
            if registry::stop_requested(&id) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if registry::stop_requested(&id) {
            break code;
        }
    };
    for h in helpers {
        sys::kill(h);
        let _ = sys::wait(h);
    }
    if let Some(r) = reg {
        r.finish(*code.as_ref().unwrap_or(&-1));
    }
    if cfg.auto_remove && !cfg.ephemeral {
        if cfg.detach {
            // a client that attached before the start (as `docker run` does) is still reading the log
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        registry::delete(&id);
    }
    if let Some(n) = &cfg.network_name {
        net::refresh_hosts(n); // ... and that we are gone
    }
    code
}

/// Per-container scratch dirs under /run, removed however `run` ends.
struct RunDirs(String);

impl RunDirs {
    fn etc(id: &str) -> std::path::PathBuf {
        net::etc_dir(id)
    }
}

impl Drop for RunDirs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(rootless::run_root().join(&self.0));
        let _ = fs::remove_dir_all(Self::etc(&self.0));
        let _ = fs::remove_file(stdin::stdin_sock(&self.0));
    }
}

struct ChildNet {
    mode: Network,
    ip: Option<std::net::Ipv4Addr>,
    /// Rootless: a slirp4netns/pasta helper configures the namespace from outside.
    helper: bool,
    ready: OwnedFd,
    go: OwnedFd,
}

/// Replace the process environment (used right before exec).
pub(super) fn apply_env(env: &[String]) {
    // SAFETY: only called in freshly forked children, which are single-threaded until exec.
    unsafe {
        for (k, _) in std::env::vars_os().collect::<Vec<_>>() {
            std::env::remove_var(k);
        }
        for kv in env {
            if let Some((k, v)) = kv.split_once('=') {
                std::env::set_var(k, v);
            }
        }
    }
}

/// PID 1 of the container. Only returns on error.
fn child(plan: &Plan, id: &str, rw: bool, gate: OwnedFd, net: ChildNet, tty: Option<OwnedFd>) -> Result<()> {
    let _ = fs::File::from(gate).read(&mut [0u8; 1]); // EOF == parent is done
    let mut ns = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC;
    if net.mode != Network::Host {
        ns |= libc::CLONE_NEWNET;
    }
    sys::unshare(ns)?;
    match (net.mode, net.ip) {
        (Network::Bridge, ip) if ip.is_some() || net.helper => {
            fs::File::from(net.ready).write_all(&[1]).map_err(|e| Error::io("network handshake", e))?;
            if fs::File::from(net.go).read(&mut [0u8; 1]).unwrap_or(0) != 1 {
                return Err(Error::Usage("network setup failed on the host side".into()));
            }
            match ip {
                Some(ip) => net::configure_inside(ip)?,
                None => net::loopback_up()?, // the helper configured the tap device and routes
            }
        }
        (Network::None, _) => net::loopback_up()?,
        _ => {}
    }
    rootfs::enter(&plan.source, id, rw, &plan.volumes)?;
    std::env::set_current_dir(&plan.workdir).map_err(|e| Error::io(format!("workdir {}", plan.workdir), e))?;
    apply_env(&plan.env);
    if let Some(link) = tty {
        pty::setup_in_container(link)?; // /dev/pts exists now; the terminal becomes stdin/stdout/stderr
    }
    harden::finalize(&plan.user, &plan.security)?;
    Err(sys::execvp(&plan.cmd))
}

pub fn stats(id: &str) -> Option<Stats> {
    let d = cgroup::dir(id);
    if registry::rootless() && !d.join("cpu.stat").exists() {
        return rootless::proc_stats(registry::find(id).ok()?.pid);
    }
    let read = |f: &str| fs::read_to_string(d.join(f)).ok();
    let cpu_us = read("cpu.stat")?.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse::<u64>().ok())?;
    Some(Stats {
        mem_bytes: read("memory.current")?.trim().parse().ok()?,
        cpu_ms: cpu_us / 1000,
        procs: read("cgroup.procs")?.lines().count() as u32,
    })
}

pub fn stop(id: &str) -> Result<()> {
    if registry::rootless() && !cgroup::dir(id).join("cgroup.kill").exists() {
        // no delegated cgroup: killing the container's init takes its whole pid namespace with it
        let e = registry::find(id)?;
        return if e.running() { sys::signal(e.pid as i32, libc::SIGKILL) } else { Ok(()) };
    }
    cgroup::kill(id)
}

/// Send `sig` to the container's first process (what `docker stop` does before it gives up and kills).
pub fn signal(e: &Entry, sig: i32) -> Result<()> {
    if !e.running() {
        return Err(Error::Usage(format!("container {} is not running", e.label())));
    }
    sys::signal(e.pid as i32, sig)
}

pub fn update(id: &str, mem_bytes: Option<u64>, cpu_percent: Option<u32>, procs: Option<u32>) -> Result<()> {
    cgroup::update(id, mem_bytes, cpu_percent, procs)
}

pub use pty::attach_sock;

pub fn freeze(id: &str, on: bool) -> Result<()> {
    cgroup::freeze(id, on)
}

pub fn top(id: &str) -> Result<Vec<ProcInfo>> {
    let procs = fs::read_to_string(cgroup::dir(id).join("cgroup.procs")).map_err(|e| Error::io("read cgroup.procs", e))?;
    Ok(procs
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // fields after the `(comm)`: state ppid ...
            let ppid = stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()?;
            let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let uid = status.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().next()?.parse().ok()?;
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let cmd = String::from_utf8_lossy(&cmdline).replace('\0', " ").trim().to_string();
            Some(ProcInfo { pid, ppid, uid, cmd })
        })
        .collect())
}
