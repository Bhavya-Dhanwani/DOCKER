//! Shared UI state: what the tabs read, and the commands they emit back to the app.
use crate::parse::{Container, Df, Image, Network, State, Volume};
use std::{collections::{HashMap, HashSet}, time::{Duration, Instant}};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tab {
    Containers,
    Images,
    Volumes,
    Networks,
}

/// Something that can be refreshed from the CLI.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Containers,
    Images,
    Volumes,
    Networks,
    Df,
}

#[derive(Default)]
pub struct Data {
    pub containers: Vec<Container>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    pub networks: Vec<Network>,
    pub df: Option<Df>,
    /// `docker ps` does not report "paused", so the GUI remembers what it paused.
    pub paused: HashSet<String>,
    /// last error per kind of data (shown as a banner instead of a toast so polling does not spam)
    pub errors: HashMap<Kind, String>,
    pub loaded: HashSet<Kind>,
}

/// A mutating CLI call (one or more invocations) with a human label.
#[derive(Clone, Debug)]
pub struct Task {
    pub label: String,
    pub steps: Vec<Vec<String>>,
}

impl Task {
    pub fn new(label: impl Into<String>, args: &[&str]) -> Task {
        Task { label: label.into(), steps: vec![crate::worker::args(args)] }
    }
}

/// What a tab asks the app to do.
pub enum Cmd {
    Task(Task),
    Confirm(String, Task),
    Logs { id: String, name: String },
    Stats { id: String, name: String },
    Pull(String),
    RunDialog(String),
    BuildDialog,
    Copy(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Ok,
    Err,
}

pub struct Toast {
    pub text: String,
    pub level: Level,
    pub at: Instant,
}

pub const TOAST_TTL: Duration = Duration::from_secs(6);

pub fn prune_toasts(toasts: &mut Vec<Toast>, now: Instant) {
    toasts.retain(|t| now.duration_since(t.at) < TOAST_TTL);
}

/// The state to display: a running container that the GUI paused shows as paused.
pub fn effective_state(c: &Container, paused: &HashSet<String>) -> State {
    if c.state == State::Running && paused.contains(&c.id) { State::Paused } else { c.state }
}

impl Data {
    pub fn is_loaded(&self, k: Kind) -> bool {
        self.loaded.contains(&k)
    }

    /// Drop pause marks for containers that are gone or no longer running.
    pub fn sync_paused(&mut self) {
        let live: HashSet<&String> = self.containers.iter().filter(|c| c.state == State::Running).map(|c| &c.id).collect();
        self.paused.retain(|id| live.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, state: State) -> Container {
        Container { id: id.into(), name: String::new(), image: String::new(), command: String::new(), state, health: None, uptime: None, exit_code: None, restarts: None, ip: String::new(), ports: String::new() }
    }

    #[test]
    fn paused_overlay() {
        let mut d = Data {
            containers: vec![c("a", State::Running), c("b", State::Exited)],
            paused: ["a".to_string(), "b".to_string(), "gone".to_string()].into(),
            ..Default::default()
        };
        assert_eq!(effective_state(&d.containers[0], &d.paused), State::Paused);
        assert_eq!(effective_state(&d.containers[1], &d.paused), State::Exited);
        d.sync_paused();
        assert_eq!(d.paused.len(), 1);
        assert!(d.paused.contains("a"));
    }

    #[test]
    fn toasts_expire() {
        let now = Instant::now();
        let mut t = vec![Toast { text: "x".into(), level: Level::Ok, at: now }, Toast { text: "y".into(), level: Level::Err, at: now - Duration::from_secs(60) }];
        prune_toasts(&mut t, now + Duration::from_secs(1));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].text, "x");
    }
}
