//! Container networking without any external tool:
//!   bridge `rdbr0` (172.30.0.0/16)  <->  veth pair per container  ->  nftables masquerade to the internet
//! plus a generated resolv.conf and a user-space forwarder for `-p`.
use super::netlink::{self as nl, Socket, NETLINK_NETFILTER, NETLINK_ROUTE};
use crate::{
    cli::Port,
    error::{Error, Result},
    registry,
};
use std::{
    collections::HashSet,
    ffi::CString,
    fs,
    io,
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    os::fd::AsRawFd,
    thread,
    time::Duration,
};

pub const BRIDGE: &str = "rdbr0";
const GATEWAY: [u8; 4] = [172, 30, 0, 1];
const SUBNET: [u8; 4] = [172, 30, 0, 0];
const PREFIX: u8 = 16;
const NFT_TABLE: &str = "rust-docker";

fn ifindex(name: &str) -> u32 {
    let c = CString::new(name).expect("no NUL");
    unsafe { libc::if_nametoindex(c.as_ptr()) }
}

fn need_ifindex(name: &str) -> Result<u32> {
    match ifindex(name) {
        0 => Err(Error::sys(format!("find interface {name}"))),
        i => Ok(i),
    }
}

/// Create the bridge, enable forwarding and install the NAT rule. Safe to call repeatedly.
fn ensure_bridge() -> Result<()> {
    let rt = Socket::open(NETLINK_ROUTE)?;
    if ifindex(BRIDGE) == 0 {
        rt.request("create bridge", nl::new_bridge(BRIDGE), Some(libc::EEXIST))?;
    }
    let idx = need_ifindex(BRIDGE)?;
    rt.request("bridge address", nl::add_addr(idx, GATEWAY, PREFIX), Some(libc::EEXIST))?;
    rt.request("bridge up", nl::link_up(idx), None)?;
    fs::write("/proc/sys/net/ipv4/ip_forward", "1").map_err(|e| Error::io("enable ip_forward", e))?;
    let nft = Socket::open(NETLINK_NETFILTER)?;
    nft.nft_batch(&nl::masquerade_batch(NFT_TABLE, "postrouting", SUBNET, PREFIX))?;
    Ok(())
}

/// Holds an exclusive lock while an address is picked, so two simultaneous `run`s never collide.
pub struct Lease {
    _lock: fs::File,
    pub ip: Ipv4Addr,
}

impl Lease {
    pub fn acquire() -> Result<Lease> {
        let dir = registry::data_dir();
        fs::create_dir_all(&dir).map_err(|e| Error::io("mkdir data dir", e))?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("net.lock"))
            .map_err(|e| Error::io("open net.lock", e))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } < 0 {
            return Err(Error::sys("lock net.lock"));
        }
        ensure_bridge()?;
        let used: HashSet<Ipv4Addr> = registry::list()
            .iter()
            .filter(|e| e.running())
            .filter_map(|e| e.ip.as_deref()?.parse().ok())
            .collect();
        // continue after the last address handed out, so a just-freed address is not reused right away
        let last_file = dir.join("net.last");
        let after = fs::read_to_string(&last_file).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(1);
        let (ip, n) = pick_ip_after(&used, after).ok_or_else(|| Error::Usage("no free container addresses left".into()))?;
        let _ = fs::write(&last_file, n.to_string());
        Ok(Lease { _lock: lock, ip })
    }
}

fn addr(n: u32) -> Ipv4Addr {
    Ipv4Addr::new(172, 30, (n >> 8) as u8, (n & 255) as u8)
}

/// The next free address in 172.30.0.0/16 *after* index `after` (wrapping; skips network, gateway, broadcast).
/// Rotating instead of reusing the lowest free address keeps neighbours' ARP caches from pointing at a dead
/// container's MAC when its address goes to a new one.
pub fn pick_ip_after(used: &HashSet<Ipv4Addr>, after: u32) -> Option<(Ipv4Addr, u32)> {
    (0..65533u32)
        .map(|k| 2 + (after.max(1) - 1 + k) % 65533) // index `after` + 1, wrapped into 2..=65534
        .map(|n| (addr(n), n))
        .find(|(ip, _)| ip.octets()[3] != 0 && ip.octets()[3] != 255 && !used.contains(ip))
}

/// Host side: create the veth pair, plug one end into the bridge, push the other into `pid`'s namespace.
pub fn attach(pid: u32, id: &str) -> Result<()> {
    let rt = Socket::open(NETLINK_ROUTE)?;
    let (host, peer) = (format!("rdv{}", &id[..8]), format!("rdp{}", &id[..8]));
    rt.request("create veth", nl::new_veth(&host, &peer), None)?;
    let (h, p, b) = (need_ifindex(&host)?, need_ifindex(&peer)?, need_ifindex(BRIDGE)?);
    rt.request("join bridge", nl::set_master(h, b), None)?;
    rt.request("veth up", nl::link_up(h), None)?;
    rt.request("move veth into container", nl::move_to_ns(p, pid, "eth0"), None)
}

/// Container side (inside its new network namespace): loopback, address, default route.
pub fn configure_inside(ip: Ipv4Addr) -> Result<()> {
    let rt = Socket::open(NETLINK_ROUTE)?;
    rt.request("lo up", nl::link_up(need_ifindex("lo")?), None)?;
    let eth = need_ifindex("eth0")?;
    // announce the address to the neighbours (gratuitous ARP) so nobody keeps a stale entry for it
    let _ = fs::write("/proc/sys/net/ipv4/conf/eth0/arp_notify", "1");
    rt.request("eth0 address", nl::add_addr(eth, ip.octets(), PREFIX), None)?;
    rt.request("eth0 up", nl::link_up(eth), None)?;
    rt.request("default route", nl::add_default_route(GATEWAY), None)
}

/// Per-container files that are bind-mounted into it (resolv.conf, hosts).
pub fn etc_dir(id: &str) -> std::path::PathBuf {
    std::path::Path::new("/run/rust-docker").join(format!("{id}.etc"))
}

/// Rewrite /etc/hosts of every running member of `network`, in place so the bind mounts see the change.
pub fn refresh_hosts(network: &str) {
    use crate::networks::{hosts_file, members, Host};
    let _lock = (|| {
        let f = fs::OpenOptions::new().create(true).append(true).open(registry::data_dir().join("net.lock")).ok()?;
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        Some(f)
    })();
    let list = members(Some(network), "");
    let hosts: Vec<Host> = list.iter().filter_map(Host::of).collect();
    for (e, own) in list.iter().zip(&hosts) {
        let path = etc_dir(&e.id).join("hosts");
        if path.is_file() {
            let _ = fs::write(&path, hosts_file(own, &hosts)); // truncates and rewrites the same inode
        }
    }
}

/// `--network none`: just bring loopback up.
pub fn loopback_up() -> Result<()> {
    let rt = Socket::open(NETLINK_ROUTE)?;
    rt.request("lo up", nl::link_up(need_ifindex("lo")?), None)
}

/// resolv.conf for the container. In an isolated namespace the host's loopback stubs
/// (systemd-resolved's 127.0.0.53, ...) are unreachable, so drop them; fall back to public DNS.
pub fn resolv_conf(host: &str, isolated: bool) -> String {
    let mut ns: Vec<&str> = host
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !isolated || !(s.starts_with("127.") || *s == "::1"))
        .collect();
    if ns.is_empty() {
        ns = vec!["1.1.1.1", "8.8.8.8"];
    }
    ns.iter().map(|n| format!("nameserver {n}\n")).collect()
}

// ---- -p: user-space TCP forwarding (no kernel DNAT rules to keep in sync)

pub struct Listeners(Vec<(TcpListener, u16)>);

/// Bind every host port up front so a conflict fails the `run` before anything starts.
pub fn bind_ports(ports: &[Port]) -> Result<Listeners> {
    let mut v = vec![];
    for p in ports {
        let l = TcpListener::bind(("0.0.0.0", p.host))
            .map_err(|e| Error::io(format!("publish port {}", p.host), e))?;
        v.push((l, p.container));
    }
    Ok(Listeners(v))
}

impl Listeners {
    /// For a helper process: forward forever.
    pub fn serve_forever(self, ip: Ipv4Addr) -> ! {
        self.serve(ip);
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }

    /// Start forwarding to the container. Threads die with the supervisor process.
    pub fn serve(self, ip: Ipv4Addr) {
        for (listener, port) in self.0 {
            thread::spawn(move || {
                for conn in listener.incoming().flatten() {
                    thread::spawn(move || forward(conn, SocketAddr::from((ip, port))));
                }
            });
        }
    }
}

fn forward(client: TcpStream, to: SocketAddr) {
    let Ok(server) = TcpStream::connect_timeout(&to, Duration::from_secs(5)) else { return };
    let (Ok(mut c2), Ok(mut s2)) = (client.try_clone(), server.try_clone()) else { return };
    let (mut c, mut s) = (client, server);
    let up = thread::spawn(move || {
        let _ = io::copy(&mut c2, &mut s2);
        let _ = s2.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut s, &mut c);
    let _ = c.shutdown(Shutdown::Write);
    let _ = up.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_rotate_instead_of_reusing_the_lowest() {
        let mut used = HashSet::new();
        assert_eq!(pick_ip_after(&used, 1), Some((Ipv4Addr::new(172, 30, 0, 2), 2)));
        assert_eq!(pick_ip_after(&used, 2), Some((Ipv4Addr::new(172, 30, 0, 3), 3)), "continues after the last one");
        used.insert(Ipv4Addr::new(172, 30, 0, 3));
        assert_eq!(pick_ip_after(&used, 2).unwrap().1, 4, "skips addresses in use");
        // skips .255 and .0 of each /24
        assert_eq!(pick_ip_after(&used, 254), Some((Ipv4Addr::new(172, 30, 1, 1), 257)));
        // wraps around the end of the range
        assert_eq!(pick_ip_after(&HashSet::new(), 65534), Some((Ipv4Addr::new(172, 30, 0, 2), 2)));
        // exhausted
        let all: HashSet<Ipv4Addr> = (2u32..65535).map(addr).collect();
        assert_eq!(pick_ip_after(&all, 7), None);
    }

    #[test]
    fn resolv_conf_drops_loopback_only_when_isolated() {
        let host = "# generated\nnameserver 127.0.0.53\nnameserver 10.255.255.254\n";
        assert_eq!(resolv_conf(host, true), "nameserver 10.255.255.254\n");
        assert_eq!(resolv_conf(host, false), "nameserver 127.0.0.53\nnameserver 10.255.255.254\n");
        assert_eq!(resolv_conf("nameserver 127.0.0.1\n", true), "nameserver 1.1.1.1\nnameserver 8.8.8.8\n");
        assert_eq!(resolv_conf("", false), "nameserver 1.1.1.1\nnameserver 8.8.8.8\n");
    }
}
