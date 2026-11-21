//! One small JSON file per container. They outlive the container (exit code kept) so `ps -a`,
//! `logs` and `rm` work, and so the CLI and GUI can see each other.
use crate::{cli::Config, error::{Error, Result}};
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::RandomState,
    env, fs,
    hash::{BuildHasher, Hasher},
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image: String,
    pub cmd: Vec<String>,
    pub mem_bytes: Option<u64>,
    pub cpu_percent: Option<u32>,
    pub started_unix: u64,
    /// Host pid of the container's first process.
    #[serde(default)]
    pub pid: u32,
    /// Final environment and working directory, so `exec` matches `run`.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub workdir: String,
    /// What `exec` must re-apply so it matches the container.
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub security: Option<crate::security::Security>,
    /// The settings it was started with, so `start`/`restart` can run it again.
    #[serde(default)]
    pub config: Option<Config>,
    /// Bridge-network address, and published ports as "host->container".
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    /// `starting` / `healthy` / `unhealthy`, when the container has a health check.
    #[serde(default)]
    pub health: Option<String>,
    /// How many times the supervisor restarted it, and whether it is between two runs right now.
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub restarting: bool,
    /// Made by `create` (API) and not started yet.
    #[serde(default)]
    pub created: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub finished_unix: Option<u64>,
}

impl Entry {
    pub fn new(id: &str, cfg: &Config, pid: u32) -> Self {
        Entry {
            id: id.into(),
            name: cfg.name.clone(),
            image: cfg.target.clone(),
            cmd: cfg.cmd.clone(),
            mem_bytes: cfg.mem_bytes,
            cpu_percent: cfg.cpu_percent,
            started_unix: now(),
            pid,
            env: vec![],
            workdir: String::new(),
            user: String::new(),
            security: None,
            config: Some(Config { ephemeral: false, ..cfg.clone() }),
            ip: None,
            health: None,
            restarts: 0,
            restarting: false,
            created: false,
            ports: cfg.ports.iter().map(|p| p.label()).collect(),
            exit_code: None,
            finished_unix: None,
        }
    }

    /// Still alive right now (not just "has no recorded exit").
    pub fn running(&self) -> bool {
        self.exit_code.is_none() && crate::platform::stats(&self.id).is_some()
    }

    /// Name if it has one, else the id.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn now_unix() -> u64 {
    now()
}

/// Where containers, images, logs and scratch data live. Override with `RUST_DOCKER_HOME`.
pub fn data_dir() -> PathBuf {
    if let Some(d) = env::var_os("RUST_DOCKER_HOME") {
        return PathBuf::from(d);
    }
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(env::temp_dir);
    base.join("rust-docker")
}

/// The append-only log of container lifecycle events (create, start, die, destroy) that the Engine API streams.
pub fn events_path() -> PathBuf {
    data_dir().join("events.jsonl")
}

/// Record one event. Best effort: a full disk must not stop a container.
pub fn emit(action: &str, e: &Entry, exit: Option<i32>) {
    use std::io::Write;
    let path = events_path();
    if fs::metadata(&path).is_ok_and(|m| m.len() > 4 << 20) {
        let _ = fs::rename(&path, path.with_extension("jsonl.old"));
    }
    let line = serde_json::json!({ "time": now(), "action": action, "id": e.id, "name": e.name, "image": e.image, "exitCode": exit });
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

fn dir() -> PathBuf {
    data_dir().join("containers")
}

/// Throwaway per-container temp dir (callers delete it on exit so nothing lingers on the SSD).
pub fn scratch_dir(id: &str) -> PathBuf {
    data_dir().join("scratch").join(id)
}

pub fn log_path(id: &str) -> PathBuf {
    data_dir().join("logs").join(format!("{id}.log"))
}

/// 16 hex chars, random from the first character so short prefixes stay unambiguous.
pub fn new_id() -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let mut h = RandomState::new().build_hasher(); // per-process random keys
    h.write_u64(nanos);
    h.write_u32(std::process::id());
    h.write_u32(SEQ.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", h.finish())
}

fn write(e: &Entry) -> Result<()> {
    let d = dir();
    fs::create_dir_all(&d).map_err(|x| Error::io(format!("mkdir {}", d.display()), x))?;
    let json = serde_json::to_vec_pretty(e).expect("entry serializes");
    // write + rename: a reader (the API, `ps`) must never see a half-written or empty record, which
    // would make the container vanish for a moment
    let tmp = d.join(format!("{}.tmp{}", e.id, std::process::id()));
    fs::write(&tmp, json).map_err(|x| Error::io("write registry", x))?;
    fs::rename(&tmp, d.join(format!("{}.json", e.id))).map_err(|x| Error::io("write registry", x))
}

/// Records the container's end. Dropped without `finish` (crash/early error) it records exit -1.
pub struct Registration {
    entry: Entry,
    done: bool,
}

pub fn register(e: &Entry) -> Result<Registration> {
    let existed = dir().join(format!("{}.json", e.id)).exists(); // created earlier (API/`create`), or brand new (`run`)?
    write(e)?;
    if !existed {
        emit("create", e, None);
    }
    emit("start", e, None);
    Ok(Registration { entry: e.clone(), done: false })
}

impl Registration {
    pub fn finish(mut self, code: i32) {
        self.mark(code);
    }

    fn mark(&mut self, code: i32) {
        let at = now();
        self.entry.created = false;
        // through `update`, so fields changed meanwhile (pid after a restart, health) are not overwritten
        let done = |e: &mut Entry| {
            e.exit_code = Some(code);
            e.finished_unix = Some(at);
            e.restarting = false;
            e.created = false;
        };
        if update(&self.entry.id, done).is_err() {
            done(&mut self.entry);
            let _ = write(&self.entry);
        }
        self.done = true;
        emit("die", &self.entry, Some(code));
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if !self.done {
            self.mark(-1);
        }
    }
}

/// Exclusive cross-process lock for one record (the supervisor, its health monitor and the CLI all update it).
struct RecordLock {
    #[cfg(unix)]
    _file: Option<fs::File>,
}

fn lock(id: &str) -> RecordLock {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let file = fs::OpenOptions::new().create(true).append(true).open(dir().join(format!("{id}.lock"))).ok();
        if let Some(f) = &file {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        }
        RecordLock { _file: file }
    }
    #[cfg(not(unix))]
    {
        let _ = id;
        RecordLock {}
    }
}

/// Read-modify-write one container record.
pub fn update(id: &str, f: impl FnOnce(&mut Entry)) -> Result<Entry> {
    let _guard = lock(id);
    let path = dir().join(format!("{id}.json"));
    let mut e: Entry = serde_json::from_slice(&fs::read(&path).map_err(|x| Error::io(format!("read {}", path.display()), x))?)
        .map_err(|x| Error::Usage(format!("corrupt record for {id}: {x}")))?;
    f(&mut e);
    write(&e)?;
    Ok(e)
}

pub fn rename(id: &str, new: &str) -> Result<()> {
    if name_taken(new) {
        return Err(Error::Usage(format!("container name {new:?} is already in use")));
    }
    update(id, |e| e.name = Some(new.to_string())).map(drop)
}

/// `docker stop`/`kill` leave a marker so a supervisor with a restart policy does not bring the container back.
pub fn request_stop(id: &str) -> Result<()> {
    fs::write(dir().join(format!("{id}.stop")), b"").map_err(|x| Error::io("write stop marker", x))
}

pub fn stop_requested(id: &str) -> bool {
    dir().join(format!("{id}.stop")).exists()
}

/// Forget a container: its record, log and scratch dir.
pub fn delete(id: &str) {
    if let Some(e) = list().into_iter().find(|e| e.id == id) {
        emit("destroy", &e, None);
    }
    let _ = fs::remove_file(dir().join(format!("{id}.json")));
    let _ = fs::remove_file(dir().join(format!("{id}.stop")));
    let _ = fs::remove_file(dir().join(format!("{id}.lock")));
    let _ = fs::remove_file(log_path(id));
    let _ = fs::remove_dir_all(scratch_dir(id));
}

/// All containers, oldest first. Unreadable files are skipped.
pub fn list() -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(dir()) else { return vec![] };
    let mut v: Vec<Entry> = rd
        .flatten()
        .filter_map(|f| serde_json::from_slice(&fs::read(f.path()).ok()?).ok())
        .collect();
    v.sort_by_key(|e| e.started_unix);
    v
}

/// Persist a container that exists but has not been started (API `create`).
pub fn create(cfg: &Config) -> Result<Entry> {
    let mut e = Entry::new(&new_id(), cfg, 0);
    e.created = true;
    write(&e)?;
    emit("create", &e, None);
    Ok(e)
}

pub fn clear_stop(id: &str) {
    let _ = fs::remove_file(dir().join(format!("{id}.stop")));
}

/// Is `name` used by a container other than `id`? (`start` re-registers the same container under its own name.)
pub fn name_taken_by_other(name: &str, id: &str) -> bool {
    list().iter().any(|e| e.name.as_deref() == Some(name) && e.id != id)
}

pub fn name_taken(name: &str) -> bool {
    list().iter().any(|e| e.name.as_deref() == Some(name))
}

/// Resolve a name, an id, or a unique id prefix.
pub fn find(query: &str) -> Result<Entry> {
    let all = list();
    if let Some(e) = all.iter().find(|e| e.name.as_deref() == Some(query) || e.id == query) {
        return Ok(e.clone());
    }
    let mut hits = all.into_iter().filter(|e| !query.is_empty() && e.id.starts_with(query));
    match (hits.next(), hits.next()) {
        (Some(e), None) => Ok(e),
        (Some(_), Some(_)) => Err(Error::Usage(format!("ambiguous container id {query:?}"))),
        _ => Err(Error::Usage(format!("no such container: {query}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_hex_and_random_first() {
        let ids: Vec<String> = (0..2000).map(|_| new_id()).collect();
        assert!(ids.iter().all(|i| i.len() == 16 && i.bytes().all(|b| b.is_ascii_hexdigit())));
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        // a 4-char prefix should already tell most ids apart
        let prefixes: std::collections::HashSet<_> = ids.iter().map(|i| &i[..4]).collect();
        assert!(prefixes.len() > 1000);
    }
}
