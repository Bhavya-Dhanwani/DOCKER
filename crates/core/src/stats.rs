/// One process of a container (`docker top`), with host pids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub cmd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpDir {
    FromContainer,
    ToContainer,
}

/// Live usage of one container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Linux: current usage. Windows: peak usage (Job Objects have no "current").
    pub mem_bytes: u64,
    pub cpu_ms: u64,
    pub procs: u32,
    /// Writable-layer usage. Container writes live in RAM-backed tmpfs, so on Linux this is the cgroup's
    /// `shmem` (tmpfs pages); 0 where it cannot be measured.
    pub disk_bytes: u64,
}
