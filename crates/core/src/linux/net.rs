//! Container networking without any external tool:
//!   bridge `rdbr0` (172.30.0.0/16 + fd00:5a57::/64 + any `network create --subnet`)  <->  veth pair per
//!   container  ->  nftables masquerade (NAT44 and NAT66) to the outside
//! plus a generated resolv.conf and user-space TCP/UDP forwarders for `-p`.
//! The bridge and its NAT rules exist only while some container uses them (see `Presence`).
use super::netlink::{self as nl, Socket, NETLINK_NETFILTER, NETLINK_ROUTE};
use crate::{
    cli::Port,
    error::{Error, Result},
    networks::{self, ip6_of, Subnet, DEFAULT_SUBNET, GATEWAY6, PREFIX6},
    registry::{self, Entry},
};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs,
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket},
    os::fd::AsRawFd,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

pub const BRIDGE: &str = "rdbr0";
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

/// Best-effort sysctl write (IPv6 knobs may not exist on a host without IPv6).
fn sysctl(path: &str, value: &str) {
    let _ = fs::write(path, value);
}

/// Make the host forward IPv6. Forwarding switches the kernel from "host" to "router" mode, which stops it
/// honouring router advertisements (it would lose its own IPv6 default route), unless `accept_ra` is 2.
fn enable_ipv6_forwarding() {
    let all = "/proc/sys/net/ipv6/conf/all/forwarding";
    if fs::read_to_string(all).map(|v| v.trim() == "1").unwrap_or(true) {
        return; // already on (or no IPv6 at all)
    }
    for e in fs::read_dir("/proc/sys/net/ipv6/conf").into_iter().flatten().flatten() {
        let ra = e.path().join("accept_ra");
        if fs::read_to_string(&ra).is_ok_and(|v| v.trim() == "1") {
            let _ = fs::write(&ra, "2");
        }
    }
    sysctl(all, "1");
}

/// Create the bridge, enable forwarding and (re)install the NAT rules. Safe to call repeatedly.
/// IPv6 is best effort: a host without it just keeps IPv4.
fn ensure_bridge() -> Result<()> {
    let rt = Socket::open(NETLINK_ROUTE)?;
    if ifindex(BRIDGE) == 0 {
        rt.request("create bridge", nl::new_bridge(BRIDGE), Some(libc::EEXIST))?;
    }
    let idx = need_ifindex(BRIDGE)?;
    let subnets = networks::all_subnets();
    for s in &subnets {
        rt.request("bridge address", nl::add_addr(idx, &s.gateway().octets(), s.prefix), Some(libc::EEXIST))?;
    }
    sysctl(&format!("/proc/sys/net/ipv6/conf/{BRIDGE}/disable_ipv6"), "0");
    let v6 = rt.request("bridge IPv6 address", nl::add_addr(idx, &GATEWAY6.octets(), PREFIX6), Some(libc::EEXIST)).is_ok();
    rt.request("bridge up", nl::link_up(idx), None)?;
    fs::write("/proc/sys/net/ipv4/ip_forward", "1").map_err(|e| Error::io("enable ip_forward", e))?;
    let nft = Socket::open(NETLINK_NETFILTER)?;
    let nets: Vec<_> = subnets.iter().map(|s| (s.base.octets().to_vec(), s.prefix)).collect();
    nft.nft_batch(&nl::nat_batch(NFT_TABLE, "postrouting", &nets, false))?;
    if v6 {
        enable_ipv6_forwarding();
        let mut net6 = GATEWAY6.octets();
        net6[15] = 0; // the /64
        let net6 = (net6.to_vec(), PREFIX6);
        let _ = nft.nft_batch(&nl::nat_batch(NFT_TABLE, "postrouting", &[net6], true)); // NAT66; no nf_tables IPv6 NAT = no v6 egress
    }
    Ok(())
}

/// Open (creating) one of the lock files in the data dir.
fn open_lock_file(name: &str) -> Result<fs::File> {
    let dir = registry::data_dir();
    fs::create_dir_all(&dir).map_err(|e| Error::io("mkdir data dir", e))?;
    fs::OpenOptions::new().create(true).append(true).open(dir.join(name)).map_err(|e| Error::io(format!("open {name}"), e))
}

fn flock(f: &fs::File, op: i32) -> bool {
    unsafe { libc::flock(f.as_raw_fd(), op) == 0 }
}

/// Holds an exclusive lock while an address is picked, so two simultaneous `run`s never collide.
pub struct Lease {
    _lock: fs::File,
    pub ip: Ipv4Addr,
    pub subnet: Subnet,
}

/// A shared lock on `net.users`, held by every supervisor that uses the bridge for as long as it lives
/// (the kernel drops it if the supervisor is killed). `teardown_if_idle` removes the bridge only when it can
/// take that lock exclusively, i.e. when nobody holds one. The lock is taken in `Lease::acquire`, under
/// `net.lock`, so a container that is starting can never have the bridge removed from under it.
pub struct Presence(#[allow(dead_code)] fs::File);

impl Lease {
    pub fn acquire(network: Option<&str>) -> Result<(Lease, Presence)> {
        if let Some(n) = network
            && !networks::exists(n)
        {
            return Err(Error::Usage(format!("network {n:?} does not exist: run `docker network create {n}`")));
        }
        let lock = open_lock_file("net.lock")?;
        if !flock(&lock, libc::LOCK_EX) {
            return Err(Error::sys("lock net.lock"));
        }
        ensure_bridge()?;
        let users = open_lock_file("net.users")?;
        if !flock(&users, libc::LOCK_SH) {
            return Err(Error::sys("lock net.users"));
        }
        let subnet = networks::subnet_of(network);
        let used: HashSet<Ipv4Addr> = registry::list()
            .iter()
            .filter(|e| e.running())
            .filter_map(|e| e.ip.as_deref()?.parse().ok())
            .collect();
        // continue after the last address handed out, so a just-freed address is not reused right away
        let last_name = match (subnet == DEFAULT_SUBNET, network) {
            (false, Some(n)) => format!("net.last-{n}"),
            _ => "net.last".to_string(),
        };
        let last_file = registry::data_dir().join(last_name);
        let after = fs::read_to_string(&last_file).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(1);
        let (ip, n) = subnet.pick_after(&used, after).ok_or_else(|| Error::Usage("no free container addresses left".into()))?;
        let _ = fs::write(&last_file, n.to_string());
        Ok((Lease { _lock: lock, ip, subnet }, Presence(users)))
    }
}

/// Remove the bridge and the NAT rules if no container uses them any more. Call after dropping our own
/// `Presence`. Takes `net.lock`, so it cannot race a container that is starting.
pub fn teardown_if_idle() {
    let Ok(lock) = open_lock_file("net.lock") else { return };
    if !flock(&lock, libc::LOCK_EX) {
        return;
    }
    let Ok(users) = open_lock_file("net.users") else { return };
    if !flock(&users, libc::LOCK_EX | libc::LOCK_NB) {
        return; // somebody is still (or just now) using the bridge
    }
    if let Ok(rt) = Socket::open(NETLINK_ROUTE)
        && let i @ 1.. = ifindex(BRIDGE)
    {
        let _ = rt.request("delete bridge", nl::del_link(i), None);
    }
    if let Ok(nft) = Socket::open(NETLINK_NETFILTER) {
        let _ = nft.nft_batch(&nl::drop_table_batch(NFT_TABLE, false));
        let _ = nft.nft_batch(&nl::drop_table_batch(NFT_TABLE, true));
    }
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

/// Container side (inside its new network namespace): loopback, addresses, default routes.
pub fn configure_inside(ip: Ipv4Addr, subnet: Subnet) -> Result<()> {
    let rt = Socket::open(NETLINK_ROUTE)?;
    rt.request("lo up", nl::link_up(need_ifindex("lo")?), None)?;
    let eth = need_ifindex("eth0")?;
    // announce the address to the neighbours (gratuitous ARP) so nobody keeps a stale entry for it
    let _ = fs::write("/proc/sys/net/ipv4/conf/eth0/arp_notify", "1");
    rt.request("eth0 address", nl::add_addr(eth, &ip.octets(), subnet.prefix), None)?;
    rt.request("eth0 up", nl::link_up(eth), None)?;
    rt.request("default route", nl::add_default_route(&subnet.gateway().octets()), None)?;
    // IPv6 (ULA, derived from the IPv4 address): best effort, the container works on IPv4 without it
    sysctl("/proc/sys/net/ipv6/conf/eth0/disable_ipv6", "0");
    if rt.request("eth0 IPv6 address", nl::add_addr(eth, &ip6_of(ip).octets(), PREFIX6), Some(libc::EEXIST)).is_ok() {
        let _ = rt.request("IPv6 default route", nl::add_default_route(&GATEWAY6.octets()), Some(libc::EEXIST));
    }
    Ok(())
}

/// Per-container files that are bind-mounted into it (resolv.conf, hosts).
pub fn etc_dir(id: &str) -> std::path::PathBuf {
    std::path::Path::new("/run/rust-docker").join(format!("{id}.etc"))
}

/// Rewrite /etc/hosts of every running member of `network`, in place so the bind mounts see the change.
pub fn refresh_hosts(network: &str) {
    use crate::networks::{hosts_file, members, Host};
    let _lock = open_lock_file("net.lock").ok().inspect(|f| {
        flock(f, libc::LOCK_EX);
    });
    let list = members(Some(network), "");
    let hosts: Vec<Host> = list.iter().filter_map(Host::of).collect();
    for (e, own) in list.iter().zip(&hosts) {
        let path = etc_dir(&e.id).join("hosts");
        if path.is_file() {
            let _ = fs::write(&path, hosts_file(own, &hosts)); // truncates and rewrites the same inode
        }
    }
}

/// /etc/hosts of one container with no peers (after `network disconnect`).
pub fn reset_hosts(e: &Entry) {
    use crate::networks::{hosts_file, Host};
    let path = etc_dir(&e.id).join("hosts");
    if let (true, Some(own)) = (path.is_file(), Host::of(e)) {
        let _ = fs::write(&path, hosts_file(&own, &[]));
    }
}

/// `--network none`: just bring loopback up.
pub fn loopback_up() -> Result<()> {
    let rt = Socket::open(NETLINK_ROUTE)?;
    rt.request("lo up", nl::link_up(need_ifindex("lo")?), None)
}

/// resolv.conf for the container. In an isolated namespace the host's loopback stubs
/// (systemd-resolved's 127.0.0.53, ...) and unscoped link-local IPv6 servers are unreachable, so drop them;
/// fall back to public DNS.
pub fn resolv_conf(host: &str, isolated: bool) -> String {
    let mut ns: Vec<&str> = host
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !isolated || !(s.starts_with("127.") || *s == "::1" || s.to_lowercase().starts_with("fe80:")))
        .collect();
    if ns.is_empty() {
        ns = vec!["1.1.1.1", "8.8.8.8"];
    }
    ns.iter().map(|n| format!("nameserver {n}\n")).collect()
}

// ---- -p: user-space forwarding (no kernel DNAT rules to keep in sync)

enum Bound {
    Tcp(TcpListener, u16),
    Udp(UdpSocket, u16),
}

pub struct Listeners(Vec<Bound>);

/// Bind every host port up front so a conflict fails the `run` before anything starts.
pub fn bind_ports(ports: &[Port]) -> Result<Listeners> {
    let mut v = vec![];
    for p in ports {
        let addr = SocketAddr::new(p.host_ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), p.host);
        let ctx = format!("publish port {}", p.host);
        v.push(if p.udp {
            Bound::Udp(UdpSocket::bind(addr).map_err(|e| Error::io(ctx, e))?, p.container)
        } else {
            Bound::Tcp(TcpListener::bind(addr).map_err(|e| Error::io(ctx, e))?, p.container)
        });
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
        for b in self.0 {
            match b {
                Bound::Tcp(listener, port) => {
                    thread::spawn(move || {
                        for conn in listener.incoming().flatten() {
                            thread::spawn(move || forward(conn, SocketAddr::from((ip, port))));
                        }
                    });
                }
                Bound::Udp(sock, port) => {
                    thread::spawn(move || forward_udp(sock, SocketAddr::from((ip, port))));
                }
            }
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

/// A UDP "connection": one upstream socket (its own source port towards the container) per client address.
struct Session {
    up: UdpSocket,
    /// unix seconds of the last packet in either direction
    last: AtomicU64,
}

type Sessions = Arc<Mutex<HashMap<SocketAddr, Arc<Session>>>>;

/// A session with no traffic for this long is dropped (and its thread ends).
const UDP_IDLE_SECS: u64 = 60;

/// Datagram forwarder: packets from each client go to the container through that client's own upstream
/// socket, and whatever comes back on it is sent to the client from the published port.
// ponytail: one thread (+64K buffer) per active client; an event loop if thousands of clients matter.
fn forward_udp(sock: UdpSocket, to: SocketAddr) {
    let sessions: Sessions = Arc::default();
    let mut buf = vec![0u8; 65535];
    loop {
        let Ok((n, client)) = sock.recv_from(&mut buf) else {
            thread::sleep(Duration::from_millis(10));
            continue;
        };
        let session = {
            let mut map = sessions.lock().unwrap();
            match map.get(&client) {
                Some(s) => s.clone(),
                None => match open_session(&sock, &sessions, client, to) {
                    Some(s) => map.entry(client).or_insert(s).clone(),
                    None => continue,
                },
            }
        };
        session.last.store(registry::now_unix(), Ordering::Relaxed);
        let _ = session.up.send(&buf[..n]);
    }
}

fn open_session(sock: &UdpSocket, sessions: &Sessions, client: SocketAddr, to: SocketAddr) -> Option<Arc<Session>> {
    let up = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    up.connect(to).ok()?;
    up.set_read_timeout(Some(Duration::from_secs(5))).ok()?; // also the idle-check tick
    let reply = sock.try_clone().ok()?;
    let s = Arc::new(Session { up, last: AtomicU64::new(registry::now_unix()) });
    let (mine, table) = (s.clone(), sessions.clone());
    thread::spawn(move || {
        let mut buf = vec![0u8; 65535];
        loop {
            match mine.up.recv(&mut buf) {
                Ok(n) => {
                    mine.last.store(registry::now_unix(), Ordering::Relaxed);
                    let _ = reply.send_to(&buf[..n], client);
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    let mut map = table.lock().unwrap();
                    // re-checked under the lock: the client may have just sent again
                    if registry::now_unix().saturating_sub(mine.last.load(Ordering::Relaxed)) >= UDP_IDLE_SECS {
                        if map.get(&client).is_some_and(|c| Arc::ptr_eq(c, &mine)) {
                            map.remove(&client);
                        }
                        return;
                    }
                }
                Err(_) => {} // ICMP "port unreachable": the container is not listening (yet); keep the session
            }
        }
    });
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolv_conf_drops_loopback_only_when_isolated() {
        let host = "# generated\nnameserver 127.0.0.53\nnameserver 10.255.255.254\nnameserver fe80::1\nnameserver 2001:4860:4860::8888\n";
        assert_eq!(resolv_conf(host, true), "nameserver 10.255.255.254\nnameserver 2001:4860:4860::8888\n");
        assert_eq!(resolv_conf(host, false).lines().count(), 4);
        assert_eq!(resolv_conf("nameserver 127.0.0.1\nnameserver ::1\n", true), "nameserver 1.1.1.1\nnameserver 8.8.8.8\n");
        assert_eq!(resolv_conf("", false), "nameserver 1.1.1.1\nnameserver 8.8.8.8\n");
    }

    #[test]
    fn udp_forwarder_relays_per_client_and_back() {
        // an echo "container", the forwarder in front of it, two clients
        let echo = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = echo.local_addr().unwrap();
        thread::spawn(move || {
            let mut b = [0u8; 64];
            while let Ok((n, from)) = echo.recv_from(&mut b) {
                let _ = echo.send_to(&b[..n], from);
            }
        });
        let front = UdpSocket::bind("127.0.0.1:0").unwrap();
        let front_addr = front.local_addr().unwrap();
        thread::spawn(move || forward_udp(front, to));
        for msg in [&b"one"[..], b"two"] {
            let c = UdpSocket::bind("127.0.0.1:0").unwrap();
            c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            for _ in 0..2 {
                // the second round goes through the existing session
                c.send_to(msg, front_addr).unwrap();
                let mut b = [0u8; 64];
                let (n, from) = c.recv_from(&mut b).unwrap();
                assert_eq!((&b[..n], from), (msg, front_addr));
            }
        }
    }
}
