//! cgroup v2: the only hard RAM/CPU guarantee for a container.
use crate::{cli::Config, error::{Error, Result}};
use std::{fs, path::PathBuf};

const ROOT: &str = "/sys/fs/cgroup";

pub fn dir(id: &str) -> PathBuf {
    if crate::registry::rootless() {
        return super::rootless::cgroup::recall(id).unwrap_or_else(|| PathBuf::from("/nonexistent-rootless-cgroup"));
    }
    PathBuf::from(ROOT).join(format!("rust-docker-{id}"))
}

/// Freeze or thaw every process of the container (`docker pause`/`unpause`).
pub fn freeze(id: &str, on: bool) -> Result<()> {
    let p = dir(id).join("cgroup.freeze");
    fs::write(&p, if on { "1" } else { "0" }).map_err(|e| Error::io(format!("write {}", p.display()), e))
}

/// Change the limits of a running container (`docker update`). 0 removes a limit.
pub fn update(id: &str, mem_bytes: Option<u64>, cpu_percent: Option<u32>, procs: Option<u32>) -> Result<()> {
    let w = |file: &str, val: String| {
        let p = dir(id).join(file);
        fs::write(&p, val).map_err(|e| Error::io(format!("write {}", p.display()), e))
    };
    if let Some(m) = mem_bytes {
        w("memory.max", if m == 0 { "max".into() } else { m.to_string() })?;
        w("memory.swap.max", if m == 0 { "max".into() } else { "0".into() })?;
    }
    if let Some(p) = cpu_percent {
        w("cpu.max", if p == 0 { "max 100000".into() } else { format!("{} 100000", p * 1000) })?;
    }
    if let Some(n) = procs {
        w("pids.max", if n == 0 { "max".into() } else { n.to_string() })?;
    }
    Ok(())
}

pub fn kill(id: &str) -> Result<()> {
    let p = dir(id).join("cgroup.kill");
    fs::write(&p, "1").map_err(|e| Error::io(format!("write {}", p.display()), e))
}

pub struct Cgroup(PathBuf);

impl Cgroup {
    pub fn create(id: &str, cfg: &Config) -> Result<Self> {
        if crate::registry::rootless() {
            return Ok(Self::create_rootless(id, cfg));
        }
        // make sure the controllers are delegated to children (best effort, may already be on)
        let _ = fs::write(format!("{ROOT}/cgroup.subtree_control"), "+memory +cpu +pids");
        let d = dir(id);
        fs::create_dir(&d).map_err(|e| Error::io(format!("create {}", d.display()), e))?;
        let cg = Cgroup(d); // Drop cleans up from here on
        if let Some(m) = cfg.mem_bytes {
            cg.write("memory.max", &m.to_string())?;
            cg.write("memory.swap.max", "0")?; // never spill to the SSD
        }
        if let Some(p) = cfg.cpu_percent {
            cg.write("cpu.max", &format!("{} 100000", p * 1000))?;
        }
        if let Some(n) = cfg.procs {
            cg.write("pids.max", &n.to_string())?;
        }
        Ok(cg)
    }

    /// Rootless: a cgroup in a delegated subtree, or an inert placeholder (empty path) with a warning
    /// when limits were asked for but cannot be enforced.
    fn create_rootless(id: &str, cfg: &Config) -> Self {
        use super::rootless::{self, cgroup as rl};
        let wants = (cfg.mem_bytes.is_some(), cfg.cpu_percent.is_some(), cfg.procs.is_some());
        let none = |why: &str| {
            if wants.0 || wants.1 || wants.2 {
                rootless::warn(&format!("resource limits (--mem/--cpus/--pids-limit) are NOT enforced: {why}"));
            }
            Cgroup(PathBuf::new())
        };
        let Some(del) = rl::find() else { return none("no delegated cgroup v2 subtree for this user") };
        let off = rl::missing(&del.controllers, wants.0, wants.1, wants.2);
        if !off.is_empty() {
            return none(&format!("controller(s) {off:?} not delegated"));
        }
        let d = del.base.join(format!("rust-docker-{id}"));
        if fs::create_dir(&d).is_err() {
            return none("cannot create a cgroup in the delegated subtree");
        }
        let cg = Cgroup(d.clone());
        let limits = [
            cfg.mem_bytes.map(|m| ("memory.max", m.to_string())),
            cfg.mem_bytes.map(|_| ("memory.swap.max", "0".to_string())),
            cfg.cpu_percent.map(|p| ("cpu.max", format!("{} 100000", p * 1000))),
            cfg.procs.map(|n| ("pids.max", n.to_string())),
        ];
        for (f, v) in limits.into_iter().flatten() {
            if cg.write(f, &v).is_err() {
                rootless::warn(&format!("could not set {f}"));
            }
        }
        rl::remember(id, &d);
        cg
    }

    pub fn add_pid(&self, pid: i32) -> Result<()> {
        if self.0.as_os_str().is_empty() {
            return Ok(()); // rootless without a cgroup
        }
        self.write("cgroup.procs", &pid.to_string())
    }

    fn write(&self, file: &str, val: &str) -> Result<()> {
        let p = self.0.join(file);
        fs::write(&p, val).map_err(|e| Error::io(format!("write {}", p.display()), e))
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        if self.0.as_os_str().is_empty() {
            return;
        }
        // the kernel tears a cgroup down asynchronously after its last process exits: retry briefly
        for _ in 0..50 {
            if fs::remove_dir(&self.0).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
