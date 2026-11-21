//! User-defined networks. A network is a name; containers started with `--network <name>` can reach each
//! other by container name or `--network-alias`, through a generated /etc/hosts that every supervisor keeps
//! current. (Like Docker's default bridge, containers outside any named network get no name resolution.
//! Unlike Docker, networks share one bridge, so they provide discovery, not isolation.)
use crate::{
    cli::is_volume_name,
    error::{Error, Result},
    registry::{self, Entry},
};
use std::{fs, path::{Path, PathBuf}};

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
    create_in(&root(), name)
}

fn create_in(root: &Path, name: &str) -> Result<()> {
    check(name)?;
    let f = root.join(format!("{name}.json"));
    if f.exists() {
        return Err(Error::Usage(format!("network {name} already exists")));
    }
    fs::create_dir_all(root).map_err(|e| Error::io("create networks dir", e))?;
    fs::write(&f, format!("{{\"name\":{name:?},\"created_unix\":{}}}", registry::now_unix())).map_err(|e| Error::io("write network", e))
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
}

impl Host {
    pub fn of(e: &Entry) -> Option<Host> {
        let mut names: Vec<String> = e.name.iter().cloned().collect();
        names.push(e.id.chars().take(12).collect());
        names.extend(e.config.iter().flat_map(|c| c.aliases.iter().cloned()));
        Some(Host { ip: e.ip.clone()?, names })
    }
}

/// /etc/hosts for the container `own` (its own line also answers to `container`, the fixed hostname).
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
    let mut mine = vec!["container".to_string()];
    mine.extend(own.names.iter().cloned());
    s.push_str(&line(&own.ip, &mine));
    for o in others.iter().filter(|o| o.ip != own.ip) {
        s.push_str(&line(&o.ip, &o.names));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(ip: &str, names: &[&str]) -> Host {
        Host { ip: ip.into(), names: names.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn hosts_file_lists_self_and_peers() {
        let own = host("172.30.0.5", &["web", "abc123def456", "frontend"]);
        let h = hosts_file(&own, &[host("172.30.0.6", &["db", "ffff00001111"]), host("172.30.0.5", &["dup"])]);
        assert!(h.starts_with("127.0.0.1\tlocalhost\n::1\t"));
        assert!(h.contains("172.30.0.5\tcontainer web abc123def456 frontend\n"));
        assert!(h.contains("172.30.0.6\tdb ffff00001111\n"));
        assert!(!h.contains("dup"), "an entry with our own address is not repeated");
        assert_eq!(hosts_file(&host("172.30.0.2", &["x", "x"]), &[]).lines().last().unwrap(), "172.30.0.2\tcontainer x");
    }

    #[test]
    fn names_are_validated_and_networks_listed() {
        let root = std::env::temp_dir().join(format!("rd-net-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for bad in ["bridge", "host", "none", "../x", "", "a b"] {
            assert!(create_in(&root, bad).is_err(), "{bad}");
        }
        create_in(&root, "appnet").unwrap();
        create_in(&root, "backend").unwrap();
        assert!(create_in(&root, "appnet").is_err(), "no duplicates");
        assert_eq!(list_in(&root), ["appnet", "backend"]);
        let _ = fs::remove_dir_all(&root);
    }
}
