//! Thin, checked wrappers over the raw syscalls. All `unsafe` lives here.
use crate::error::{Error, Result};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

fn cstr(s: impl AsRef<[u8]>) -> CString {
    CString::new(s.as_ref()).expect("interior NUL in path/arg")
}

fn check(ret: i64, ctx: impl FnOnce() -> String) -> Result<()> {
    if ret < 0 { Err(Error::sys(ctx())) } else { Ok(()) }
}

pub fn mount(src: &str, dst: &str, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> Result<()> {
    let (s, d, f, o) = (cstr(src), cstr(dst), fstype.map(cstr), data.map(cstr));
    let ptr = |x: &Option<CString>| x.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let r = unsafe { libc::mount(s.as_ptr(), d.as_ptr(), ptr(&f), flags, ptr(&o).cast()) };
    check(r as i64, || format!("mount {dst}"))
}

pub fn pivot_root(new_root: &str, put_old: &str) -> Result<()> {
    let (n, o) = (cstr(new_root), cstr(put_old));
    let r = unsafe { libc::syscall(libc::SYS_pivot_root, n.as_ptr(), o.as_ptr()) };
    check(r, || "pivot_root".into())
}

pub fn umount_detach(path: &str) -> Result<()> {
    let p = cstr(path);
    let r = unsafe { libc::umount2(p.as_ptr(), libc::MNT_DETACH) };
    check(r as i64, || format!("umount {path}"))
}

pub fn unshare(flags: libc::c_int) -> Result<()> {
    check(unsafe { libc::unshare(flags) } as i64, || "unshare (are you root?)".into())
}

pub fn set_hostname(name: &str) -> Result<()> {
    let r = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    check(r as i64, || "sethostname".into())
}

pub fn mknod_char(path: &str, major: u32, minor: u32) -> Result<()> {
    let p = cstr(path);
    let r = unsafe { libc::mknod(p.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(major, minor)) };
    check(r as i64, || format!("mknod {path}"))?;
    // mknod honours the umask (022 would leave /dev/null unwritable for non-root users, e.g. postgres)
    let r = unsafe { libc::chmod(p.as_ptr(), 0o666) };
    check(r as i64, || format!("chmod {path}"))
}

/// fork() whose child (and only the child) is PID 1 of a new pid namespace.
/// Unlike unshare(CLONE_NEWPID) + fork(), the caller stays in its own pid namespace and can still start threads.
pub fn fork_newpid() -> Result<Fork> {
    // raw clone with a null stack behaves like fork(); all remaining arguments are unused (0)
    let r = unsafe { libc::syscall(libc::SYS_clone, (libc::CLONE_NEWPID | libc::SIGCHLD) as libc::c_ulong, 0, 0, 0, 0) };
    match r {
        0 => Ok(Fork::Child),
        p if p > 0 => Ok(Fork::Parent(p as i32)),
        _ => Err(Error::sys("clone (new pid namespace)")),
    }
}

pub enum Fork {
    Child,
    Parent(i32),
}

pub fn fork() -> Result<Fork> {
    match unsafe { libc::fork() } {
        0 => Ok(Fork::Child),
        p if p > 0 => Ok(Fork::Parent(p)),
        _ => Err(Error::sys("fork")),
    }
}

/// Block until the child exits; returns a shell-style exit code.
pub fn wait(pid: i32) -> Result<i32> {
    let mut st = 0;
    check(unsafe { libc::waitpid(pid, &mut st, 0) } as i64, || "waitpid".into())?;
    Ok(if libc::WIFSIGNALED(st) { 128 + libc::WTERMSIG(st) } else { libc::WEXITSTATUS(st) })
}

pub fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } as i64, || "pipe".into())?;
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Replace this process. Only returns on failure.
pub fn execvp(cmd: &[String]) -> Error {
    let args: Vec<CString> = cmd.iter().map(cstr).collect();
    let mut ptrs: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execvp(ptrs[0], ptrs.as_ptr()) };
    Error::sys(format!("exec {}", cmd[0]))
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

pub fn setsid() -> Result<()> {
    check(unsafe { libc::setsid() } as i64, || "setsid".into())
}

/// stdin <- /dev/null, stdout and stderr -> the log file (append).
pub fn redirect_stdio(log: &Path) -> Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(format!("mkdir {}", dir.display()), e))?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| Error::io(format!("open {}", log.display()), e))?;
    let null = std::fs::File::open("/dev/null").map_err(|e| Error::io("open /dev/null", e))?;
    for (from, to) in [(null.as_raw_fd(), 0), (out.as_raw_fd(), 1), (out.as_raw_fd(), 2)] {
        check(unsafe { libc::dup2(from, to) } as i64, || format!("dup2 -> {to}"))?;
    }
    Ok(())
}

pub fn setns(fd: RawFd, nstype: libc::c_int) -> Result<()> {
    check(unsafe { libc::setns(fd, nstype) } as i64, || "setns".into())
}

pub fn fchdir(fd: RawFd) -> Result<()> {
    check(unsafe { libc::fchdir(fd) } as i64, || "fchdir".into())
}

/// chroot into the current directory.
pub fn chroot_here() -> Result<()> {
    check(unsafe { libc::chroot(c".".as_ptr()) } as i64, || "chroot".into())
}

pub fn signal(pid: i32, sig: i32) -> Result<()> {
    check(unsafe { libc::kill(pid, sig) } as i64, || format!("signal {pid}"))
}

/// Run `f` in a forked helper process that dies with its parent. Returns the helper's pid.
pub fn spawn_helper(f: impl FnOnce()) -> Result<i32> {
    match fork()? {
        Fork::Child => {
            unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
            f();
            std::process::exit(0);
        }
        Fork::Parent(pid) => Ok(pid),
    }
}

pub fn kill(pid: i32) {
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// Become `uid:gid` (clears supplementary groups first). Must run before exec, as root.
pub fn become_user(uid: u32, gid: u32) -> Result<()> {
    check(unsafe { libc::setgroups(0, std::ptr::null()) } as i64, || "setgroups".into())?;
    check(unsafe { libc::setgid(gid) } as i64, || "setgid".into())?;
    check(unsafe { libc::setuid(uid) } as i64, || "setuid".into())
}

pub fn dup2(from: RawFd, to: RawFd) -> Result<()> {
    check(unsafe { libc::dup2(from, to) } as i64, || format!("dup2 -> {to}"))
}
