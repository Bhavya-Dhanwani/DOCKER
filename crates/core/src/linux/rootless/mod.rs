//! Rootless mode: run containers as an unprivileged user inside a user namespace.
//!
//! * ids: `newuidmap`/`newgidmap` + /etc/subuid when installed (full mapping), else a single-id mapping
//! * layers: unpacked inside a user namespace with `user.overlay.*` xattrs and mounted with overlayfs
//!   `userxattr`; if the kernel refuses that, the layers are copied into a tmpfs ([`copyfs`])
//! * network: `slirp4netns` or `pasta` (see [`net`]); otherwise the host network namespace is shared
//! * cgroups: only inside a delegated subtree (see [`cgroup`]); otherwise limits are skipped with a warning
//! * /dev: host device nodes are bind-mounted (no `mknod` without real root)
pub mod cgroup;
mod copyfs;
pub mod idmap;
pub mod net;

use super::{rootfs, sys};
use crate::error::{Error, Result};
use docker_image::{LayerRef, Store};
use idmap::{Mapping, SubRange};
use libc::{MS_BIND, MS_REC};
use std::{fs, path::{Path, PathBuf}, process};

/// Per-container scratch space: `/run/rust-docker` for root, a directory in the data dir otherwise.
pub fn run_root() -> PathBuf {
    if crate::registry::rootless() {
        crate::registry::data_dir().join("run")
    } else {
        PathBuf::from("/run/rust-docker")
    }
}

/// A degraded-feature notice; always on stderr (a detached container's goes to its log).
pub fn warn(msg: &str) {
    eprintln!("docker: rootless: {msg}");
}

/// Choose the id mapping for this host. `SASTA_ROOTLESS_MAP=single` forces the fallback.
pub fn detect() -> Mapping {
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let single = Mapping::Single { uid, gid };
    if std::env::var("SASTA_ROOTLESS_MAP").as_deref() == Ok("single") || net::find_in_path("newuidmap").is_none() || net::find_in_path("newgidmap").is_none() {
        return single;
    }
    let name = fs::read_to_string("/etc/passwd").ok().and_then(|p| idmap::user_name(&p, uid)).unwrap_or_default();
    let range = |file: &str, id: u32| fs::read_to_string(file).ok().and_then(|t| idmap::parse_subid(&t, &name, id));
    match (range("/etc/subuid", uid), range("/etc/subgid", uid)) {
        (Some(sub_uid), Some(sub_gid)) => Mapping::Full { uid, gid, sub_uid, sub_gid },
        _ => single,
    }
}

/// Tell the user what the chosen mapping means (only the limited one deserves a notice).
pub fn describe(m: &Mapping) {
    if !m.is_full() {
        warn("single-uid mapping (container root = you; no other uid/gid exists: chown to other ids fails, USER is ignored). \
              Install `uidmap` and add a /etc/subuid + /etc/subgid range for full mapping");
    }
}

/// Inside the container's user namespace: is only one id mapped?
pub fn single_map() -> bool {
    fs::read_to_string("/proc/self/uid_map").is_ok_and(|m| idmap::is_single_map(&m))
}

/// Write the uid/gid maps of the user namespace that `pid` was just cloned into (called by its parent).
pub fn write_maps(pid: i32, m: &Mapping) -> Result<()> {
    let io = |what: &str, e| Error::io(format!("{what} (pid {pid})"), e);
    match m {
        Mapping::Single { uid, gid } => {
            // gid_map may only be written by an unprivileged user after setgroups is denied
            fs::write(format!("/proc/{pid}/setgroups"), "deny").map_err(|e| io("write setgroups", e))?;
            fs::write(format!("/proc/{pid}/uid_map"), idmap::single_line(*uid)).map_err(|e| io("write uid_map", e))?;
            fs::write(format!("/proc/{pid}/gid_map"), idmap::single_line(*gid)).map_err(|e| io("write gid_map", e))
        }
        Mapping::Full { uid, gid, sub_uid, sub_gid } => {
            for (prog, id, sub) in [("newuidmap", *uid, *sub_uid), ("newgidmap", *gid, *sub_gid)] {
                run_idmap(prog, pid, id, sub)?;
            }
            Ok(())
        }
    }
}

fn run_idmap(prog: &str, pid: i32, id: u32, sub: SubRange) -> Result<()> {
    let out = process::Command::new(prog).args(idmap::newidmap_args(pid as u32, id, sub)).output().map_err(|e| Error::io(format!("run {prog}"), e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Usage(format!("{prog} failed: {}", String::from_utf8_lossy(&out.stderr).trim())))
    }
}

/// Run `f` in a child inside a fresh user namespace with mapping `m` (where we are "root": whiteout
/// `mknod 0/0`, chown and `user.*` xattrs work). Returns whether `f` reported success.
pub fn in_userns(m: &Mapping, f: impl FnOnce() -> bool) -> Result<bool> {
    let (rd, wr) = sys::pipe()?;
    match sys::fork_ns(libc::CLONE_NEWUSER)? {
        sys::Fork::Child => {
            drop(wr);
            let _ = std::io::Read::read(&mut fs::File::from(rd), &mut [0u8; 1]); // EOF: maps are written
            process::exit(i32::from(!f()));
        }
        sys::Fork::Parent(pid) => {
            drop(rd);
            if let Err(e) = write_maps(pid, m) {
                sys::kill(pid);
                let _ = sys::wait(pid);
                return Err(e);
            }
            drop(wr);
            Ok(sys::wait(pid)? == 0)
        }
    }
}

/// Unpacked layers for `layers` in this user's own flavour (`layers-rootless/`, never mixed with the
/// root engine's `layers/`), unpacking from the downloaded blobs inside a user namespace when needed.
pub fn ensure_layers(store: &Store, layers: &[LayerRef]) -> Result<Vec<PathBuf>> {
    let mut mapping = None;
    let mut out = vec![];
    for l in layers {
        let hex = store.layer_path(&l.digest).map_err(|e| Error::Usage(e.to_string()))?;
        let dir = store.root().join("layers-rootless").join(hex.file_name().unwrap_or_default());
        if !dir.is_dir() {
            let blob = store.blob_path(&l.digest).map_err(|e| Error::Usage(e.to_string()))?;
            if !blob.is_file() {
                return Err(Error::Usage(format!(
                    "layer {} has no rootless copy and no downloaded blob: run `docker pull` as this user (the root engine's store is not shared)",
                    l.digest
                )));
            }
            let m = mapping.get_or_insert_with(|| {
                let m = detect();
                describe(&m);
                m
            });
            let staging = store.root().join("tmp").join(format!("unpack-rl-{}", dir.file_name().unwrap_or_default().to_string_lossy()));
            let _ = fs::remove_dir_all(&staging);
            fs::create_dir_all(&staging).map_err(|e| Error::io("mkdir staging", e))?;
            let (b, mt, s) = (blob.clone(), l.media_type.clone(), staging.clone());
            let ok = in_userns(m, move || {
                docker_image::unpack_rootless(&b, &mt, &s).map_err(|e| eprintln!("docker: unpack layer: {e}")).is_ok()
            })?;
            if !ok {
                return Err(Error::Usage(format!("unpacking layer {} failed (see above)", l.digest)));
            }
            fs::create_dir_all(dir.parent().expect("has parent")).map_err(|e| Error::io("mkdir layers-rootless", e))?;
            fs::rename(&staging, &dir).map_err(|e| Error::io("publish layer", e))?;
            let _ = fs::remove_file(blob);
        }
        out.push(dir);
    }
    Ok(out)
}

/// Mount the image's root at `base/merged` (base is already a tmpfs): userxattr overlayfs, or copy-up.
pub fn mount_root(layers: &[PathBuf], base: &Path) -> Result<PathBuf> {
    let (upper, work, merged) = (base.join("upper"), base.join("work"), base.join("merged"));
    for d in [&upper, &work, &merged] {
        fs::create_dir(d).map_err(|e| Error::io(format!("mkdir {}", d.display()), e))?;
    }
    let opts = format!("{},userxattr", rootfs::overlay_opts(layers, &upper, &work));
    let forced = std::env::var("SASTA_ROOTLESS_COPY").as_deref() == Ok("1");
    let why = if forced {
        "forced by SASTA_ROOTLESS_COPY=1".to_string()
    } else {
        match sys::mount("overlay", &sys::path_str(&merged), Some("overlay"), 0, Some(&opts)) {
            Ok(()) => return Ok(merged),
            Err(e) => e.to_string(),
        }
    };
    warn(&format!("overlayfs not usable ({why}); copying the image into RAM instead"));
    copyfs::merge(layers, &merged).map_err(|e| Error::io("copy image layers", e))?;
    Ok(merged)
}

/// Before pivot_root: a tmpfs /dev holding bind mounts of the host's device nodes.
pub fn bind_devices(root: &Path) -> Result<()> {
    let dev = root.join("dev");
    fs::create_dir_all(&dev).map_err(|e| Error::io("mkdir /dev", e))?;
    sys::mount("tmpfs", &sys::path_str(&dev), Some("tmpfs"), 0, Some("size=1m,mode=755"))?;
    for name in ["null", "zero", "random", "urandom", "tty"] {
        let target = dev.join(name);
        fs::File::create(&target).map_err(|e| Error::io(format!("create /dev/{name}"), e))?;
        sys::mount(&format!("/dev/{name}"), &sys::path_str(&target), None, MS_BIND | MS_REC, None)?;
    }
    Ok(())
}

/// Liveness and rough usage of a container without a cgroup: only its init process (`pid`) is measured.
pub fn proc_stats(pid: u32) -> Option<crate::stats::Stats> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let f: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    if f.first() == Some(&"Z") {
        return None; // dead, not yet reaped
    }
    let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?; // utime + stime
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss_kb = status.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).unwrap_or(0);
    Some(crate::stats::Stats { mem_bytes: rss_kb * 1024, cpu_ms: ticks * 10, procs: 1 })
}
