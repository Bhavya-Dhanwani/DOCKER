//! cgroup v2: the only hard RAM/CPU guarantee for a container.
use crate::{cli::Config, error::{Error, Result}};
use std::{fs, path::PathBuf};

const ROOT: &str = "/sys/fs/cgroup";

pub fn dir(id: &str) -> PathBuf {
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

    pub fn add_pid(&self, pid: i32) -> Result<()> {
        self.write("cgroup.procs", &pid.to_string())
    }

    fn write(&self, file: &str, val: &str) -> Result<()> {
        let p = self.0.join(file);
        fs::write(&p, val).map_err(|e| Error::io(format!("write {}", p.display()), e))
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // the kernel tears a cgroup down asynchronously after its last process exits: retry briefly
        for _ in 0..50 {
            if fs::remove_dir(&self.0).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
