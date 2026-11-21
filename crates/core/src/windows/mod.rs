//! Windows backend: a process tree under a Job Object. Not a Linux-style sandbox:
//! it caps memory/CPU and kills the tree on exit, but does not isolate files, pids or network.
mod job;

use crate::{
    cli::Config,
    env,
    error::{Error, Result},
    registry::{self, Entry},
    stats::Stats,
};
use job::Job;
use std::{fs, os::windows::io::AsRawHandle, path::PathBuf, process::Command};

/// Per-container TEMP dir, deleted on exit so scratch files never accumulate on the SSD.
struct Scratch(PathBuf);

impl Scratch {
    fn create(id: &str) -> Result<Self> {
        let d = registry::scratch_dir(id);
        fs::create_dir_all(&d).map_err(|e| Error::io(format!("mkdir {}", d.display()), e))?;
        Ok(Scratch(d))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn run(cfg: &Config) -> Result<i32> {
    if cfg.cmd.is_empty() {
        return Err(Error::Usage("missing <command>".into()));
    }
    if cfg.network.is_some_and(|n| n != crate::cli::Network::Host) || !cfg.ports.is_empty() {
        return Err(Error::Usage("--network bridge/none and -p need the Linux backend (Windows shares the host network)".into()));
    }
    if cfg.privileged || !cfg.cap_add.is_empty() || !cfg.cap_drop.is_empty() || !cfg.security_opt.is_empty() {
        return Err(Error::Usage("capabilities and seccomp are Linux features".into()));
    }
    if cfg.tty || cfg.restart != crate::cli::Restart::No || cfg.health.is_some() {
        return Err(Error::Usage("-t, --restart and health checks need the Linux backend".into()));
    }
    if cfg.detach || !cfg.volumes.is_empty() {
        return Err(Error::Usage("-d and -v need the Linux backend".into()));
    }
    if cfg.name.as_deref().is_some_and(registry::name_taken) {
        return Err(Error::Usage("container name is already in use".into()));
    }
    let id = registry::new_id();
    let job = Job::create(&id, cfg)?;
    let scratch = Scratch::create(&id)?;
    let host: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
    let env = env::merge(&host, &cfg.env, |k| std::env::var(k).ok());
    let mut command = Command::new(&cfg.cmd[0]);
    command.args(&cfg.cmd[1..]).current_dir(&cfg.target).env("TEMP", &scratch.0).env("TMP", &scratch.0);
    for (k, v) in env.iter().filter_map(|kv| kv.split_once('=')).filter(|(k, _)| !k.is_empty() && *k != "TEMP" && *k != "TMP") {
        command.env(k, v);
    }
    let mut child = command.spawn().map_err(|e| Error::io(format!("start {}", cfg.cmd[0]), e))?;
    // ponytail: tiny window where the child runs before assignment; use CREATE_SUSPENDED if it matters
    job.assign(child.as_raw_handle())?;
    let reg = registry::register(&Entry::new(&id, cfg, child.id()))?;
    let status = child.wait().map_err(|e| Error::io("wait", e))?;
    let code = status.code().unwrap_or(1);
    reg.finish(code);
    Ok(code)
}

pub fn build(_: &crate::cli::BuildArgs, _: &mut dyn FnMut(String)) -> Result<docker_image::ImageMeta> {
    Err(Error::Usage("build needs the Linux backend (layers are overlayfs directories)".into()))
}

pub fn exec(_: &registry::Entry, _: &[String], _: bool) -> Result<i32> {
    Err(Error::Usage("exec needs the Linux backend (Windows containers are plain process trees)".into()))
}

pub fn stats(id: &str) -> Option<Stats> {
    Job::open(id)?.stats()
}

pub fn stop(id: &str) -> Result<()> {
    Job::open(id).ok_or_else(|| Error::Usage(format!("no running container {id}")))?.terminate()
}

pub fn signal(e: &crate::registry::Entry, _: i32) -> Result<()> {
    stop(&e.id)
}

pub fn update(_: &str, _: Option<u64>, _: Option<u32>, _: Option<u32>) -> Result<()> {
    Err(Error::Usage("update needs the Linux backend".into()))
}

pub fn freeze(_: &str, _: bool) -> Result<()> {
    Err(Error::Usage("pause needs the Linux backend".into()))
}

pub fn top(_: &str) -> Result<Vec<crate::stats::ProcInfo>> {
    Err(Error::Usage("top needs the Linux backend".into()))
}

pub fn cp(_: &crate::registry::Entry, _: crate::stats::CpDir, _: &str, _: &str) -> Result<()> {
    Err(Error::Usage("cp needs the Linux backend".into()))
}
