//! Container groups: several images started together on one user-defined network (like a compose project).
//! A container still holds exactly one image; the group is the network plus a remembered list of members.
use crate::{
    catalog,
    spec::{Restart, RunSpec, valid_name},
    worker::args,
};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    /// container names
    pub members: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default)]
    groups: Vec<Group>,
}

/// `%LOCALAPPDATA%\sasta-docker\groups.json` (or `~/.local/share/sasta-docker/groups.json`).
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))?;
    Some(base.join("sasta-docker").join("groups.json"))
}

/// A missing or corrupt file is simply "no groups".
pub fn load(path: &Path) -> Vec<Group> {
    fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<File>(&t).ok()).map(|f| f.groups).unwrap_or_default()
}

pub fn save(path: &Path, groups: &[Group]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&File { groups: groups.to_vec() }).map_err(io::Error::other)?;
    fs::write(path, text)
}

/// Add members to a group (creating it), without duplicates.
pub fn upsert(groups: &mut Vec<Group>, name: &str, members: &[String]) {
    let i = groups.iter().position(|g| g.name == name).unwrap_or_else(|| {
        groups.push(Group { name: name.into(), members: vec![] });
        groups.len() - 1
    });
    for m in members {
        if !groups[i].members.contains(m) {
            groups[i].members.push(m.clone());
        }
    }
}

/// Drop members whose container no longer exists, and groups left empty. True if anything changed.
pub fn prune(groups: &mut Vec<Group>, existing: &[&str]) -> bool {
    let before: Vec<Group> = groups.clone();
    for g in groups.iter_mut() {
        g.members.retain(|m| existing.contains(&m.as_str()));
    }
    groups.retain(|g| !g.members.is_empty());
    *groups != before
}

/// Default group name for a first image: `postgres:alpine` -> `postgres`.
pub fn default_group_name(first_image: &str) -> String {
    let n: String = catalog::short_name(first_image).to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || "_.-".contains(c) { c } else { '-' }).collect();
    if n.is_empty() { "group".into() } else { n }
}

pub fn member_name(group: &str, image: &str) -> String {
    format!("{group}-{}", catalog::short_name(image))
}

/// Per-image fields of the group dialog.
#[derive(Clone, Debug)]
pub struct MemberSpec {
    pub image: String,
    pub name: String,
    pub ports: String,
    pub env: String,
    pub mem_mb: String,
    pub restart: Restart,
    /// shown under the env box (empty if the image needs nothing)
    pub hint: String,
}

impl MemberSpec {
    /// Defaults from the catalog: required env pre-filled, typical port mapped 1:1 for the first image port only.
    pub fn for_image(group: &str, image: &str) -> MemberSpec {
        let entry = catalog::lookup(image);
        let env = entry.map(|e| e.env.join("\n")).unwrap_or_default();
        let hint = if env.is_empty() { String::new() } else { format!("Required by this image: {}. Change the values below.", entry.map(|e| e.env.iter().map(|v| v.split('=').next().unwrap_or("")).collect::<Vec<_>>().join(", ")).unwrap_or_default()) };
        MemberSpec { image: image.into(), name: member_name(group, image), ports: String::new(), env, mem_mb: String::new(), restart: Restart::No, hint }
    }

    pub fn run_spec(&self, group: &str) -> RunSpec {
        RunSpec {
            image: self.image.clone(),
            name: self.name.clone(),
            ports: self.ports.clone(),
            env: self.env.clone(),
            mem_mb: self.mem_mb.clone(),
            restart: self.restart,
            network: group.into(),
            alias: catalog::short_name(&self.image),
            ..Default::default()
        }
    }

    pub fn args(&self, group: &str) -> Result<Vec<String>, String> {
        self.run_spec(group).args()
    }
}

pub fn validate_group(name: &str) -> Result<(), String> {
    if valid_name(name.trim()) { Ok(()) } else { Err("Group name may only use letters, digits, _ . - and must start with a letter or digit".into()) }
}

/// Validate every member and build all CLI calls: `[network create, run...]`. Names must be unique.
pub fn plan(group: &str, members: &[MemberSpec]) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let group = group.trim();
    validate_group(group)?;
    let mut runs = vec![];
    for (i, m) in members.iter().enumerate() {
        if m.name.trim().is_empty() {
            return Err(format!("{}: container name is required", m.image));
        }
        if members[..i].iter().any(|o| o.name.trim() == m.name.trim()) {
            return Err(format!("Two containers are named {:?}", m.name.trim()));
        }
        runs.push(m.args(group).map_err(|e| format!("{}: {e}", m.image))?);
    }
    Ok((args(&["network", "create", group]), runs))
}

/// `network create` reporting "already exists" is fine.
pub fn network_error_is_benign(e: &str) -> bool {
    e.to_lowercase().contains("exist")
}

pub fn start_steps(g: &Group) -> Vec<Vec<String>> {
    g.members.iter().map(|m| args(&["start", m])).collect()
}

pub fn stop_steps(g: &Group) -> Vec<Vec<String>> {
    g.members.iter().map(|m| args(&["stop", m])).collect()
}

/// Stop+remove every container, then delete the network.
pub fn remove_steps(g: &Group) -> Vec<Vec<String>> {
    let mut v: Vec<Vec<String>> = g.members.iter().map(|m| args(&["rm", "-f", m])).collect();
    v.push(args(&["network", "rm", &g.name]));
    v
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Pending,
    Running,
    Ok,
    Failed(String),
}

pub struct RunItem {
    pub image: String,
    pub name: String,
    pub status: Status,
}

/// Progress of one "run group" job (what the progress window shows).
pub struct GroupRun {
    pub name: String,
    pub network: Status,
    pub items: Vec<RunItem>,
    pub done: bool,
    pub finished_at: Option<Instant>,
}

impl GroupRun {
    pub fn new(name: &str, members: &[MemberSpec]) -> GroupRun {
        GroupRun { name: name.into(), network: Status::Running, items: members.iter().map(|m| RunItem { image: m.image.clone(), name: m.name.clone(), status: Status::Pending }).collect(), done: false, finished_at: None }
    }

    /// `idx` None = the network step.
    pub fn apply(&mut self, idx: Option<usize>, result: Result<String, String>) {
        let status = match result {
            Ok(_) => Status::Ok,
            Err(e) => Status::Failed(e),
        };
        match idx {
            None => {
                self.network = status;
                if let Some(i) = self.items.first_mut() {
                    i.status = Status::Running;
                }
            }
            Some(i) => {
                if let Some(it) = self.items.get_mut(i) {
                    it.status = status;
                }
                if let Some(next) = self.items.get_mut(i + 1) {
                    next.status = Status::Running;
                }
            }
        }
    }

    pub fn finish(&mut self) {
        self.done = true;
        self.finished_at = Some(Instant::now());
    }

    pub fn succeeded(&self) -> Vec<String> {
        self.items.iter().filter(|i| i.status == Status::Ok).map(|i| i.name.clone()).collect()
    }

    pub fn failed(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.status, Status::Failed(_))).count()
    }
}

/// A freshly created group is not pruned until its containers have had time to show up in `ps`.
pub const PRUNE_GRACE: Duration = Duration::from_secs(8);

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sasta-gui-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn file_roundtrip_missing_and_corrupt() {
        let dir = tmp("groups");
        let p = dir.join("sub").join("groups.json");
        assert!(load(&p).is_empty()); // missing
        let g = vec![Group { name: "shop".into(), members: vec!["shop-redis".into()] }];
        save(&p, &g).unwrap(); // creates the directory
        assert_eq!(load(&p), g);
        fs::write(&p, "{ not json").unwrap();
        assert!(load(&p).is_empty()); // corrupt
        fs::write(&p, "{}").unwrap();
        assert!(load(&p).is_empty()); // wrong shape
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_prune_lookup() {
        let mut g = vec![];
        upsert(&mut g, "a", &["a-x".into(), "a-y".into()]);
        upsert(&mut g, "a", &["a-y".into(), "a-z".into()]);
        upsert(&mut g, "b", &["b-x".into()]);
        assert_eq!(g[0].members, ["a-x", "a-y", "a-z"]);
        assert!(prune(&mut g, &["a-y", "a-z"]));
        assert_eq!((g.len(), g[0].members.len()), (1, 2));
        assert!(!prune(&mut g, &["a-y", "a-z"]));
    }

    #[test]
    fn naming() {
        assert_eq!(default_group_name("docker.io/library/Postgres:alpine"), "postgres");
        assert_eq!(default_group_name(""), "group");
        assert_eq!(member_name("shop", "postgres:alpine"), "shop-postgres");
        assert_eq!(member_name("shop", "grafana/grafana:latest"), "shop-grafana");
    }

    #[test]
    fn defaults_prefill_required_env() {
        let m = MemberSpec::for_image("shop", "postgres:alpine");
        assert_eq!(m.env, "POSTGRES_PASSWORD=postgres");
        assert!(m.hint.contains("POSTGRES_PASSWORD"));
        assert!(MemberSpec::for_image("g", "mariadb:lts").env.contains("MARIADB_ROOT_PASSWORD"));
        assert!(MemberSpec::for_image("g", "mysql").env.contains("MYSQL_ROOT_PASSWORD"));
        let r = MemberSpec::for_image("g", "redis:alpine");
        assert!(r.env.is_empty() && r.hint.is_empty());
    }

    #[test]
    fn member_args_join_the_network() {
        let mut m = MemberSpec::for_image("shop", "postgres:alpine");
        m.ports = "5432:5432".into();
        m.mem_mb = "256".into();
        m.restart = Restart::Always;
        assert_eq!(
            m.args("shop").unwrap().join(" "),
            "run -d --name shop-postgres --restart always --network shop --network-alias postgres -p 5432:5432 -e POSTGRES_PASSWORD=postgres --mem 256 postgres:alpine"
        );
    }

    #[test]
    fn plan_validates() {
        let a = MemberSpec::for_image("g", "redis");
        let b = MemberSpec::for_image("g", "alpine");
        let (net, runs) = plan("g", &[a.clone(), b.clone()]).unwrap();
        assert_eq!(net, ["network", "create", "g"]);
        assert_eq!(runs.len(), 2);
        assert!(plan("bad name", std::slice::from_ref(&a)).is_err());
        assert!(plan("g", &[a.clone(), a.clone()]).is_err()); // duplicate container name
        let mut bad = b;
        bad.ports = "oops".into();
        assert!(plan("g", &[a, bad]).unwrap_err().starts_with("alpine"));
    }

    #[test]
    fn group_steps() {
        let g = Group { name: "g".into(), members: vec!["g-a".into(), "g-b".into()] };
        assert_eq!(start_steps(&g)[1], ["start", "g-b"]);
        assert_eq!(stop_steps(&g)[0], ["stop", "g-a"]);
        let r = remove_steps(&g);
        assert_eq!(r[0], ["rm", "-f", "g-a"]);
        assert_eq!(r.last().unwrap(), &["network", "rm", "g"]);
        assert!(network_error_is_benign("network g already exists"));
        assert!(!network_error_is_benign("engine down"));
    }

    #[test]
    fn run_progress_one_failure_does_not_stop_the_rest() {
        let ms = [MemberSpec::for_image("g", "redis"), MemberSpec::for_image("g", "alpine"), MemberSpec::for_image("g", "nginx")];
        let mut r = GroupRun::new("g", &ms);
        r.apply(None, Ok(String::new()));
        assert_eq!(r.items[0].status, Status::Running);
        r.apply(Some(0), Err("boom".into()));
        assert_eq!(r.items[1].status, Status::Running);
        r.apply(Some(1), Ok("id".into()));
        r.apply(Some(2), Ok("id".into()));
        r.finish();
        assert!(r.done);
        assert_eq!(r.succeeded(), ["g-alpine", "g-nginx"]);
        assert_eq!(r.failed(), 1);
    }
}
