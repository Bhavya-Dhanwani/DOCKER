//! Single-node swarm: `docker swarm`, `docker service`, `docker stack`.
//!
//! Desired state (services) lives in `<data>/swarm/state.json`, guarded by an flock like the container
//! registry. Actual state is the set of containers named `<service>.<slot>.<id>` (and carrying a
//! `SWARM_SERVICE=<service>` variable) in the normal registry, so they show up in `docker ps`.
//!
//! There is no always-on daemon, so `swarm init` starts one detached *supervisor* process (this same
//! binary, `docker swarm __supervisor`). Every half second it diffs desired against actual state
//! (`reconcile`, a pure function) and starts, replaces or removes replicas. It also runs a round-robin TCP
//! forwarder per published port, in front of the replicas' bridge addresses. It exits when the state file
//! is gone or marked `leaving` (`swarm leave`). Any mutating command restarts it if it died.
use crate::{commands, compose};
use docker_core::{
    cli::{is_volume_name, Command, Config, Network, Port, Restart},
    networks, platform, registry::{self, Entry},
};
use docker_image::{dockerfile::parse_duration, Health, Reference, Store};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    error::Error,
    fs,
    io::{self, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Command as Proc, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

type Res<T = ()> = Result<T, Box<dyn Error>>;

/// Environment variable that marks a container as a replica of a service.
const MARK: &str = "SWARM_SERVICE";
/// SIGTERM-to-SIGKILL grace when a replica is replaced or removed.
const STOP_GRACE_S: u64 = 2;
const TICK: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------- desired state

/// When does the reconciler replace a replica that is not running?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Cond {
    Any,
    OnFailure,
    Never,
}

impl Cond {
    fn parse(s: &str) -> Res<Cond> {
        Ok(match s {
            "any" => Cond::Any,
            "on-failure" => Cond::OnFailure,
            "none" => Cond::Never,
            _ => return Err("--restart-condition must be any, on-failure or none".into()),
        })
    }

    fn replaces(self, exit: Option<i32>) -> bool {
        match self {
            Cond::Any => true,
            Cond::OnFailure => exit != Some(0),
            Cond::Never => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Spec {
    name: String,
    /// The stack that owns the service (`docker stack deploy`).
    stack: Option<String>,
    /// Template for every replica: image, cmd, env, published ports (served by the balancer, not by the
    /// replicas), network, limits, health check.
    cfg: Config,
    replicas: u32,
    cond: Cond,
    /// Replicas replaced per step of a rolling update, and the pause between steps.
    parallelism: u32,
    delay_s: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    id: String,
    /// Set by `swarm leave`: the supervisor removes every replica and exits.
    leaving: bool,
    services: BTreeMap<String, Spec>,
}

fn dir() -> PathBuf {
    registry::data_dir().join("swarm")
}

fn load() -> Option<State> {
    serde_json::from_slice(&fs::read(dir().join("state.json")).ok()?).ok()
}

fn save(s: &State) -> Res {
    let tmp = dir().join(format!("state.tmp{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    fs::rename(&tmp, dir().join("state.json"))?; // readers never see half a file
    Ok(())
}

/// Exclusive cross-process lock for read-modify-write of the state; released when dropped.
fn lock() -> Res<fs::File> {
    fs::create_dir_all(dir())?;
    let f = fs::OpenOptions::new().create(true).append(true).open(dir().join("state.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    }
    Ok(f)
}

const NOT_ACTIVE: &str = "this node is not part of a swarm: run `docker swarm init`";

fn with_state<T>(f: impl FnOnce(&mut State) -> Res<T>) -> Res<T> {
    let _guard = lock()?;
    let mut st = load().ok_or(NOT_ACTIVE)?;
    let out = f(&mut st)?;
    save(&st)?;
    Ok(out)
}

// ---------------------------------------------------------------- replicas (tasks)

/// `<service>.<slot>.<id>` -> (service, slot), only for containers that carry the swarm marker.
fn parse_task(e: &Entry) -> Option<(String, u32)> {
    let mut it = e.name.as_deref()?.rsplitn(3, '.');
    let (id, slot, svc) = (it.next()?, it.next()?, it.next()?);
    let marked = e.config.as_ref()?.env.iter().any(|v| v == &format!("{MARK}={svc}"));
    if !(marked && id.len() == 6 && id.bytes().all(|b| b.is_ascii_hexdigit())) {
        return None;
    }
    Some((svc.to_string(), slot.parse().ok()?))
}

fn all_tasks() -> Vec<(String, u32, Entry)> {
    registry::list().into_iter().filter_map(|e| parse_task(&e).map(|(s, n)| (s, n, e))).collect()
}

fn tasks_of(service: &str) -> Vec<(u32, Entry)> {
    let mut v: Vec<(u32, Entry)> = all_tasks().into_iter().filter(|(s, ..)| s == service).map(|(_, n, e)| (n, e)).collect();
    v.sort_by_key(|(n, e)| (*n, e.started_unix));
    v
}

/// The container config of replica `slot`.
fn task_cfg(spec: &Spec, slot: u32) -> Config {
    let mut c = spec.cfg.clone();
    c.detach = true;
    c.ports.clear();
    c.restart = Restart::No; // the reconciler decides about restarts
    c.name = Some(format!("{}.{slot}.{}", spec.name, &registry::new_id()[..6]));
    if !c.aliases.contains(&spec.name) {
        c.aliases.push(spec.name.clone());
    }
    c.env.push(format!("{MARK}={}", spec.name));
    c
}

/// Was this container made from the current spec? (Anything else is "outdated": a rolling update replaces it.)
fn is_current(spec: &Spec, c: &Config) -> bool {
    let env: Vec<&String> = c.env.iter().filter(|e| !e.starts_with(&format!("{MARK}="))).collect();
    let (a, b) = (&spec.cfg, c);
    a.target == b.target
        && a.cmd == b.cmd
        && a.entrypoint == b.entrypoint
        && a.mem_bytes == b.mem_bytes
        && a.cpu_percent == b.cpu_percent
        && a.network_name == b.network_name
        && a.health == b.health
        && a.env.iter().eq(env)
}

/// What the reconciler sees of one replica.
#[derive(Debug, Clone)]
struct Task {
    id: String,
    slot: u32,
    running: bool,
    exit: Option<i32>,
    current: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Act {
    Start(u32),
    Remove(String),
    /// Rolling update step: stop the outdated replica, start a fresh one in its slot.
    Replace(String, u32),
}

/// Pure diff of desired (`spec`) against actual (`tasks`). `budget` is how many outdated replicas this
/// call may replace (0 while the update delay has not passed).
fn reconcile(spec: &Spec, tasks: &[Task], mut budget: usize) -> Vec<Act> {
    let mut acts = vec![];
    for t in tasks.iter().filter(|t| t.slot == 0 || t.slot > spec.replicas) {
        acts.push(Act::Remove(t.id.clone())); // scaled down
    }
    for slot in 1..=spec.replicas {
        let mine: Vec<&Task> = tasks.iter().filter(|t| t.slot == slot).collect();
        match mine.iter().find(|t| t.running) {
            Some(live) => {
                acts.extend(mine.iter().filter(|t| t.id != live.id).map(|t| Act::Remove(t.id.clone())));
                if !live.current && budget > 0 {
                    budget -= 1;
                    acts.push(Act::Replace(live.id.clone(), slot));
                }
            }
            // nothing alive here: (re)start unless the restart condition says to leave the dead one alone
            None if mine.iter().all(|t| spec.cond.replaces(t.exit)) => {
                acts.extend(mine.iter().map(|t| Act::Remove(t.id.clone())));
                acts.push(Act::Start(slot));
            }
            None => {}
        }
    }
    acts
}

fn task_view(spec: &Spec, slot: u32, e: &Entry) -> Task {
    Task { id: e.id.clone(), slot, running: e.running(), exit: e.exit_code, current: e.config.as_ref().is_some_and(|c| is_current(spec, c)) }
}

/// `docker run -d` for one replica (a child process of this binary, so the supervisor stays single-threaded
/// with respect to the container machinery). Returns the container id.
fn start_task(spec: &Spec, slot: u32) -> Res<String> {
    let args = Command::Run(task_cfg(spec, slot)).to_args();
    let out = Proc::new(std::env::current_exe()?).args(args).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(format!("start {}.{slot}: {}", spec.name, String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Stop and delete replicas (all at once, so removing a big service does not take `n` grace periods).
fn remove_tasks(ids: &[String]) {
    let entries: Vec<Entry> = ids.iter().filter_map(|i| registry::find(i).ok()).collect();
    for e in &entries {
        let _ = registry::request_stop(&e.id); // a restart policy must not revive it
        if e.running() {
            let _ = platform::signal(e, 15);
        }
    }
    let wait = |tenths: u32, done: &dyn Fn(&Entry) -> bool| {
        for _ in 0..tenths {
            if entries.iter().all(done) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    };
    wait(STOP_GRACE_S as u32 * 10, &|e| !e.running());
    for e in entries.iter().filter(|e| e.running()) {
        let _ = platform::stop(&e.id);
    }
    // wait until the container's own supervisor has written the exit code: deleting before that would
    // let it re-create the record
    wait(50, &|e| registry::find(&e.id).map_or(true, |x| x.exit_code.is_some()));
    for e in &entries {
        registry::delete(&e.id);
    }
}

/// After a replacement: hold until the new replica is up and, if it has a health check, healthy.
fn wait_ready(service: &str, id: &str) {
    for _ in 0..600 {
        if load().is_none_or(|s| s.leaving || !s.services.contains_key(service)) {
            return;
        }
        match registry::find(id) {
            Ok(e) if e.running() && e.health.as_deref() == Some("starting") => thread::sleep(Duration::from_millis(100)),
            _ => return, // up and healthy (or no check), or it died: the next tick deals with it
        }
    }
}

// ---------------------------------------------------------------- port balancer

/// Listens on a published host port and spreads connections over the replicas, round robin.
struct Balancer {
    backends: Arc<Mutex<Vec<SocketAddr>>>,
    stop: Arc<AtomicBool>,
}

impl Balancer {
    fn start(port: u16) -> io::Result<Balancer> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        listener.set_nonblocking(true)?;
        let b = Balancer { backends: Arc::default(), stop: Arc::default() };
        let (backends, stop, next) = (b.backends.clone(), b.stop.clone(), Arc::new(AtomicUsize::new(0)));
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let (backends, next) = (backends.clone(), next.clone());
                        thread::spawn(move || proxy(conn, &backends, &next));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(20)),
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        });
        Ok(b)
    }
}

/// Connect to the next replica that answers and copy bytes both ways until both sides are done.
fn proxy(client: TcpStream, backends: &Mutex<Vec<SocketAddr>>, next: &AtomicUsize) {
    let list = backends.lock().map(|l| l.clone()).unwrap_or_default();
    if list.is_empty() {
        return;
    }
    let start = next.fetch_add(1, Ordering::Relaxed);
    let Some(up) = (0..list.len()).find_map(|i| TcpStream::connect_timeout(&list[(start + i) % list.len()], Duration::from_secs(2)).ok()) else { return };
    let _ = client.set_nonblocking(false);
    let (Ok(mut c2), Ok(mut u2)) = (client.try_clone(), up.try_clone()) else { return };
    let (mut client, mut up) = (client, up);
    let t = thread::spawn(move || {
        let _ = io::copy(&mut c2, &mut u2);
        let _ = u2.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut up, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = t.join();
}

/// Addresses of the replicas that can take traffic for `container_port`.
fn backends_of(service: &str, container_port: u16) -> Vec<SocketAddr> {
    tasks_of(service)
        .into_iter()
        .filter(|(_, e)| e.running() && matches!(e.health.as_deref(), None | Some("healthy")))
        .filter_map(|(_, e)| Some(SocketAddr::new(e.ip.as_deref()?.parse::<IpAddr>().ok()?, container_port)))
        .collect()
}

// ---------------------------------------------------------------- the supervisor

fn pid_file() -> PathBuf {
    dir().join("supervisor.pid")
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
        && fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains("__supervisor"))
}

#[cfg(not(unix))]
fn alive(_: u32) -> bool {
    false
}

fn supervisor_pid() -> Option<u32> {
    fs::read_to_string(pid_file()).ok()?.trim().parse().ok().filter(|p| alive(*p))
}

/// Start the reconciler unless it is already running (it dies with the machine, so every command checks).
fn ensure_supervisor() -> Res {
    let _guard = lock()?;
    if supervisor_pid().is_some() {
        return Ok(());
    }
    let log = fs::OpenOptions::new().create(true).append(true).open(dir().join("supervisor.log"))?;
    let mut c = Proc::new(std::env::current_exe()?);
    c.args(["swarm", "__supervisor"]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        c.pre_exec(|| {
            libc::setsid(); // survive the terminal, like `run -d`
            Ok(())
        });
    }
    let child = c.spawn()?;
    fs::write(pid_file(), child.id().to_string())?;
    Ok(())
}

fn supervisor() -> Res {
    let me = std::process::id();
    let mut balancers: HashMap<(String, u16), Balancer> = HashMap::new();
    let mut next_step: HashMap<String, Instant> = HashMap::new(); // rolling update pacing
    let mut retry_at: HashMap<String, Instant> = HashMap::new(); // back-off after a failed start
    // ponytail: a crash-looping replica is replaced every tick; add a per-slot delay if that ever matters
    while let Some(st) = load() {
        if st.leaving {
            remove_tasks(&all_tasks().into_iter().map(|(.., e)| e.id).collect::<Vec<_>>());
            break;
        }
        let orphans: Vec<String> = all_tasks().into_iter().filter(|(s, ..)| !st.services.contains_key(s)).map(|(.., e)| e.id).collect();
        remove_tasks(&orphans);
        for spec in st.services.values() {
            let now = Instant::now();
            let tasks: Vec<Task> = tasks_of(&spec.name).iter().map(|(n, e)| task_view(spec, *n, e)).collect();
            let budget = if next_step.get(&spec.name).is_none_or(|t| now >= *t) { spec.parallelism.max(1) as usize } else { 0 };
            let (mut stepped, mut failed) = (false, false);
            for act in reconcile(spec, &tasks, budget) {
                match act {
                    Act::Remove(id) => remove_tasks(&[id]),
                    Act::Start(_) | Act::Replace(..) if retry_at.get(&spec.name).is_some_and(|t| now < *t) => {}
                    Act::Start(slot) => failed |= start_logged(spec, slot).is_none(),
                    Act::Replace(old, slot) => {
                        remove_tasks(&[old]);
                        match start_logged(spec, slot) {
                            Some(id) => wait_ready(&spec.name, &id),
                            None => failed = true,
                        }
                        stepped = true;
                    }
                }
            }
            if stepped {
                next_step.insert(spec.name.clone(), Instant::now() + Duration::from_secs(spec.delay_s));
            }
            if failed {
                retry_at.insert(spec.name.clone(), Instant::now() + Duration::from_secs(3));
            }
            for p in &spec.cfg.ports {
                let key = (spec.name.clone(), p.host);
                if !balancers.contains_key(&key) {
                    match Balancer::start(p.host) {
                        Ok(b) => {
                            balancers.insert(key.clone(), b);
                        }
                        Err(e) => eprintln!("swarm: cannot listen on port {}: {e}", p.host),
                    }
                }
                if let Some(b) = balancers.get(&key)
                    && let Ok(mut l) = b.backends.lock()
                {
                    *l = backends_of(&spec.name, p.container);
                }
            }
        }
        balancers.retain(|(svc, port), b| {
            let wanted = st.services.get(svc).is_some_and(|s| s.cfg.ports.iter().any(|p| p.host == *port));
            if !wanted {
                b.stop.store(true, Ordering::Relaxed);
            }
            wanted
        });
        thread::sleep(TICK);
    }
    for b in balancers.values() {
        b.stop.store(true, Ordering::Relaxed);
    }
    thread::sleep(Duration::from_millis(100)); // let the listeners close
    if fs::read_to_string(pid_file()).is_ok_and(|p| p.trim() == me.to_string()) {
        let _ = fs::remove_file(pid_file());
    }
    Ok(())
}

fn start_logged(spec: &Spec, slot: u32) -> Option<String> {
    match start_task(spec, slot) {
        Ok(id) => Some(id),
        Err(e) => {
            eprintln!("swarm: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------- commands

pub fn run(words: &[String]) -> Res {
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let rest = |n: usize| &words[n..];
    match w.as_slice() {
        ["swarm", "__supervisor"] => supervisor(),
        ["swarm", "init"] => init(),
        ["swarm", "leave"] | ["swarm", "leave", "-f" | "--force"] => leave(),
        ["swarm", "status"] => status(),
        ["swarm", "join", ..] | ["swarm", "join-token", ..] => Err("not supported: single-node only (swarm init makes this machine the one manager)".into()),
        ["service", "create", ..] => service_create(rest(2)),
        ["service", "ls" | "list"] => service_ls(None),
        ["service", "ps", name] => service_ps(&[name]),
        ["service", "inspect", name] => service_inspect(name),
        ["service", "logs", name] | ["service", "logs", "-f", name] => service_logs(name),
        ["service", "scale", ..] if words.len() > 2 => service_scale(rest(2)),
        ["service", "update", ..] => service_update(rest(2)),
        ["service", "rm" | "remove", ..] if words.len() > 2 => rest(2).iter().try_for_each(|n| remove_service(n)),
        ["stack", "deploy", ..] => stack_deploy(rest(2)),
        ["stack", "ls" | "list"] => stack_ls(),
        ["stack", "services", name] => service_ls(Some(name)),
        ["stack", "ps", name] => stack_ps(name),
        ["stack", "rm" | "remove", ..] if words.len() > 2 => rest(2).iter().try_for_each(|n| stack_rm(n)),
        _ => Err(format!("usage: see `docker` for swarm|service|stack commands (got {:?})", w.join(" ")).into()),
    }
}

fn init() -> Res {
    let _guard = lock()?;
    if load().is_some() {
        return Err("this node is already part of a swarm (run `docker swarm leave` first)".into());
    }
    let id = registry::new_id()[..12].to_string();
    save(&State { id: id.clone(), ..State::default() })?;
    drop(_guard);
    ensure_supervisor()?;
    println!("Swarm initialized: current node ({id}) is now a manager.");
    Ok(())
}

fn leave() -> Res {
    let stacks: Vec<String> = with_state(|st| {
        st.leaving = true;
        Ok(st.services.values().filter_map(|s| s.stack.clone()).collect())
    })?;
    // the supervisor removes the replicas and exits; wait for it, then make sure nothing is left
    for _ in 0..200 {
        if supervisor_pid().is_none() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    #[cfg(unix)]
    if let Some(pid) = supervisor_pid() {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    remove_tasks(&all_tasks().into_iter().map(|(.., e)| e.id).collect::<Vec<_>>());
    for s in stacks {
        let _ = networks::remove(&format!("{s}_default"));
    }
    fs::remove_dir_all(dir())?;
    println!("Node left the swarm.");
    Ok(())
}

fn status() -> Res {
    let Some(st) = load() else {
        println!("Swarm: inactive");
        return Ok(());
    };
    println!("Swarm: active\nNodeID: {}\nIsManager: true\nServices: {}", st.id, st.services.len());
    match supervisor_pid() {
        Some(p) => println!("Reconciler: running (pid {p})"),
        None => println!("Reconciler: not running (any service command starts it again)"),
    }
    Ok(())
}

// -- parsing service flags

/// `-e K=V` replaces an earlier `K`.
fn put_env(env: &mut Vec<String>, kv: String) {
    let key = kv.split('=').next().unwrap_or("").to_string();
    env.retain(|e| e.split('=').next() != Some(&key));
    env.push(kv);
}

fn seconds(v: &str) -> Res<u64> {
    v.parse().ok().or_else(|| parse_duration(v)).ok_or_else(|| format!("bad duration {v:?} (use 5 or 5s)").into())
}

fn number<T: std::str::FromStr + PartialOrd + From<u8>>(flag: &str, v: &str) -> Res<T> {
    v.parse().ok().filter(|n: &T| *n >= T::from(1)).ok_or_else(|| format!("{flag} needs a positive number").into())
}

/// Apply one `--flag value`. `create` selects the flags that only make sense at creation (`--name`, `-p`,
/// `--network`); the others (`--image`, `--env-rm`) only on update.
fn apply(spec: &mut Spec, flag: &str, v: String, create: bool) -> Res {
    match flag {
        "--name" if create => {
            if !is_volume_name(&v) {
                return Err("--name must match [a-zA-Z0-9][a-zA-Z0-9_.-]*".into());
            }
            spec.name = v;
        }
        "-p" | "--publish" if create => spec.cfg.ports.push(Port::parse(&v)?),
        "--network" | "--net" if create => {
            if !networks::exists(&v) {
                return Err(format!("network {v:?} does not exist: run `docker network create {v}`").into());
            }
            spec.cfg.network = Some(Network::Bridge);
            spec.cfg.network_name = Some(v);
        }
        "--image" if !create => spec.cfg.target = v,
        "--env-rm" if !create => spec.cfg.env.retain(|e| e.split('=').next() != Some(&v)),
        "-e" | "--env" | "--env-add" => put_env(&mut spec.cfg.env, v),
        "--replicas" => spec.replicas = v.parse().map_err(|_| "--replicas needs a number")?,
        "--restart-condition" => spec.cond = Cond::parse(&v)?,
        "--mem" => spec.cfg.mem_bytes = Some(number::<u64>(flag, &v)? << 20),
        "--cpu" => spec.cfg.cpu_percent = Some(number::<u32>(flag, &v)?.min(100)),
        "--update-delay" => spec.delay_s = seconds(&v)?,
        "--update-parallelism" => spec.parallelism = number(flag, &v)?,
        "--health-cmd" => spec.cfg.health = Some(Health::new(vec!["/bin/sh".into(), "-c".into(), v])),
        "--health-interval" => spec.cfg.health.as_mut().ok_or("--health-interval needs --health-cmd first")?.interval_s = seconds(&v)?,
        other => return Err(format!("unknown flag {other}").into()),
    }
    Ok(())
}

/// Leading `--flag value` pairs (`--flag=value` too); returns how many words they took.
fn parse_flags(args: &[String], spec: &mut Spec, create: bool) -> Res<usize> {
    let mut i = 0;
    while let Some(a) = args.get(i).filter(|a| a.starts_with('-')) {
        i += 1;
        if a == "-d" || a == "--detach" {
            continue; // creating and updating never wait for convergence here
        }
        let (flag, v) = match a.split_once('=').filter(|_| a.starts_with("--")) {
            Some((f, v)) => (f.to_string(), v.to_string()),
            None => {
                i += 1;
                (a.clone(), args.get(i - 1).ok_or(format!("{a} needs a value"))?.clone())
            }
        };
        apply(spec, &flag, v, create)?;
    }
    Ok(i)
}

fn new_spec() -> Spec {
    Spec {
        name: format!("svc-{}", &registry::new_id()[..6]),
        stack: None,
        cfg: Config::default(),
        replicas: 1,
        cond: Cond::Any,
        parallelism: 1,
        delay_s: 0,
    }
}

fn check_image(image: &str) -> Res {
    let store = Store::open(registry::data_dir().join("store"));
    if store.get(&Reference::parse(image)?)?.is_none() {
        return Err(format!("image {image} not found locally; run `docker pull {image}`").into());
    }
    Ok(())
}

fn add_service(spec: Spec) -> Res {
    check_image(&spec.cfg.target)?;
    let name = spec.name.clone();
    with_state(|st| {
        if st.services.contains_key(&name) {
            return Err(format!("service {name} already exists").into());
        }
        st.services.insert(name.clone(), spec);
        Ok(())
    })?;
    ensure_supervisor()
}

fn service_create(args: &[String]) -> Res {
    let mut spec = new_spec();
    let n = parse_flags(args, &mut spec, true)?;
    spec.cfg.target = args.get(n).ok_or("usage: docker service create [flags] <image> [command...]")?.clone();
    spec.cfg.cmd = args[n + 1..].to_vec();
    let name = spec.name.clone();
    add_service(spec)?;
    println!("{name}");
    Ok(())
}

fn service_update(args: &[String]) -> Res {
    let mut patch = new_spec();
    let n = parse_flags(args, &mut patch, false)?;
    let name = args.get(n).ok_or("usage: docker service update [flags] <name>")?;
    if args.len() > n + 1 {
        return Err("too many arguments".into());
    }
    // replay the flags on the stored spec (parsing needs a Spec to write into)
    with_state(|st| {
        let spec = st.services.get_mut(name).ok_or_else(|| format!("no such service: {name}"))?;
        parse_flags(&args[..n], spec, false)?;
        if args[..n].iter().any(|a| a == "--image" || a.starts_with("--image=")) {
            check_image(&spec.cfg.target)?;
        }
        Ok(())
    })?;
    ensure_supervisor()?;
    println!("{name}");
    Ok(())
}

fn service_scale(args: &[String]) -> Res {
    let pairs: Vec<(&str, u32)> = args
        .iter()
        .map(|a| a.split_once('=').and_then(|(n, r)| Some((n, r.parse().ok()?))).ok_or_else(|| format!("bad scale argument {a:?}: expected name=N")))
        .collect::<Result<_, _>>()?;
    with_state(|st| {
        for (n, r) in &pairs {
            st.services.get_mut(*n).ok_or_else(|| format!("no such service: {n}"))?.replicas = *r;
        }
        Ok(())
    })?;
    ensure_supervisor()?;
    for (n, r) in pairs {
        println!("{n} scaled to {r}");
    }
    Ok(())
}

fn remove_service(name: &str) -> Res {
    with_state(|st| st.services.remove(name).map(drop).ok_or_else(|| format!("no such service: {name}").into()))?;
    remove_tasks(&tasks_of(name).into_iter().map(|(_, e)| e.id).collect::<Vec<_>>());
    println!("{name}");
    Ok(())
}

// -- listings

fn services(stack: Option<&str>) -> Res<Vec<Spec>> {
    let st = load().ok_or(NOT_ACTIVE)?;
    Ok(st.services.into_values().filter(|s| stack.is_none_or(|n| s.stack.as_deref() == Some(n))).collect())
}

fn service_ls(stack: Option<&str>) -> Res {
    println!("{:<24} {:<12} {:<10} {:<20} PORTS", "NAME", "MODE", "REPLICAS", "IMAGE");
    for s in services(stack)? {
        let up = tasks_of(&s.name).iter().filter(|(_, e)| e.running()).count();
        let ports: Vec<String> = s.cfg.ports.iter().map(|p| format!("*:{}->{}/tcp", p.host, p.container)).collect();
        println!("{:<24} {:<12} {:<10} {:<20} {}", s.name, "replicated", format!("{up}/{}", s.replicas), s.cfg.target, ports.join(", "));
    }
    Ok(())
}

fn service_ps(names: &[&str]) -> Res {
    println!("{:<14} {:<34} {:<20} {:<16} NODE", "ID", "NAME", "IMAGE", "STATE");
    let now = registry::now_unix();
    for name in names {
        let spec = services(None)?.into_iter().find(|s| s.name == *name).ok_or_else(|| format!("no such service: {name}"))?;
        for (_, e) in tasks_of(&spec.name) {
            println!("{:<14} {:<34} {:<20} {:<16} {}", &e.id[..12], e.label(), e.image, commands::status_of(&e, now), "local");
        }
    }
    Ok(())
}

fn service_inspect(name: &str) -> Res {
    let spec = services(None)?.into_iter().find(|s| s.name == name).ok_or_else(|| format!("no such service: {name}"))?;
    let tasks: Vec<_> = tasks_of(name).iter().map(|(n, e)| serde_json::json!({ "slot": n, "id": e.id, "name": e.name, "running": e.running(), "ip": e.ip, "health": e.health })).collect();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "service": spec, "tasks": tasks }))?);
    Ok(())
}

/// Every replica's log, each line prefixed with the replica's name.
fn service_logs(name: &str) -> Res {
    services(None)?.iter().find(|s| s.name == name).ok_or_else(|| format!("no such service: {name}"))?;
    let mut out = io::stdout().lock();
    for (_, e) in tasks_of(name) {
        let text = fs::read(registry::log_path(&e.id)).unwrap_or_default();
        for line in String::from_utf8_lossy(&text).lines() {
            writeln!(out, "{} | {line}", e.label())?;
        }
    }
    Ok(())
}

// -- stacks

fn stack_deploy(args: &[String]) -> Res {
    let (file, name) = match args {
        [f, file, name] if f == "-c" || f == "--compose-file" => (file, name),
        _ => return Err("usage: docker stack deploy -c <compose.yml> <name>".into()),
    };
    if !is_volume_name(name) {
        return Err("a stack name must match [a-zA-Z0-9][a-zA-Z0-9_.-]*".into());
    }
    let specs: Vec<Spec> = compose::stack_services(file, name)?
        .into_iter()
        .map(|s| stack_spec(name, s))
        .collect();
    for s in &specs {
        check_image(&s.cfg.target)?;
        if let Some(n) = &s.cfg.network_name
            && !networks::exists(n)
        {
            networks::create(n)?;
        }
    }
    with_state(|_| Ok(()))?; // fail early when there is no swarm
    for s in specs {
        let name = s.name.clone();
        let existed = with_state(|st| Ok(st.services.insert(name.clone(), s).is_some()))?;
        println!("{} service {name}", if existed { "Updating" } else { "Creating" });
    }
    ensure_supervisor()
}

/// compose service -> swarm service `<stack>_<service>` (`deploy:` decides replicas, restart, rolling update, limits).
fn stack_spec(stack: &str, s: compose::StackService) -> Spec {
    let compose::StackService { name, mut cfg, deploy } = s;
    cfg.name = None;
    cfg.restart = Restart::No;
    cfg.aliases.push(format!("{stack}_{name}"));
    cfg.mem_bytes = deploy.mem_mb.map(|m| m << 20).or(cfg.mem_bytes);
    cfg.cpu_percent = deploy.cpu_percent.or(cfg.cpu_percent);
    Spec {
        name: format!("{stack}_{name}"),
        stack: Some(stack.to_string()),
        cfg,
        replicas: deploy.replicas.unwrap_or(1),
        cond: deploy.restart_condition.as_deref().and_then(|c| Cond::parse(c).ok()).unwrap_or(Cond::Any),
        parallelism: deploy.parallelism.unwrap_or(1),
        delay_s: deploy.delay_s.unwrap_or(0),
    }
}

fn stack_ls() -> Res {
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for s in services(None)? {
        if let Some(n) = s.stack {
            *count.entry(n).or_default() += 1;
        }
    }
    println!("{:<24} SERVICES", "NAME");
    for (n, c) in count {
        println!("{n:<24} {c}");
    }
    Ok(())
}

fn stack_ps(name: &str) -> Res {
    let names: Vec<String> = services(Some(name))?.into_iter().map(|s| s.name).collect();
    service_ps(&names.iter().map(String::as_str).collect::<Vec<_>>())
}

fn stack_rm(name: &str) -> Res {
    let names: Vec<String> = services(Some(name))?.into_iter().map(|s| s.name).collect();
    if names.is_empty() {
        return Err(format!("nothing found in stack: {name}").into());
    }
    names.iter().try_for_each(|n| remove_service(n))?;
    let _ = networks::remove(&format!("{name}_default"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(replicas: u32, cond: Cond) -> Spec {
        let mut s = new_spec();
        s.name = "web".into();
        s.cfg.target = "alpine".into();
        s.cfg.cmd = vec!["sleep".into(), "9".into()];
        s.replicas = replicas;
        s.cond = cond;
        s
    }

    fn task(id: &str, slot: u32, running: bool, exit: Option<i32>, current: bool) -> Task {
        Task { id: id.into(), slot, running, exit, current }
    }

    #[test]
    fn starts_missing_replicas_and_removes_extra_ones() {
        let s = spec(3, Cond::Any);
        assert_eq!(reconcile(&s, &[], 1), [Act::Start(1), Act::Start(2), Act::Start(3)]);
        let t = [task("a", 1, true, None, true), task("b", 2, true, None, true), task("c", 3, true, None, true), task("d", 4, true, None, true)];
        assert_eq!(reconcile(&s, &t, 1), [Act::Remove("d".into())]);
        assert!(reconcile(&s, &t[..3], 1).is_empty());
    }

    #[test]
    fn dead_replicas_follow_the_restart_condition() {
        let dead = [task("a", 1, false, Some(0), true)];
        assert_eq!(reconcile(&spec(1, Cond::Any), &dead, 0), [Act::Remove("a".into()), Act::Start(1)]);
        assert!(reconcile(&spec(1, Cond::OnFailure), &dead, 0).is_empty());
        assert!(reconcile(&spec(1, Cond::Never), &dead, 0).is_empty());
        let failed = [task("a", 1, false, Some(1), true)];
        assert_eq!(reconcile(&spec(1, Cond::OnFailure), &failed, 0), [Act::Remove("a".into()), Act::Start(1)]);
        assert!(reconcile(&spec(1, Cond::Never), &failed, 0).is_empty());
        // a vanished container (no exit code recorded) counts as a failure
        assert_eq!(reconcile(&spec(1, Cond::OnFailure), &[task("a", 1, false, None, true)], 0).len(), 2);
    }

    #[test]
    fn rolling_update_replaces_at_most_budget_outdated_replicas() {
        let s = spec(3, Cond::Any);
        let old = [task("a", 1, true, None, false), task("b", 2, true, None, false), task("c", 3, true, None, true)];
        assert_eq!(reconcile(&s, &old, 1), [Act::Replace("a".into(), 1)]);
        assert_eq!(reconcile(&s, &old, 2), [Act::Replace("a".into(), 1), Act::Replace("b".into(), 2)]);
        assert!(reconcile(&s, &old, 0).is_empty()); // update delay has not passed
    }

    #[test]
    fn current_means_same_settings_ignoring_the_marker() {
        let mut s = spec(1, Cond::Any);
        s.cfg.env = vec!["A=1".into()];
        let c = task_cfg(&s, 1);
        assert!(is_current(&s, &c));
        assert!(c.name.as_deref().is_some_and(|n| n.starts_with("web.1.")));
        assert!(c.detach && c.ports.is_empty() && c.aliases == ["web"]);
        let mut changed = s.clone();
        put_env(&mut changed.cfg.env, "A=2".into());
        assert!(!is_current(&changed, &c));
        changed = s.clone();
        changed.cfg.target = "alpine:3.19".into();
        assert!(!is_current(&changed, &c));
    }

    #[test]
    fn task_names_need_the_marker() {
        let mut s = spec(1, Cond::Any);
        s.cfg.ports = vec![];
        let cfg = task_cfg(&s, 7);
        let mut e = Entry::new("0123456789abcdef", &cfg, 1);
        assert_eq!(parse_task(&e), Some(("web".into(), 7)));
        e.config.as_mut().unwrap().env.clear();
        assert_eq!(parse_task(&e), None);
    }

    #[test]
    fn parses_service_flags() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let mut s = new_spec();
        let args = a("--name web --replicas 3 -p 8080:80 -e A=1 -e A=2 --restart-condition on-failure --mem 64 --cpu 50 --update-delay 5s --update-parallelism 2 --health-cmd true --health-interval 1 alpine sleep 9");
        let n = parse_flags(&args, &mut s, true).unwrap();
        assert_eq!(&args[n..], ["alpine", "sleep", "9"]);
        assert_eq!((s.name.as_str(), s.replicas, s.cond, s.parallelism, s.delay_s), ("web", 3, Cond::OnFailure, 2, 5));
        assert_eq!(s.cfg.env, ["A=2"]);
        assert_eq!((s.cfg.mem_bytes, s.cfg.cpu_percent, s.cfg.ports[0].host), (Some(64 << 20), Some(50), 8080));
        assert_eq!(s.cfg.health.as_ref().map(|h| h.interval_s), Some(1));
        // update-only and create-only flags are kept apart
        assert!(parse_flags(&a("--image x web"), &mut new_spec(), true).is_err());
        assert!(parse_flags(&a("-p 1:2 web"), &mut new_spec(), false).is_err());
        assert!(parse_flags(&a("--replicas"), &mut new_spec(), true).is_err());
        assert!(parse_flags(&a("--restart-condition sometimes x"), &mut new_spec(), true).is_err());
    }

    #[test]
    fn stack_service_takes_its_settings_from_deploy() {
        let ss = compose::StackService {
            name: "api".into(),
            cfg: Config { name: Some("shop-api-1".into()), target: "alpine".into(), aliases: vec!["api".into()], network_name: Some("shop_default".into()), ..Config::default() },
            deploy: compose::deploy(&serde_yaml::from_str("replicas: 2\nrestart_policy: {condition: none}\nupdate_config: {parallelism: 2, delay: 3s}\nresources: {limits: {memory: 64M}}").unwrap()).unwrap(),
        };
        let s = stack_spec("shop", ss);
        assert_eq!((s.name.as_str(), s.stack.as_deref(), s.replicas, s.cond, s.parallelism, s.delay_s), ("shop_api", Some("shop"), 2, Cond::Never, 2, 3));
        assert_eq!((s.cfg.name, s.cfg.mem_bytes, s.cfg.aliases), (None, Some(64 << 20), vec!["api".to_string(), "shop_api".into()]));
    }
}
