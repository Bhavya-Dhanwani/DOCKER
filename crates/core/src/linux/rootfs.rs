//! Runs inside the new mount namespace: build the root, pivot into it, mount the minimum.
use super::sys;
use crate::{cli::Volume, error::{Error, Result}};
use libc::{MS_BIND, MS_PRIVATE, MS_RDONLY, MS_REC, MS_REMOUNT};
use std::{env, fs, path::{Path, PathBuf}};

pub enum Source {
    /// A ready-made root directory (read-only unless `--rw`).
    Dir(PathBuf),
    /// Image layers, bottom first. Shared read-only; writes go to a RAM-backed upper layer.
    Image(Vec<PathBuf>),
    /// A build step: like `Image`, but the upper layer is a real directory that becomes the new layer.
    Step { layers: Vec<PathBuf>, upper: PathBuf, work: PathBuf },
}

/// overlayfs wants the top layer first.
pub fn overlay_opts(layers: &[PathBuf], upper: &Path, work: &Path) -> String {
    // An image can use the same layer twice (identical empty or repeated steps), and the kernel refuses a lowerdir
    // that appears twice ("conflicting lowerdir path"). Keep the topmost copy: it covers everything the lower one
    // provided, so what the container sees is unchanged.
    let mut seen = std::collections::HashSet::new();
    let lower: Vec<String> = layers.iter().rev().filter(|p| seen.insert(*p)).map(|p| sys::path_str(p)).collect();
    format!("lowerdir={},upperdir={},workdir={}", lower.join(":"), sys::path_str(upper), sys::path_str(work))
}

fn mount_overlay(layers: &[PathBuf], id: &str) -> Result<PathBuf> {
    let base = super::rootless::run_root().join(id);
    fs::create_dir_all(&base).map_err(|e| Error::io(format!("mkdir {}", base.display()), e))?;
    // tmpfs: container writes live in RAM (counted against --mem) and vanish on exit: zero SSD writes
    sys::mount("tmpfs", &sys::path_str(&base), Some("tmpfs"), 0, Some("mode=755"))?;
    if crate::registry::rootless() {
        return super::rootless::mount_root(layers, &base);
    }
    let (upper, work, merged) = (base.join("upper"), base.join("work"), base.join("merged"));
    for d in [&upper, &work, &merged] {
        fs::create_dir(d).map_err(|e| Error::io(format!("mkdir {}", d.display()), e))?;
    }
    let opts = overlay_opts(layers, &upper, &work);
    sys::mount("overlay", &sys::path_str(&merged), Some("overlay"), 0, Some(&opts))?;
    Ok(merged)
}

fn mount_step_overlay(layers: &[PathBuf], upper: &Path, work: &Path, id: &str) -> Result<PathBuf> {
    if crate::registry::rootless() {
        return Err(Error::Usage("docker build is not supported in rootless mode yet".into()));
    }
    let merged = super::rootless::run_root().join(id).join("merged");
    fs::create_dir_all(&merged).map_err(|e| Error::io(format!("mkdir {}", merged.display()), e))?;
    // plain overlay semantics: no redirects or metacopy, so the upper dir is a portable layer
    let opts = format!("{},redirect_dir=off,metacopy=off", overlay_opts(layers, upper, work));
    sys::mount("overlay", &sys::path_str(&merged), Some("overlay"), 0, Some(&opts))?;
    Ok(merged)
}

/// Bind-mount a host path into the (not yet pivoted) root.
fn mount_volume(root: &Path, v: &Volume) -> Result<()> {
    let host = fs::canonicalize(&v.host).map_err(|e| Error::io(format!("volume {}", v.host), e))?;
    let target = root.join(v.container.trim_start_matches('/'));
    let io = |e| Error::io(format!("prepare mount point {}", v.container), e);
    if host.is_dir() {
        fs::create_dir_all(&target).map_err(io)?;
    } else {
        // a symlink here (e.g. /etc/resolv.conf -> ../run/...) would point outside the image: replace it
        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            let _ = fs::remove_file(&target);
        }
        fs::create_dir_all(target.parent().unwrap_or(root)).map_err(io)?;
        fs::OpenOptions::new().create(true).append(true).open(&target).map_err(io)?;
    }
    // a symlink inside the image must not redirect the mount onto the host
    // ponytail: dirs were created before this check; resolve parents first if images are untrusted
    if !fs::canonicalize(&target).map_err(io)?.starts_with(root) {
        return Err(Error::Usage(format!("volume target {} escapes the container root", v.container)));
    }
    let t = sys::path_str(&target);
    sys::mount(&sys::path_str(&host), &t, None, MS_BIND | MS_REC, None)?;
    if v.ro {
        sys::mount("none", &t, None, MS_REMOUNT | MS_BIND | MS_RDONLY, None)?;
    }
    Ok(())
}

pub fn enter(src: &Source, id: &str, rw: bool, volumes: &[Volume]) -> Result<()> {
    sys::mount("none", "/", None, MS_PRIVATE | MS_REC, None)?; // don't leak mounts to the host
    let (root, read_only) = match src {
        Source::Dir(p) => (p.clone(), !rw),
        Source::Image(layers) => (mount_overlay(layers, id)?, false),
        Source::Step { layers, upper, work } => (mount_step_overlay(layers, upper, work, id)?, false),
    };
    let root_s = sys::path_str(&root);
    sys::mount(&root_s, &root_s, None, MS_BIND | MS_REC, None)?; // pivot_root needs a mount point
    let rootless = crate::registry::rootless();
    if rootless {
        super::rootless::bind_devices(&root)?; // no mknod without real root: bind the host's nodes
    }
    for v in volumes {
        mount_volume(&root, v)?;
    }
    env::set_current_dir(&root).map_err(|e| Error::io("chdir rootfs", e))?;
    fs::create_dir_all("old").map_err(|e| Error::io("mkdir old", e))?;
    sys::pivot_root(".", "old")?;
    env::set_current_dir("/").map_err(|e| Error::io("chdir /", e))?;
    sys::umount_detach("/old")?;
    let _ = fs::remove_dir("/old");

    for d in ["proc", "tmp", "dev"] {
        fs::create_dir_all(Path::new("/").join(d)).map_err(|e| Error::io(format!("mkdir /{d}"), e))?;
    }
    sys::mount("proc", "/proc", Some("proc"), 0, None)?;
    // /sys, read-only like Docker: runtimes read CPU and cgroup facts from it (MongoDB's tcmalloc aborts without it).
    // Best effort: a rootless container may not be allowed to mount it.
    let _ = fs::create_dir_all("/sys");
    let _ = sys::mount("sysfs", "/sys", Some("sysfs"), libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None);
    if read_only {
        // a read-only root directory needs somewhere to write; an image's /tmp is plain container storage, as in Docker
        sys::mount("tmpfs", "/tmp", Some("tmpfs"), 0, Some("size=64m"))?;
    }
    if !rootless {
        sys::mount("tmpfs", "/dev", Some("tmpfs"), 0, Some("size=1m,mode=755"))?;
        for (name, major, minor) in [("null", 1, 3), ("zero", 1, 5), ("random", 1, 8), ("urandom", 1, 9), ("tty", 5, 0)] {
            sys::mknod_char(&format!("/dev/{name}"), major, minor)?;
        }
    }
    // a private devpts: the container creates its terminals here (and `tty`/ttyname work)
    fs::create_dir_all("/dev/pts").map_err(|e| Error::io("mkdir /dev/pts", e))?;
    // rootless: gid 5 usually is not mapped in the user namespace
    let pts = if rootless { "newinstance,ptmxmode=0666,mode=0600" } else { "newinstance,ptmxmode=0666,mode=0620,gid=5" };
    sys::mount("devpts", "/dev/pts", Some("devpts"), 0, Some(pts))?;
    let _ = std::os::unix::fs::symlink("pts/ptmx", "/dev/ptmx");
    // what shells and databases expect: /dev/fd (process substitution), std streams, POSIX shared memory
    for (target, link) in [("/proc/self/fd", "fd"), ("/proc/self/fd/0", "stdin"), ("/proc/self/fd/1", "stdout"), ("/proc/self/fd/2", "stderr")] {
        let _ = std::os::unix::fs::symlink(target, Path::new("/dev").join(link));
    }
    if fs::create_dir_all("/dev/shm").is_ok() {
        let _ = sys::mount("tmpfs", "/dev/shm", Some("tmpfs"), 0, Some("size=64m,mode=1777"));
    }
    if read_only {
        sys::mount("/", "/", None, MS_REMOUNT | MS_BIND | MS_RDONLY, None)?;
    }
    sys::set_hostname("container")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_lists_top_layer_first() {
        let layers = [PathBuf::from("/l/base"), PathBuf::from("/l/top")];
        assert_eq!(
            overlay_opts(&layers, Path::new("/u"), Path::new("/w")),
            "lowerdir=/l/top:/l/base,upperdir=/u,workdir=/w"
        );
    }

    #[test]
    fn a_layer_used_twice_appears_once_at_its_topmost_place() {
        // base, a, b, a  (an image that reuses layer `a`)
        let l = |n: &str| PathBuf::from(format!("/l/{n}"));
        let layers = [l("base"), l("a"), l("b"), l("a")];
        assert_eq!(
            overlay_opts(&layers, Path::new("/u"), Path::new("/w")),
            "lowerdir=/l/a:/l/b:/l/base,upperdir=/u,workdir=/w"
        );
    }
}
