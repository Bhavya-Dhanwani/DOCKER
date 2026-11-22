//! User-defined networks. A network is a name; containers started with `--network <name>` can reach each
//! other by container name or `--network-alias`, through a generated /etc/hosts that every supervisor keeps
//! current. (Like Docker's default bridge, containers outside any named network get no name resolution.
//! Unlike Docker, networks share one bridge, so they provide discovery, not isolation.)
use crate::{
    cli::is_volume_name,
    error::{Error, Result},
    registry::{self, Entry},
};
use std::{
    collections::HashSet,
    fs,
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};

fn root() -> PathBuf {
    registry::data_dir().join("networks")
}

fn check(name: &str) -> Result<()> {
    if !is_volume_name(name) {
        return Err(Error::Usage(format!("bad network name {name:?}")));
    }
    if matches!(name, "bridge" | "host" | "none") {
        return Err(Error::Usage(format!("{name:?} is a built-in network name")));
    }
    Ok(())
}

pub fn exists(name: &str) -> bool {
    root().join(format!("{name}.json")).is_file()
}

pub fn create(name: &str) -> Result<()> {
    create_in(&root(), name, None)
}

/// `network create --subnet 10.5.0.0/24`
pub fn create_with(name: &str, subnet: Option<&str>) -> Result<()> {
    create_in(&root(), name, subnet)
}

fn create_in(root: &Path, name: &str, subnet: Option<&str>) -> Result<()> {
    check(name)?;
    let subnet = subnet.map(Subnet::parse).transpose()?;
    if let Some(s) = subnet {
        let mut taken = vec![DEFAULT_SUBNET];
        taken.extend(list_in(root).iter().filter_map(|n| read_subnet(&root.join(format!("{n}.json")))));
        if let Some(o) = taken.into_iter().find(|o| o.overlaps(&s)) {
            return Err(Error::Usage(format!("subnet {} overlaps {}", s.cidr(), o.cidr())));
        }
    }
    let f = root.join(format!("{name}.json"));
    if f.exists() {
        return Err(Error::Usage(format!("network {name} already exists")));
    }
    fs::create_dir_all(root).map_err(|e| Error::io("create networks dir", e))?;
    let sub = subnet.map(|s| format!(",\"subnet\":\"{}\"", s.cidr())).unwrap_or_default();
    fs::write(&f, format!("{{\"name\":{name:?},\"created_unix\":{}{sub}}}", registry::now_unix())).map_err(|e| Error::io("write network", e))
}

pub fn list() -> Vec<String> {
    list_in(&root())
}

fn list_in(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(root)
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(String::from)).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub fn remove(name: &str) -> Result<()> {
    if !exists(name) {
        return Err(Error::Usage(format!("no such network: {name}")));
    }
    let users = members(Some(name), "");
    if !users.is_empty() {
        let labels: Vec<&str> = users.iter().map(Entry::label).collect();
        return Err(Error::Usage(format!("network {name} is in use by {}", labels.join(", "))));
    }
    fs::remove_file(root().join(format!("{name}.json"))).map_err(|e| Error::io("remove network", e))
}

/// Running containers attached to `network` (all of them if `exclude` is empty, else without that id).
pub fn members(network: Option<&str>, exclude: &str) -> Vec<Entry> {
    let Some(net) = network else { return vec![] };
    registry::list()
        .into_iter()
        .filter(|e| e.id != exclude && e.ip.is_some() && e.config.as_ref().is_some_and(|c| c.network_name.as_deref() == Some(net)))
        .filter(Entry::running)
        .collect()
}

/// One host entry: an address and every name it answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub ip: String,
    pub names: Vec<String>,
    /// `--hostname`; the default is `container`.
    pub hostname: Option<String>,
    /// `--add-host` entries as (name, ip), `host-gateway` already resolved.
    pub extra: Vec<(String, String)>,
}

/// `name:ip` entries of `--add-host`; `host-gateway` means the address of the host on the container's network.
pub fn resolve_extra_hosts(specs: &[String], network: Option<&str>) -> Vec<(String, String)> {
    let gateway = subnet_of(network).gateway().to_string();
    specs
        .iter()
        .filter_map(|s| s.split_once(':'))
        .map(|(n, ip)| (n.to_string(), if ip == "host-gateway" { gateway.clone() } else { ip.to_string() }))
        .collect()
}

impl Host {
    pub fn of(e: &Entry) -> Option<Host> {
        let mut names: Vec<String> = e.name.iter().cloned().collect();
        names.push(e.id.chars().take(12).collect());
        names.extend(e.config.iter().flat_map(|c| c.aliases.iter().cloned()));
        let (hostname, extra) = match &e.config {
            Some(c) => (c.hostname.clone(), resolve_extra_hosts(&c.extra_hosts, c.network_name.as_deref())),
            None => (None, vec![]),
        };
        Some(Host { ip: e.ip.clone()?, names, hostname, extra })
    }
}

/// /etc/hosts for the container `own` (its own line also answers to its hostname, `container` unless `--hostname` says otherwise).
pub fn hosts_file(own: &Host, others: &[Host]) -> String {
    let line = |ip: &str, names: &[String]| {
        let mut seen: Vec<&str> = vec![];
        for n in names {
            if !seen.contains(&n.as_str()) {
                seen.push(n);
            }
        }
        format!("{ip}\t{}\n", seen.join(" "))
    };
    let mut s = String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    let mut mine = vec![own.hostname.clone().unwrap_or_else(|| "container".to_string())];
    mine.extend(own.names.iter().cloned());
    s.push_str(&line(&own.ip, &mine));
    // its own IPv6 address too (peers stay IPv4-only: some tools take the first address they get and do not retry)
    if let Ok(v4) = own.ip.parse::<Ipv4Addr>() {
        s.push_str(&line(&ip6_of(v4).to_string(), &mine));
    }
    for o in others.iter().filter(|o| o.ip != own.ip) {
        s.push_str(&line(&o.ip, &o.names));
    }
    for (name, ip) in &own.extra {
        s.push_str(&line(ip, std::slice::from_ref(name)));
    }
    s
}

// ---- addressing

/// An IPv4 subnet on the shared bridge. The gateway is its first address (.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub base: Ipv4Addr,
    pub prefix: u8,
}

/// The pool used by containers on the default bridge and on networks made without `--subnet`.
pub const DEFAULT_SUBNET: Subnet = Subnet { base: Ipv4Addr::new(172, 30, 0, 0), prefix: 16 };
/// IPv6 ULA /64 on the bridge; a container's address embeds its IPv4 one (see `ip6_of`).
pub const GATEWAY6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x5a57, 0, 0, 0, 0, 0, 1);
pub const PREFIX6: u8 = 64;

/// `172.30.0.5` -> `fd00:5a57::ac1e:5`: unique whenever the IPv4 address is, and needs no extra state.
pub fn ip6_of(v4: Ipv4Addr) -> Ipv6Addr {
    let o = v4.octets();
    Ipv6Addr::new(0xfd00, 0x5a57, 0, 0, 0, 0, u16::from_be_bytes([o[0], o[1]]), u16::from_be_bytes([o[2], o[3]]))
}

impl Subnet {
    /// `10.5.0.0/24`: network address, prefix 8..=29.
    pub fn parse(s: &str) -> Result<Subnet> {
        let bad = |m: &str| Error::Usage(format!("bad subnet {s:?}: {m}"));
        let (ip, p) = s.split_once('/').ok_or_else(|| bad("expected a.b.c.d/n"))?;
        let base: Ipv4Addr = ip.parse().map_err(|_| bad("not an IPv4 address"))?;
        let prefix: u8 = p.parse().ok().filter(|p| (8..=29).contains(p)).ok_or_else(|| bad("prefix must be 8-29"))?;
        let sub = Subnet { base, prefix };
        if u32::from(base) & !sub.mask() != 0 {
            return Err(bad("host bits are set (use the network address)"));
        }
        Ok(sub)
    }

    fn mask(&self) -> u32 {
        u32::MAX << (32 - self.prefix as u32)
    }

    pub fn size(&self) -> u32 {
        1 << (32 - self.prefix as u32)
    }

    pub fn gateway(&self) -> Ipv4Addr {
        self.addr(1)
    }

    pub fn addr(&self, n: u32) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.base) + n)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.base)
    }

    pub fn overlaps(&self, o: &Subnet) -> bool {
        self.contains(o.base) || o.contains(self.base)
    }

    pub fn cidr(&self) -> String {
        format!("{}/{}", self.base, self.prefix)
    }

    /// The next free address *after* host index `after` (wrapping; skips network, gateway, broadcast and
    /// any .0/.255). Rotating instead of reusing the lowest free address keeps neighbours' ARP caches from
    /// pointing at a dead container's MAC when its address goes to a new one.
    pub fn pick_after(&self, used: &HashSet<Ipv4Addr>, after: u32) -> Option<(Ipv4Addr, u32)> {
        let n = self.size() - 3;
        (0..n)
            .map(|k| 2 + (after.max(1) - 1 + k) % n) // index `after` + 1, wrapped into 2..=size-2
            .map(|i| (self.addr(i), i))
            .find(|(ip, _)| ip.octets()[3] != 0 && ip.octets()[3] != 255 && !used.contains(ip))
    }
}

/// The subnet of a user-defined network (the default pool if it was made without `--subnet`).
pub fn subnet_of(name: Option<&str>) -> Subnet {
    name.and_then(|n| read_subnet(&root().join(format!("{n}.json")))).unwrap_or(DEFAULT_SUBNET)
}

fn read_subnet(file: &Path) -> Option<Subnet> {
    let v: serde_json::Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    Subnet::parse(v.get("subnet")?.as_str()?).ok()
}

/// Every subnet in use: the default pool plus those of networks created with `--subnet`.
pub fn all_subnets() -> Vec<Subnet> {
    let mut v = vec![DEFAULT_SUBNET];
    v.extend(list().iter().filter_map(|n| read_subnet(&root().join(format!("{n}.json")))));
    v
}

fn refresh(network: &str) {
    #[cfg(target_os = "linux")]
    crate::platform::refresh_hosts(network);
    #[cfg(not(target_os = "linux"))]
    let _ = network;
}

/// `network connect`: add a running bridge container to a user-defined network (name discovery only: a
/// container keeps its address, so a network with its own `--subnet` can only be joined at `run` time).
pub fn connect(network: &str, container: &str) -> Result<()> {
    if !exists(network) {
        return Err(Error::Usage(format!("no such network: {network}")));
    }
    let e = registry::find(container)?;
    let (Some(ip), true) = (e.ip.as_deref().and_then(|i| i.parse::<Ipv4Addr>().ok()), e.running()) else {
        return Err(Error::Usage(format!("{} is not a running container on the bridge", e.label())));
    };
    if let Some(cur) = e.config.as_ref().and_then(|c| c.network_name.as_deref()) {
        return Err(Error::Usage(if cur == network {
            format!("{} is already on {network}", e.label())
        } else {
            format!("{} is already on {cur}: `docker network disconnect {cur} {}` first", e.label(), e.label())
        }));
    }
    if !subnet_of(Some(network)).contains(ip) {
        return Err(Error::Usage(format!("{network} has its own subnet; start the container with --network {network} instead")));
    }
    registry::update(&e.id, |e| {
        if let Some(c) = e.config.as_mut() {
            c.network_name = Some(network.to_string());
            c.network.get_or_insert(crate::cli::Network::Bridge);
        }
    })?;
    refresh(network);
    Ok(())
}

pub fn disconnect(network: &str, container: &str) -> Result<()> {
    let e = registry::find(container)?;
    if e.config.as_ref().and_then(|c| c.network_name.as_deref()) != Some(network) {
        return Err(Error::Usage(format!("{} is not connected to {network}", e.label())));
    }
    registry::update(&e.id, |e| {
        if let Some(c) = e.config.as_mut() {
            c.network_name = None;
        }
    })?;
    #[cfg(target_os = "linux")]
    crate::platform::reset_hosts(&e); // it no longer sees its old peers ...
    refresh(network); // ... and they no longer see it
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    fn host(ip: &str, names: &[&str]) -> Host {
        Host { ip: ip.into(), names: names.iter().map(|s| s.to_string()).collect(), hostname: None, extra: vec![] }
    }

    #[test]
    fn hosts_file_lists_self_and_peers() {
        let own = host("172.30.0.5", &["web", "abc123def456", "frontend"]);
        let h = hosts_file(&own, &[host("172.30.0.6", &["db", "ffff00001111"]), host("172.30.0.5", &["dup"])]);
        assert!(h.starts_with("127.0.0.1\tlocalhost\n::1\t"));
        assert!(h.contains("172.30.0.5\tcontainer web abc123def456 frontend\n"));
        assert!(h.contains("fd00:5a57::ac1e:5\tcontainer web abc123def456 frontend\n"));
        assert!(h.contains("172.30.0.6\tdb ffff00001111\n"));
        assert!(!h.contains("dup"), "an entry with our own address is not repeated");
        assert_eq!(hosts_file(&host("172.30.0.2", &["x", "x"]), &[]).lines().last().unwrap(), "fd00:5a57::ac1e:2\tcontainer x");
    }

    #[test]
    fn names_are_validated_and_networks_listed() {
        let root = std::env::temp_dir().join(format!("rd-net-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for bad in ["bridge", "host", "none", "../x", "", "a b"] {
            assert!(create_in(&root, bad, None).is_err(), "{bad}");
        }
        create_in(&root, "appnet", None).unwrap();
        create_in(&root, "backend", Some("10.5.0.0/24")).unwrap();
        assert!(create_in(&root, "appnet", None).is_err(), "no duplicates");
        assert_eq!(list_in(&root), ["appnet", "backend"]);
        assert!(create_in(&root, "x", Some("10.5.0.128/25")).is_err(), "overlap with another network");
        assert!(create_in(&root, "x", Some("172.30.4.0/24")).is_err(), "overlap with the default pool");
        assert_eq!(read_subnet(&root.join("backend.json")), Some(Subnet::parse("10.5.0.0/24").unwrap()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn subnets_parse_and_pick_addresses() {
        for bad in ["10.0.0.1/24", "10.0.0.0/30", "10.0.0.0/7", "10.0.0.0", "x/24", "10.0.0.0/24/1"] {
            assert!(Subnet::parse(bad).is_err(), "{bad}");
        }
        let s = Subnet::parse("10.9.0.0/24").unwrap();
        assert_eq!((s.gateway(), s.size(), s.cidr()), (Ipv4Addr::new(10, 9, 0, 1), 256, "10.9.0.0/24".to_string()));
        assert!(s.contains(Ipv4Addr::new(10, 9, 0, 200)) && !s.contains(Ipv4Addr::new(10, 9, 1, 1)));
        assert_eq!(s.pick_after(&HashSet::new(), 1), Some((Ipv4Addr::new(10, 9, 0, 2), 2)));
        assert_eq!(s.pick_after(&HashSet::new(), 253), Some((Ipv4Addr::new(10, 9, 0, 254), 254)));
        assert_eq!(s.pick_after(&HashSet::new(), 254), Some((Ipv4Addr::new(10, 9, 0, 2), 2)), "wraps past .255");
        let all: HashSet<Ipv4Addr> = (2u32..255).map(|n| s.addr(n)).collect();
        assert_eq!(s.pick_after(&all, 7), None);
        // the default pool behaves as it always did
        let mut used = HashSet::new();
        let d = DEFAULT_SUBNET;
        assert_eq!(d.pick_after(&used, 1), Some((Ipv4Addr::new(172, 30, 0, 2), 2)));
        used.insert(Ipv4Addr::new(172, 30, 0, 3));
        assert_eq!(d.pick_after(&used, 2).unwrap().1, 4, "skips addresses in use");
        assert_eq!(d.pick_after(&used, 254), Some((Ipv4Addr::new(172, 30, 1, 1), 257)));
        assert_eq!(d.pick_after(&HashSet::new(), 65534), Some((Ipv4Addr::new(172, 30, 0, 2), 2)));
        assert_eq!(ip6_of(Ipv4Addr::new(172, 30, 0, 5)).to_string(), "fd00:5a57::ac1e:5");
    }
}
