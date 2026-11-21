//! cgroup v2 for an unprivileged user: only possible inside a subtree delegated to us (systemd gives each
//! user `user@UID.service` with `Delegate=yes`). Without one, limits are skipped with a warning.
use std::{fs, path::{Path, PathBuf}};

/// The cgroup v2 path from the text of /proc/self/cgroup (`0::/user.slice/...`).
pub fn parse_own(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup.lines().find_map(|l| l.strip_prefix("0::")).map(str::trim)
}

fn writable(p: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str())) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// Where container cgroups can be created, with the controllers usable there (memory/cpu/pids subset).
pub struct Delegation {
    pub base: PathBuf,
    pub controllers: Vec<String>,
}

/// Find a delegated cgroup: our own cgroup's parent first (systemd layout: we live in a leaf, the delegated
/// parent holds the controllers), then our own cgroup. Needs `cgroup.subtree_control` to be writable.
pub fn find() -> Option<Delegation> {
    let own = fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = parse_own(&own)?.trim_start_matches('/').to_string();
    let root = Path::new("/sys/fs/cgroup");
    let mine = root.join(&rel);
    let mut cands = vec![];
    if let Some(p) = mine.parent().filter(|p| p.starts_with(root) && *p != root) {
        cands.push(p.to_path_buf());
    }
    if mine != root {
        cands.push(mine);
    }
    cands.into_iter().find(|d| writable(&d.join("cgroup.subtree_control")) && writable(&d.join("cgroup.procs"))).map(|base| {
        // best effort: ask for the controllers we need (fails with EBUSY if processes live here, fine)
        let avail = fs::read_to_string(base.join("cgroup.controllers")).unwrap_or_default();
        let want: Vec<&str> = ["memory", "cpu", "pids"].into_iter().filter(|c| avail.split_whitespace().any(|a| a == *c)).collect();
        let _ = fs::write(base.join("cgroup.subtree_control"), want.iter().map(|c| format!("+{c}")).collect::<Vec<_>>().join(" "));
        let on = fs::read_to_string(base.join("cgroup.subtree_control")).unwrap_or_default();
        let controllers = on.split_whitespace().map(String::from).collect();
        Delegation { base, controllers }
    })
}

/// Which of the requested limits cannot be enforced (names of controllers).
pub fn missing(controllers: &[String], mem: bool, cpu: bool, pids: bool) -> Vec<&'static str> {
    let has = |c: &str| controllers.iter().any(|x| x == c);
    [(mem, "memory"), (cpu, "cpu"), (pids, "pids")].into_iter().filter(|(asked, c)| *asked && !has(c)).map(|(_, c)| c).collect()
}

/// Remember where a container's cgroup is, so `stats`/`stop`/`update` from other `docker` invocations find it.
pub fn marker(id: &str) -> PathBuf {
    crate::registry::data_dir().join("cgroups").join(id)
}

pub fn remember(id: &str, dir: &Path) {
    let m = marker(id);
    if let Some(p) = m.parent() {
        let _ = fs::create_dir_all(p);
    }
    let _ = fs::write(m, dir.to_string_lossy().as_bytes());
}

pub fn recall(id: &str) -> Option<PathBuf> {
    fs::read_to_string(marker(id)).ok().map(|s| PathBuf::from(s.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_cgroup_path() {
        assert_eq!(parse_own("0::/user.slice/user-1001.slice/session-3.scope\n"), Some("/user.slice/user-1001.slice/session-3.scope"));
        assert_eq!(parse_own("12:cpu:/x\n"), None);
    }

    #[test]
    fn missing_controllers() {
        let have = vec!["pids".to_string(), "memory".to_string()];
        assert_eq!(missing(&have, true, true, true), ["cpu"]);
        assert!(missing(&have, true, false, true).is_empty());
        assert_eq!(missing(&[], false, false, false), Vec::<&str>::new());
    }
}
