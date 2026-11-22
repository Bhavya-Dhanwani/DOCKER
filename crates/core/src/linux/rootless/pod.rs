//! Rootless user-defined networks.
//!
//! Without root there is no bridge, and a user-space network helper (slirp4netns) only serves one network
//! namespace. So all containers of a user-defined network live in ONE user namespace and ONE network namespace,
//! like the containers of a pod:
//!
//! * they reach each other on `127.0.0.1` and by name (every member's names point at `127.0.0.1` in /etc/hosts),
//! * they share one set of ports: two containers cannot both listen on port 80,
//! * one slirp4netns serves the whole network (outbound access, DNS and `-p` publishing).
//!
//! A small background process per network (the *holder*) owns the namespaces and the helper. It starts when the
//! first container of the network starts and goes away a few seconds after the last one has stopped.
use super::net::{self, Helper};
use super::{idmap::Mapping, run_root, write_maps};
use super::super::sys;
use crate::{cli::Port, error::{Error, Result}};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long a network without running containers stays up.
const IDLE_SECS: u32 = 8;

fn dir(network: &str) -> PathBuf {
    run_root().join(format!("net-{network}"))
}

pub fn api_socket(network: &str) -> PathBuf {
    dir(network).join("slirp.sock")
}

fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0 && fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s[s.rfind(')').unwrap_or(0)..].starts_with(") Z"))
}

fn keeper_of(network: &str) -> Option<i32> {
    let pid: i32 = fs::read_to_string(dir(network).join("keeper")).ok()?.trim().parse().ok()?;
    (alive(pid) && dir(network).join("ready").exists()).then_some(pid)
}

/// Held (the lock is released when the file is closed) while a network is being looked up or created.
struct Lock(#[allow(dead_code)] fs::File);

impl Lock {
    fn take(path: &Path) -> Result<Lock> {
        let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).map_err(|e| Error::io("open network lock", e))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } < 0 {
            return Err(Error::sys("lock the network"));
        }
        Ok(Lock(f))
    }
}

/// Put this process into the user and network namespaces of `network`, creating them (and the helper that
/// serves them) if no container of the network is running. Returns the helper's API socket for port publishing.
/// This process must be single-threaded (`setns` into a user namespace requires it).
pub fn join(network: &str, helper: Helper, map: &Mapping) -> Result<PathBuf> {
    if helper != Helper::Slirp4netns {
        return Err(Error::Usage("rootless user-defined networks need slirp4netns (pasta cannot publish ports of a shared network)".into()));
    }
    let d = dir(network);
    fs::create_dir_all(&d).map_err(|e| Error::io("mkdir network dir", e))?;
    let _lock = Lock::take(&d.join("lock"))?;
    let keeper = match keeper_of(network) {
        Some(k) => k,
        None => {
            let _ = fs::remove_file(d.join("ready"));
            match sys::fork()? {
                sys::Fork::Child => {
                    drop(_lock); // the lock must not outlive the supervisor that took it: this copy would keep it held
                    let _ = sys::setsid();
                    let _ = sys::redirect_stdio(&d.join("log"));
                    let code = match holder(network, helper, map) {
                        Ok(()) => 0,
                        Err(e) => {
                            eprintln!("docker: network {network}: {e}");
                            1
                        }
                    };
                    std::process::exit(code);
                }
                sys::Fork::Parent(_) => {
                    let end = Instant::now() + Duration::from_secs(20);
                    loop {
                        if let Some(k) = keeper_of(network) {
                            break k;
                        }
                        if Instant::now() > end {
                            let why = fs::read_to_string(d.join("log")).unwrap_or_default();
                            return Err(Error::Usage(format!("rootless network {network} did not come up: {}", why.trim())));
                        }
                        std::thread::sleep(Duration::from_millis(30));
                    }
                }
            }
        }
    };
    let open = |ns: &str| fs::File::open(format!("/proc/{keeper}/ns/{ns}")).map_err(|e| Error::io(format!("open {ns} namespace of network {network}"), e));
    let (user, net) = (open("user")?, open("net")?);
    // user first: it is what grants the right to join the network namespace
    if unsafe { libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) } < 0 {
        return Err(Error::sys(format!("join the user namespace of network {network}")));
    }
    if unsafe { libc::setns(net.as_raw_fd(), libc::CLONE_NEWNET) } < 0 {
        return Err(Error::sys(format!("join the network namespace of network {network}")));
    }
    Ok(api_socket(network))
}

/// The background process of one network: namespaces + helper, until the network has been idle for a while.
fn holder(network: &str, helper: Helper, map: &Mapping) -> Result<()> {
    let d = dir(network);
    let (rd, wr) = sys::pipe()?;
    let keeper = match sys::fork_ns(libc::CLONE_NEWUSER | libc::CLONE_NEWNET)? {
        sys::Fork::Child => {
            drop(wr);
            let _ = std::io::Read::read(&mut fs::File::from(rd), &mut [0u8; 1]); // EOF: the id maps are written
            if super::super::net::loopback_up().is_err() {
                std::process::exit(1);
            }
            loop {
                std::thread::sleep(Duration::from_secs(3600)); // owns the namespaces; killed by the holder
            }
        }
        sys::Fork::Parent(k) => k,
    };
    drop(rd);
    if let Err(e) = write_maps(keeper, map) {
        sys::kill(keeper);
        let _ = sys::wait(keeper);
        return Err(e);
    }
    drop(wr);
    let helper_pid = match net::start(helper, keeper as u32, &[], &d) {
        Ok(p) => p,
        Err(e) => {
            sys::kill(keeper);
            let _ = sys::wait(keeper);
            return Err(e);
        }
    };
    fs::write(d.join("keeper"), keeper.to_string()).map_err(|e| Error::io("write keeper", e))?;
    fs::write(d.join("ready"), b"1").map_err(|e| Error::io("write ready", e))?;
    let mut idle = 0u32;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if !alive(keeper) {
            break;
        }
        idle = if crate::networks::members(Some(network), "").is_empty() { idle + 1 } else { 0 };
        if idle >= IDLE_SECS {
            break;
        }
    }
    let _ = fs::remove_file(d.join("ready"));
    sys::kill(helper_pid);
    sys::kill(keeper);
    let _ = sys::wait(helper_pid);
    let _ = sys::wait(keeper);
    Ok(())
}

/// Ask slirp4netns for something; returns the reply line.
fn api(sock: &Path, request: &str) -> Result<String> {
    let mut s = UnixStream::connect(sock).map_err(|e| Error::io("connect slirp4netns api", e))?;
    s.write_all(request.as_bytes()).and_then(|()| s.write_all(b"\n")).map_err(|e| Error::io("slirp4netns api", e))?;
    let mut reply = String::new();
    BufReader::new(&s).read_line(&mut reply).map_err(|e| Error::io("slirp4netns api", e))?;
    Ok(reply)
}

/// Stop publishing these host ports (a restarted container publishes the same ports again).
pub fn unpublish(sock: &Path, ports: &[Port]) {
    let Ok(reply) = api(sock, r#"{"execute":"list_hostfwd"}"#) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&reply) else { return };
    for e in v.pointer("/return/entries").and_then(|e| e.as_array()).into_iter().flatten() {
        let (id, port) = (e.get("id").and_then(|i| i.as_u64()), e.get("host_port").and_then(|p| p.as_u64()));
        if let (Some(id), Some(port)) = (id, port)
            && ports.iter().any(|p| u64::from(p.host) == port)
        {
            let _ = api(sock, &format!(r#"{{"execute":"remove_hostfwd","arguments":{{"id":{id}}}}}"#));
        }
    }
}

/// Publish `ports` of a container of the network (`-p host:container`).
pub fn publish(sock: &Path, ports: &[Port]) -> Result<()> {
    if ports.is_empty() {
        return Ok(());
    }
    unpublish(sock, ports);
    net::forward_slirp(sock, ports)
}
