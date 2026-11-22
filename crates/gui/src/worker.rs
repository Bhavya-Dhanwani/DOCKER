//! Background execution: every call to the `docker` CLI runs on a thread and reports back through a channel.
use crate::parse::{Container, Df, Image, Network, StatRow, Volume};
use eframe::egui;
use std::{
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::Duration,
};

pub type Res<T> = Result<T, String>;

/// Everything a background thread can tell the UI.
pub enum Event {
    Containers(Res<Vec<Container>>),
    Images(Res<Vec<Image>>),
    Volumes(Res<Vec<Volume>>),
    Networks(Res<Vec<Network>>),
    Df(Res<Df>),
    Stats(String, Res<StatRow>),
    Version(Res<String>),
    Usage(Res<Vec<crate::usage::Sample>>),
    /// progress of a group run; idx None = the network step
    GroupStep { group: String, idx: Option<usize>, result: Res<String> },
    GroupDone(String),
    Done { label: String, steps: Vec<Vec<String>>, result: Res<String> },
    Line(u64, String),
    End(u64, bool),
    Picked(String),
}

/// The `docker` CLI that sits next to this executable (never whatever is first on PATH).
pub fn cli_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.with_file_name(if cfg!(windows) { "docker.exe" } else { "docker" })
}

fn command(args: &[String]) -> Command {
    let mut c = Command::new(cli_path());
    c.args(args).stdin(Stdio::null());
    hide_window(&mut c);
    c
}

fn hide_window(_c: &mut Command) {
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(_c, 0x0800_0000); // CREATE_NO_WINDOW
}

pub fn clean_error(stderr: &str) -> String {
    stderr.trim().trim_start_matches("docker: ").to_string()
}

/// Run the CLI to completion: Ok(stdout) or Err(message).
pub fn run_cli(args: &[String]) -> Res<String> {
    let out = command(args).output().map_err(|e| format!("cannot run {}: {e}", cli_path().display()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = clean_error(&String::from_utf8_lossy(&out.stderr));
        Err(if err.is_empty() { clean_error(&String::from_utf8_lossy(&out.stdout)) } else { err })
    }
}

pub fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Run every step even if one fails; Err collects all failures.
pub fn run_steps(steps: &[Vec<String>]) -> Res<String> {
    let (mut last, mut errors) = (String::new(), vec![]);
    for s in steps {
        match run_cli(s) {
            Ok(o) => last = o,
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() { Ok(last) } else { Err(errors.join("; ")) }
}

#[derive(Clone)]
pub struct Worker {
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl Worker {
    pub fn new(ctx: egui::Context) -> (Worker, Receiver<Event>) {
        let (tx, rx) = channel();
        (Worker { tx, ctx }, rx)
    }

    fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }

    /// Create the group network (an existing one is fine), then start every member in order,
    /// reporting each step. A failing member never stops the others.
    pub fn run_group(&self, group: String, network: Vec<String>, runs: Vec<Vec<String>>) {
        let w = self.clone();
        thread::spawn(move || {
            let net = match run_cli(&network) {
                Err(e) if crate::groups::network_error_is_benign(&e) => Ok(String::new()),
                r => r,
            };
            w.send(Event::GroupStep { group: group.clone(), idx: None, result: net });
            for (i, a) in runs.iter().enumerate() {
                w.send(Event::GroupStep { group: group.clone(), idx: Some(i), result: run_cli(a) });
            }
            w.send(Event::GroupDone(group));
        });
    }

    /// Run `f` on a thread and deliver its event.
    pub fn spawn(&self, f: impl FnOnce() -> Event + Send + 'static) {
        let w = self.clone();
        thread::spawn(move || {
            let e = f();
            w.send(e);
        });
    }

    /// Stream the CLI's stdout+stderr line by line as `Event::Line(id, ..)`, then `Event::End`.
    /// Setting the returned flag kills the process.
    pub fn stream(&self, id: u64, args: Vec<String>) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (w, flag) = (self.clone(), cancel.clone());
        thread::spawn(move || {
            let ok = match command(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
                Err(e) => {
                    w.send(Event::Line(id, format!("cannot run {}: {e}", cli_path().display())));
                    false
                }
                Ok(mut child) => {
                    let readers = [child.stdout.take().map(|r| pump(&w, id, r)), child.stderr.take().map(|r| pump(&w, id, r))];
                    let status = loop {
                        if flag.load(Ordering::Relaxed) {
                            let _ = child.kill();
                            break child.wait();
                        }
                        match child.try_wait() {
                            Ok(Some(s)) => break Ok(s),
                            Ok(None) => thread::sleep(Duration::from_millis(80)),
                            Err(e) => break Err(e),
                        }
                    };
                    for r in readers.into_iter().flatten() {
                        let _ = r.join();
                    }
                    status.is_ok_and(|s| s.success())
                }
            };
            w.send(Event::End(id, ok));
        });
        cancel
    }

    /// Native folder picker (PowerShell's FolderBrowserDialog; there is no extra GUI dependency).
    pub fn pick_folder(&self) {
        if !cfg!(windows) {
            return;
        }
        self.spawn(|| {
            let mut c = Command::new("powershell");
            c.args([
                "-NoProfile",
                "-STA",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog; if ($d.ShowDialog() -eq 'OK') { $d.SelectedPath }",
            ]);
            hide_window(&mut c);
            let out = c.stdin(Stdio::null()).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
            Event::Picked(out.unwrap_or_default())
        });
    }
}

fn pump(w: &Worker, id: u64, r: impl Read + Send + 'static) -> thread::JoinHandle<()> {
    let w = w.clone();
    thread::spawn(move || {
        for line in BufReader::new(r).split(b'\n').map_while(Result::ok) {
            let s = String::from_utf8_lossy(&line);
            // progress lines redraw with \r: keep the last frame
            w.send(Event::Line(id, s.rsplit('\r').find(|p| !p.is_empty()).unwrap_or("").to_string()));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_cleanup() {
        assert_eq!(clean_error("docker: no such container\n"), "no such container");
    }

    #[test]
    fn cli_sits_next_to_exe() {
        let p = cli_path();
        assert!(p.file_name().unwrap().to_string_lossy().starts_with("docker"));
    }
}
