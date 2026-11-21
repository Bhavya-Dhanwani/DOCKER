//! `docker exec`: join a running container's namespaces and cgroup, then run a command in it.
use super::{apply_env, cgroup, sys};
use crate::{error::{Error, Result}, registry::Entry};
use std::{fs, os::fd::AsRawFd, process};

pub fn exec(e: &Entry, cmd: &[String], tty: bool) -> Result<i32> {
    if !e.running() {
        return Err(Error::Usage(format!("container {} is not running", e.label())));
    }
    let pid = e.pid;
    // open everything first: after joining the mount namespace /proc paths mean something else
    let open = |p: String| fs::File::open(&p).map_err(|x| Error::io(format!("open {p}"), x));
    let rootless = crate::registry::rootless();
    // rootless: join the container's user namespace first; it grants the rights to join the others
    let user_ns = rootless.then_some(("user", libc::CLONE_NEWUSER));
    let namespaces = user_ns.into_iter().chain([
        ("pid", libc::CLONE_NEWPID),
        ("uts", libc::CLONE_NEWUTS),
        ("ipc", libc::CLONE_NEWIPC),
        ("net", libc::CLONE_NEWNET),
        ("mnt", libc::CLONE_NEWNS), // last: joining it changes how the others resolve
    ])
    .map(|(n, flag)| open(format!("/proc/{pid}/ns/{n}")).map(|f| (f, flag)))
    .collect::<Vec<_>>();
    let root = open(format!("/proc/{pid}/root"))?;

    // count the exec'd process against the container's limits (rootless: only when a cgroup was delegated)
    let procs = cgroup::dir(&e.id).join("cgroup.procs");
    if let Err(x) = fs::write(&procs, process::id().to_string())
        && !rootless
    {
        return Err(Error::io(format!("write {}", procs.display()), x));
    }

    for ns in namespaces {
        let (f, flag) = ns?;
        sys::setns(f.as_raw_fd(), flag)?;
    }
    sys::fchdir(root.as_raw_fd())?;
    sys::chroot_here()?;
    std::env::set_current_dir(if e.workdir.is_empty() { "/" } else { &e.workdir })
        .or_else(|_| std::env::set_current_dir("/"))
        .map_err(|x| Error::io("chdir", x))?;

    let link = if tty { Some(super::pty::link()?) } else { None }; // (our end, the child's end)
    // the pid namespace applies to children only, so fork once more
    match sys::fork()? {
        sys::Fork::Child => {
            apply_env(&e.env);
            if let Some((_, child_end)) = link
                && let Err(err) = super::pty::setup_in_container(child_end)
            {
                eprintln!("docker: {err}");
                process::exit(126);
            }
            if tty {
                // SAFETY: freshly forked, single-threaded child
                unsafe { std::env::set_var("TERM", std::env::var("TERM").unwrap_or_else(|_| "xterm".into())) };
            }
            if let Err(err) = super::harden::finalize(&e.user, &e.security.clone().unwrap_or_default()) {
                eprintln!("docker: {err}");
                process::exit(126);
            }
            let err = sys::execvp(cmd);
            eprintln!("docker: {err}");
            process::exit(127);
        }
        sys::Fork::Parent(child) => match link.map(|(ours, _)| ours).and_then(super::pty::receive_master) {
            Some(master) => super::pty::relay(master, child, true, ""),
            None => sys::wait(child),
        },
    }
}
