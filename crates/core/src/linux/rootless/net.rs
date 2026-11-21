//! Rootless networking: the container gets its own network namespace (inside our user namespace) and a
//! user-space helper (`slirp4netns` or `pasta`) provides outbound access and port publishing through the
//! namespace's tap device. We do NOT implement our own TCP/IP stack; with no helper installed the engine
//! falls back to sharing the host network namespace (reported with a warning).
use crate::{cli::Port, error::{Error, Result}};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    Slirp4netns,
    Pasta,
}

/// Address the helper's DNS forwarder answers on (both helpers can map it to the host's resolver).
pub const DNS: &str = "10.0.2.3";

pub fn find_in_path(prog: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(prog)).find(|p| p.is_file())
}

/// Best installed helper; `SASTA_ROOTLESS_NET=slirp4netns|pasta|host` overrides (host = none).
pub fn detect() -> Option<Helper> {
    match std::env::var("SASTA_ROOTLESS_NET").as_deref() {
        Ok("host") => None,
        Ok("pasta") => find_in_path("pasta").map(|_| Helper::Pasta),
        Ok("slirp4netns") => find_in_path("slirp4netns").map(|_| Helper::Slirp4netns),
        _ => find_in_path("slirp4netns").map(|_| Helper::Slirp4netns).or_else(|| find_in_path("pasta").map(|_| Helper::Pasta)),
    }
}

/// `-t 8080:80,9090:90` (TCP only: the CLI has no UDP publishing).
fn pasta_ports(ports: &[Port]) -> String {
    if ports.is_empty() {
        "none".into()
    } else {
        ports.iter().map(|p| format!("{}:{}", p.host, p.container)).collect::<Vec<_>>().join(",")
    }
}

pub fn pasta_args(pid: u32, ports: &[Port]) -> Vec<String> {
    let mut a: Vec<String> = ["--foreground", "--quiet", "--config-net", "--dns-forward", DNS, "-u", "none", "-U", "none", "-T", "none", "-t"]
        .map(String::from)
        .to_vec();
    a.push(pasta_ports(ports));
    a.push(pid.to_string());
    a
}

pub fn slirp_args(pid: u32, api: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["--configure", "--mtu=65520", "--disable-host-loopback"].map(String::from).to_vec();
    a.push(format!("--api-socket={}", api.display()));
    a.push(pid.to_string());
    a.push("tap0".into());
    a
}

/// slirp4netns API request that publishes one TCP port to the guest address slirp4netns configured.
pub fn hostfwd_json(p: &Port) -> String {
    format!(
        r#"{{"execute":"add_hostfwd","arguments":{{"proto":"tcp","host_addr":"0.0.0.0","host_port":{},"guest_addr":"10.0.2.100","guest_port":{}}}}}"#,
        p.host, p.container
    )
}

/// True when the text of /proc/<pid>/net/route has a default route (the helper finished configuring).
pub fn has_default_route(route: &str) -> bool {
    route.lines().skip(1).any(|l| l.split_whitespace().nth(1) == Some("00000000"))
}

fn wait_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) -> Result<()> {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(Error::Usage(format!("rootless networking: timed out waiting for {what}")))
}

/// Start the helper for the container whose network namespace belongs to `pid`; returns the helper's pid
/// (the caller kills it when the container is gone) once the namespace has a default route.
pub fn start(h: Helper, pid: u32, ports: &[Port], run_dir: &Path) -> Result<i32> {
    let _ = fs::create_dir_all(run_dir);
    let api = run_dir.join("slirp.sock");
    let (prog, args) = match h {
        Helper::Slirp4netns => ("slirp4netns", slirp_args(pid, &api)),
        Helper::Pasta => ("pasta", pasta_args(pid, ports)),
    };
    let child = Command::new(prog)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| Error::io(format!("start {prog}"), e))?;
    let hpid = child.id() as i32;
    std::mem::forget(child); // reaped by the caller with waitpid
    let route = format!("/proc/{pid}/net/route");
    let ready = wait_for("the network helper", Duration::from_secs(10), || fs::read_to_string(&route).is_ok_and(|r| has_default_route(&r)));
    let ready = ready.and_then(|()| {
        if h == Helper::Slirp4netns {
            forward_slirp(&api, ports)
        } else {
            Ok(())
        }
    });
    if let Err(e) = ready {
        super::super::sys::kill(hpid);
        let _ = super::super::sys::wait(hpid);
        return Err(e);
    }
    Ok(hpid)
}

fn forward_slirp(api: &Path, ports: &[Port]) -> Result<()> {
    if ports.is_empty() {
        return Ok(());
    }
    wait_for("the slirp4netns API socket", Duration::from_secs(5), || api.exists())?;
    for p in ports {
        let mut s = UnixStream::connect(api).map_err(|e| Error::io("connect slirp4netns api", e))?;
        s.write_all(hostfwd_json(p).as_bytes()).and_then(|()| s.write_all(b"\n")).map_err(|e| Error::io("slirp4netns api", e))?;
        let mut reply = String::new();
        BufReader::new(&s).read_line(&mut reply).map_err(|e| Error::io("slirp4netns api", e))?;
        if reply.contains("\"error\"") {
            return Err(Error::Usage(format!("publish port {}: {}", p.host, reply.trim())));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_detection() {
        let none = "Iface\tDestination\tGateway\nlo\t0000007F\t00000000\n";
        let some = "Iface\tDestination\tGateway\ntap0\t00000000\t0202000A\n";
        assert!(!has_default_route(none));
        assert!(has_default_route(some));
    }

    #[test]
    fn helper_command_lines() {
        let ports = [Port { host: 8080, container: 80, host_ip: None, udp: false }];
        assert_eq!(pasta_ports(&ports), "8080:80");
        assert_eq!(pasta_ports(&[]), "none");
        let a = pasta_args(7, &ports);
        assert_eq!(a.last().unwrap(), "7");
        assert!(a.windows(2).any(|w| w == ["-t", "8080:80"]));
        assert_eq!(slirp_args(7, Path::new("/s")).last().unwrap(), "tap0");
        assert!(hostfwd_json(&ports[0]).contains(r#""host_port":8080"#));
    }
}
