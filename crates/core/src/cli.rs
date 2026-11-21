use crate::error::{Error, Result};
use docker_image::Health;
use serde::{Deserialize, Serialize};

pub const USAGE: &str = "\
usage:
  docker run [flags] <image|dir> [command...]
  docker exec [-i] [-t] <container> <command...>
  docker ps [-a]
  docker logs [-f] <container>
  docker stop [-t <seconds>] <container>
  docker kill | pause | unpause | wait | top <container>
  docker update [--mem <MB>] [--cpu <PERCENT>] [--procs <N>] <container>   (0 removes a limit)
  docker start <container>        (re-runs it with its saved settings)
  docker restart [-t <seconds>] <container>
  docker rename <container> <new-name>
  docker rm [-f] <container>
  docker inspect <container>
  docker stats [--no-stream] [container...]
  docker cp <container>:<path> <host-path> | <host-path> <container>:<path>
  docker volume create|ls|rm|inspect|prune [name]
  docker network create|ls|rm|inspect [name]
  docker compose [-f <file>] [-p <project>] up [-d] [--build] | down [-v] | ps | logs [-f] [service] | config | build
  docker daemon [--host unix:///path | tcp://127.0.0.1:2375]     serve the Docker Engine API
  docker system df
  docker container prune     remove all exited containers
  docker build -t <name[:tag]> [-f <Dockerfile>] [--build-arg K=V] [--target <stage>] [--no-cache] <context>
  docker pull <image>
  docker push <image>
  docker save -o <file.tar> <image>...
  docker load -i <file.tar>
  docker tag <source-image> <target-image>
  docker version
  docker vm init [--distro <name>] [--use-existing] [--dry-run] | start | stop | status
  docker login [-u <user>] (-p <password> | --password-stdin) [server]
  docker logout [server]
  docker images
  docker rmi <image>
  docker prune          delete unused image layers and build leftovers
  docker swarm init|leave|status        single-node swarm: a background reconciler keeps services running
  docker service create [--name N] [--replicas R] [-p H:C] [-e K=V] [--network N] [--restart-condition any|on-failure|none]
                 [--mem MB] [--cpu P] [--update-delay S] [--update-parallelism N] <image> [cmd...]
  docker service ls|ps|inspect|logs|rm <name> | scale <name>=<N>... | update [--image I] [-e K=V] [--replicas R] <name>
  docker stack deploy -c <compose.yml> <name> | ls | ps | services | rm <name>

run flags:
  -d, --detach          run in the background, print the container id
  -i, --interactive     keep stdin open (stdin is always passed through; accepted for compatibility)
  -t, --tty             allocate a terminal (use `-it` for an interactive shell)
      --restart <p>     no | always | unless-stopped | on-failure[:max]
      --health-cmd <c>  run <c> inside the container to check its health
      --health-interval|--health-timeout|--health-start-period <dur>  e.g. 10s, 1m (with --health-cmd)
      --health-retries <n>   failures in a row before it is unhealthy
      --no-healthcheck  ignore the image's HEALTHCHECK
      --name <name>     name the container (usable instead of the id)
      --entrypoint <x>  replace the image's ENTRYPOINT (and drop its default CMD)
  -w, --workdir <dir>   working directory inside the container
  -u, --user <u[:g]>    run as this user (name or uid, resolved inside the image)
  -e, --env K=V         set an environment variable (or `-e K` to pass the host's value)
  -v, --volume H:C[:ro] bind-mount host path H (or a named volume H) at C inside the container
      --network <mode>  bridge (default: own IP, NAT to the internet), host, none, or the name of a
                        network made with `docker network create` (containers find each other by name)
      --network-alias <a>  an extra DNS name for the container on its network
  -p, --publish H:C     publish container TCP port C on host port H (bridge mode)
      --cap-add <CAP>   add a Linux capability (e.g. NET_ADMIN, or ALL)
      --cap-drop <CAP>  drop a capability (ALL drops everything)
      --security-opt <o>  seccomp=unconfined | no-new-privileges
      --privileged      all capabilities, no seccomp (dangerous)
      --mem <MB>        hard memory cap (Linux: cgroup v2, swap off; Windows: Job Object)
      --cpu <PERCENT>   hard CPU cap, 1-100 of the whole machine
      --procs <N>       max processes in the container (fork-bomb guard)
      --rw              with a <dir> root: make it writable (default read-only)

<image> is a pulled image (alpine, ubuntu:24.04, ghcr.io/org/app:tag).
<dir>   is a root filesystem directory (Linux) or a working directory (Windows).
<container> is an id (a unique prefix works) or a --name.

Windows/macOS: Linux containers run in a hidden Linux VM (`docker vm init` once). On Windows,
`docker --native run [flags] <dir> <cmd>` instead runs a Windows process tree under a Job Object.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub host: String,
    pub container: String,
    pub ro: bool,
}

/// `C:\dir` or `C:/dir`
fn is_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// `web:/etc/hosts` -> ("web", "/etc/hosts"). Windows drive paths (`C:\x`) and plain paths are not container refs.
pub fn split_container_ref(s: &str) -> Option<(&str, &str)> {
    if is_drive_path(s) {
        return None;
    }
    let (name, path) = s.split_once(':')?;
    is_volume_name(name).then_some((name, path))
}

/// A named volume: letters, digits, `_ . -`, starting with a letter or digit.
pub fn is_volume_name(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

impl Volume {
    /// Is the host side a volume name (as opposed to a path)?
    pub fn is_named(&self) -> bool {
        is_volume_name(&self.host)
    }

    /// `host:container` or `host:container:ro|rw`. Host must be a path (starts with `/` or `.`).
    pub fn parse(s: &str) -> Result<Volume> {
        let bad = |m: &str| Error::Usage(format!("bad volume {s:?}: {m}"));
        // a Windows drive letter ("C:\\data") must not be split at its own colon
        let skip = if is_drive_path(s) { 2 } else { 0 };
        let parts: Vec<&str> = match s[skip..].find(':') {
            Some(i) => std::iter::once(&s[..skip + i]).chain(s[skip + i + 1..].split(':')).collect(),
            None => vec![s],
        };
        let (host, container, mode) = match parts[..] {
            [h, c] => (h, c, "rw"),
            [h, c, m] => (h, c, m),
            _ => return Err(bad("expected host:container[:ro]")),
        };
        if !(host.starts_with('/') || host.starts_with('.') || is_drive_path(host) || is_volume_name(host)) {
            return Err(bad("host must be a path (starting with / or .) or a volume name"));
        }
        if !container.starts_with('/') || container.split('/').any(|c| c == "..") {
            return Err(bad("container path must be absolute and contain no .."));
        }
        let ro = match mode {
            "ro" => true,
            "rw" => false,
            _ => return Err(bad("mode must be ro or rw")),
        };
        Ok(Volume { host: host.into(), container: container.into(), ro })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Network {
    Bridge,
    Host,
    None,
}

/// `docker compose ...`
#[derive(Debug, PartialEq, Eq)]
pub struct ComposeArgs {
    pub file: Option<String>,
    pub project: Option<String>,
    pub sub: ComposeSub,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ComposeSub {
    Up { detach: bool, build: bool },
    Down { volumes: bool },
    Ps,
    Logs { follow: bool, service: Option<String> },
    Config,
    Build,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NetCmd {
    Create(String),
    Ls,
    Rm(String),
    Inspect(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum VolumeCmd {
    Create(String),
    Ls,
    Rm(String),
    Inspect(String),
    Prune,
}

/// `--restart`: what the supervisor does when the container exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Restart {
    #[default]
    No,
    Always,
    UnlessStopped,
    /// `on-failure[:max]`: restart after a non-zero exit, at most `max` times (0 = unlimited).
    OnFailure(u32),
}

impl Restart {
    pub fn parse(s: &str) -> Result<Restart> {
        Ok(match s {
            "no" => Restart::No,
            "always" => Restart::Always,
            "unless-stopped" => Restart::UnlessStopped,
            "on-failure" => Restart::OnFailure(0),
            other => match other.strip_prefix("on-failure:").and_then(|n| n.parse().ok()) {
                Some(n) => Restart::OnFailure(n),
                None => return Err(usage("--restart must be no, always, unless-stopped or on-failure[:max]")),
            },
        })
    }

    pub fn to_arg(self) -> String {
        match self {
            Restart::No => "no".into(),
            Restart::Always => "always".into(),
            Restart::UnlessStopped => "unless-stopped".into(),
            Restart::OnFailure(0) => "on-failure".into(),
            Restart::OnFailure(n) => format!("on-failure:{n}"),
        }
    }

    /// Should the supervisor start the container again after it exited with `code`?
    /// `restarts` is how many times it was already restarted; `stopped` means the user ran `docker stop`/`kill`.
    pub fn should_restart(self, code: i32, restarts: u32, stopped: bool) -> bool {
        if stopped {
            return false;
        }
        match self {
            Restart::No => false,
            Restart::Always | Restart::UnlessStopped => true,
            Restart::OnFailure(max) => code != 0 && (max == 0 || restarts < max),
        }
    }
}

/// `docker vm ...`: manage the hidden Linux VM (Windows/macOS).
#[derive(Debug, PartialEq, Eq)]
pub enum VmCommand {
    Init { distro: Option<String>, use_existing: bool, dry_run: bool },
    Start,
    Stop,
    Status,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Password {
    Arg(String),
    Stdin,
}

/// `docker build` arguments.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct BuildArgs {
    pub tag: String,
    pub file: Option<String>,
    pub args: Vec<(String, String)>,
    pub no_cache: bool,
    /// Stop after this build stage (`--target`).
    pub target: Option<String>,
    /// `--label k=v`: added to the image config.
    pub labels: Vec<(String, String)>,
    /// `--secret id=<id>,src=<file>`: (id, file) pairs for `RUN --mount=type=secret`.
    pub secrets: Vec<(String, String)>,
    /// `--platform os/arch`: only the host platform can be built.
    pub platform: Option<String>,
    /// `-q`: print only the image id.
    pub quiet: bool,
    pub context: String,
}

/// `-p host:container` (TCP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    pub host: u16,
    pub container: u16,
}

impl Port {
    pub fn parse(s: &str) -> Result<Port> {
        let bad = |m: &str| Error::Usage(format!("bad port mapping {s:?}: {m}"));
        let body = match s.split_once('/') {
            Some((b, "tcp")) => b,
            Some(_) => return Err(bad("only TCP is supported yet")),
            None => s,
        };
        let (h, c) = body.split_once(':').ok_or_else(|| bad("expected host:container"))?;
        let num = |x: &str| x.parse::<u16>().ok().filter(|&n| n > 0).ok_or_else(|| bad("ports are 1-65535"));
        Ok(Port { host: num(h)?, container: num(c)? })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Config {
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub security_opt: Vec<String>,
    pub privileged: bool,
    /// Internal: a build step. Not recorded in the container registry.
    pub ephemeral: bool,
    /// None = the platform default (bridge on Linux).
    pub network: Option<Network>,
    /// A user-defined network (implies the bridge).
    pub network_name: Option<String>,
    pub aliases: Vec<String>,
    pub ports: Vec<Port>,
    pub mem_bytes: Option<u64>,
    pub cpu_percent: Option<u32>,
    pub procs: Option<u32>,
    pub rw: bool,
    pub detach: bool,
    /// `--rm`: delete the container (and its logs) as soon as it exits.
    pub auto_remove: bool,
    /// Internal: reuse this container id (`docker start`, API `start`) instead of making a new one.
    pub preset_id: Option<String>,
    pub tty: bool,
    pub interactive: bool,
    pub entrypoint: Option<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub restart: Restart,
    pub health: Option<Health>,
    pub no_healthcheck: bool,
    pub name: Option<String>,
    pub env: Vec<String>,
    pub volumes: Vec<Volume>,
    /// An image reference or a directory.
    pub target: String,
    /// Empty means "use the image's default command".
    pub cmd: Vec<String>,
}

// a `Config` is big but a `Command` lives for one function call, so boxing it would only add noise
#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Config),
    Exec { id: String, cmd: Vec<String>, tty: bool },
    Ps { all: bool },
    Logs { follow: bool, id: String },
    Stop { id: String, timeout: u64 },
    Restart { id: String, timeout: u64 },
    Start(String),
    Kill(String),
    Pause(String),
    /// `docker update`: change the limits of a running container (memory in MB; 0 removes a limit).
    Update { id: String, mem_mb: Option<u64>, cpu: Option<u32>, procs: Option<u32> },
    Unpause(String),
    Wait(String),
    Top(String),
    Rename { old: String, new: String },
    Stats { no_stream: bool, ids: Vec<String> },
    Cp { src: String, dst: String },
    Volume(VolumeCmd),
    Network(NetCmd),
    Compose(ComposeArgs),
    Daemon { host: Option<String> },
    SystemDf,
    ContainerPrune,
    Rm { force: bool, id: String },
    Inspect(String),
    Build(BuildArgs),
    Save { out: String, images: Vec<String> },
    Load { input: String },
    Version,
    Vm(VmCommand),
    Login { server: Option<String>, user: String, password: Password },
    Logout(Option<String>),
    Push(String),
    Tag { src: String, dst: String },
    Pull(String),
    Images,
    Rmi(String),
    Prune,
    /// `docker swarm|service|stack ...`: the raw words (group first), parsed by the CLI crate's swarm module.
    Swarm(Vec<String>),
}

fn usage(msg: &str) -> Error {
    Error::Usage(msg.to_string())
}

/// Leading flags from `allowed`, then exactly one positional argument.
fn flags_then_one(args: Vec<String>, allowed: &[&str], what: &str) -> Result<(Vec<String>, String)> {
    let mut it = args.into_iter().peekable();
    let mut flags = vec![];
    while let Some(f) = it.next_if(|a| a.starts_with('-')) {
        if !allowed.contains(&f.as_str()) {
            return Err(usage(&format!("unknown flag {f}")));
        }
        flags.push(f);
    }
    let v = it.next().ok_or_else(|| usage(&format!("missing <{what}>")))?;
    if it.next().is_some() {
        return Err(usage("too many arguments"));
    }
    Ok((flags, v))
}

fn only_flags(args: Vec<String>, allowed: &[&str]) -> Result<Vec<String>> {
    for a in &args {
        if !allowed.contains(&a.as_str()) {
            return Err(usage(&format!("unexpected argument {a}")));
        }
    }
    Ok(args)
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut it = args.into_iter();
    let name = it.next();
    let rest: Vec<String> = it.collect();
    let (name, rest) = resolve_alias(name, rest);
    match name.as_deref() {
        Some("run") => parse_run(rest).map(Command::Run),
        Some("exec") => {
            let mut it = expand_short_flags(rest, &[]).into_iter().peekable();
            let mut tty = false;
            while let Some(f) = it.next_if(|a| a.starts_with('-')) {
                match f.as_str() {
                    "-t" | "--tty" => tty = true,
                    "-i" | "--interactive" => {}
                    other => return Err(usage(&format!("unknown flag {other}"))),
                }
            }
            let id = it.next().ok_or_else(|| usage("missing <container>"))?;
            let cmd: Vec<String> = it.collect();
            if cmd.is_empty() {
                return Err(usage("missing <command>"));
            }
            Ok(Command::Exec { id, cmd, tty })
        }
        Some("ps") => Ok(Command::Ps { all: !only_flags(rest, &["-a", "--all"])?.is_empty() }),
        Some("logs") => {
            let (f, id) = flags_then_one(rest, &["-f", "--follow"], "container")?;
            Ok(Command::Logs { follow: !f.is_empty(), id })
        }
        Some("rm") => {
            let (f, id) = flags_then_one(rest, &["-f", "--force"], "container")?;
            Ok(Command::Rm { force: !f.is_empty(), id })
        }
        Some("stop") => timeout_and_id(rest).map(|(timeout, id)| Command::Stop { id, timeout }),
        Some("restart") => timeout_and_id(rest).map(|(timeout, id)| Command::Restart { id, timeout }),
        Some("start") => flags_then_one(rest, &[], "container").map(|(_, v)| Command::Start(v)),
        Some("kill") => flags_then_one(rest, &[], "container").map(|(_, v)| Command::Kill(v)),
        Some("update") => parse_update(rest),
        Some("pause") => flags_then_one(rest, &[], "container").map(|(_, v)| Command::Pause(v)),
        Some("unpause") => flags_then_one(rest, &[], "container").map(|(_, v)| Command::Unpause(v)),
        Some("wait") => flags_then_one(rest, &[], "container").map(|(_, v)| Command::Wait(v)),
        Some("top") => flags_then_one(rest, &[], "container").map(|(_, v)| Command::Top(v)),
        Some("rename") => match <[String; 2]>::try_from(rest) {
            Ok([old, new]) => Ok(Command::Rename { old, new }),
            Err(_) => Err(usage("usage: docker rename <container> <new-name>")),
        },
        Some("cp") => match <[String; 2]>::try_from(rest) {
            Ok([src, dst]) => Ok(Command::Cp { src, dst }),
            Err(_) => Err(usage("usage: docker cp <container>:<path> <host-path>  |  <host-path> <container>:<path>")),
        },
        Some("stats") => {
            let (mut no_stream, mut ids) = (false, vec![]);
            for a in rest {
                match a.as_str() {
                    "--no-stream" => no_stream = true,
                    f if f.starts_with('-') => return Err(usage(&format!("unknown flag {f}"))),
                    _ => ids.push(a),
                }
            }
            Ok(Command::Stats { no_stream, ids })
        }
        Some("volume") => parse_volume(rest).map(Command::Volume),
        Some("network") => parse_network(rest).map(Command::Network),
        Some("compose") => parse_compose(rest).map(Command::Compose),
        Some("daemon") => match rest.as_slice() {
            [] => Ok(Command::Daemon { host: None }),
            [f, h] if f == "--host" || f == "-H" => Ok(Command::Daemon { host: Some(h.clone()) }),
            _ => Err(usage("usage: docker daemon [--host unix:///path | tcp://host:port]")),
        },
        Some("system-df") => only_flags(rest, &[]).map(|_| Command::SystemDf),
        Some("container-prune") => only_flags(rest, &[]).map(|_| Command::ContainerPrune),
        Some("inspect") => flags_then_one(rest, &[], "container").map(|(_, v)| Command::Inspect(v)),
        Some("version") => only_flags(rest, &[]).map(|_| Command::Version),
        Some("vm") => parse_vm(rest).map(Command::Vm),
        Some("login") => parse_login(rest),
        Some("logout") => {
            let mut it = rest.into_iter();
            let server = it.next();
            if it.next().is_some() {
                return Err(usage("too many arguments"));
            }
            Ok(Command::Logout(server))
        }
        Some("push") => flags_then_one(rest, &[], "image").map(|(_, v)| Command::Push(v)),
        Some("tag") => {
            let mut it = rest.into_iter();
            match (it.next(), it.next(), it.next()) {
                (Some(src), Some(dst), None) => Ok(Command::Tag { src, dst }),
                _ => Err(usage("usage: docker tag <source-image> <target-image>")),
            }
        }
        Some("save") => parse_save(rest),
        Some("load") => match <[String; 2]>::try_from(rest) {
            Ok([f, input]) if f == "-i" || f == "--input" => Ok(Command::Load { input }),
            _ => Err(usage("usage: docker load -i <file.tar>")),
        },
        Some("build") => parse_build(rest).map(Command::Build),
        Some("prune") => only_flags(rest, &[]).map(|_| Command::Prune),
        Some("pull") => flags_then_one(rest, &[], "image").map(|(_, v)| Command::Pull(v)),
        Some("rmi") => flags_then_one(rest, &[], "image").map(|(_, v)| Command::Rmi(v)),
        Some("images") => only_flags(rest, &[]).map(|_| Command::Images),
        Some(g @ ("swarm" | "service" | "stack")) => Ok(Command::Swarm([vec![g.to_string()], rest].concat())),
        _ => Err(usage("expected a command")),
    }
}

/// `docker container ls`, `docker image rm`, `docker system df` ... are the same commands under their group name.
fn resolve_alias(name: Option<String>, mut rest: Vec<String>) -> (Option<String>, Vec<String>) {
    let group = name.clone().unwrap_or_default();
    if group == "buildx" && rest.first().is_some_and(|s| s == "build") {
        // `docker buildx build` is our one builder; its output flags mean nothing here
        let flags = ["--load", "--push-none"];
        let mut it = rest.into_iter().skip(1);
        let mut kept = vec![];
        while let Some(a) = it.next() {
            match a.as_str() {
                f if flags.contains(&f) => {}
                "--progress" | "--platform" => {
                    it.next();
                }
                f if f.starts_with("--progress=") || f.starts_with("--platform=") => {}
                _ => kept.push(a),
            }
        }
        return (Some("build".into()), kept);
    }
    if !matches!(group.as_str(), "container" | "image" | "system") || rest.is_empty() {
        return (name, rest);
    }
    let sub = rest.remove(0);
    let mapped = match (group.as_str(), sub.as_str()) {
        ("container", "ls" | "list") => "ps",
        ("container", "prune") => "container-prune",
        ("image", "ls" | "list") => "images",
        ("image", "rm") => "rmi",
        ("system", "df") => "system-df",
        ("system", "prune") => "prune",
        (_, other) => other,
    };
    (Some(mapped.to_string()), rest)
}

fn parse_update(args: Vec<String>) -> Result<Command> {
    let mut it = args.into_iter().peekable();
    let (mut mem_mb, mut cpu, mut procs) = (None, None, None);
    while let Some(f) = it.next_if(|a| a.starts_with('-')) {
        let mut num = |what: &str| it.next().and_then(|v| v.parse::<u64>().ok()).ok_or_else(|| usage(&format!("{f} needs {what}")));
        match f.as_str() {
            "--mem" | "--memory" | "-m" => mem_mb = Some(num("a number of MB")?),
            "--cpu" => cpu = Some(num("a percent")?.min(100) as u32),
            "--procs" | "--pids-limit" => procs = Some(num("a number")? as u32),
            other => return Err(usage(&format!("unknown flag {other}"))),
        }
    }
    let id = it.next().ok_or_else(|| usage("missing <container>"))?;
    if it.next().is_some() {
        return Err(usage("too many arguments"));
    }
    if mem_mb.is_none() && cpu.is_none() && procs.is_none() {
        return Err(usage("docker update needs at least one of --mem, --cpu, --procs"));
    }
    Ok(Command::Update { id, mem_mb, cpu, procs })
}

fn timeout_and_id(args: Vec<String>) -> Result<(u64, String)> {
    let mut it = args.into_iter().peekable();
    let mut timeout = 10;
    while let Some(f) = it.next_if(|a| a.starts_with('-')) {
        match f.as_str() {
            "-t" | "--time" => timeout = it.next().and_then(|v| v.parse().ok()).ok_or_else(|| usage("-t needs a number of seconds"))?,
            other => return Err(usage(&format!("unknown flag {other}"))),
        }
    }
    let id = it.next().ok_or_else(|| usage("missing <container>"))?;
    if it.next().is_some() {
        return Err(usage("too many arguments"));
    }
    Ok((timeout, id))
}

fn parse_compose(args: Vec<String>) -> Result<ComposeArgs> {
    let mut it = args.into_iter().peekable();
    let (mut file, mut project) = (None, None);
    while let Some(f) = it.next_if(|a| a.starts_with('-')) {
        match f.as_str() {
            "-f" | "--file" => file = Some(it.next().ok_or_else(|| usage("-f needs a file"))?),
            "-p" | "--project-name" => {
                let n = it.next().ok_or_else(|| usage("-p needs a project name"))?;
                if !is_volume_name(&n) {
                    return Err(usage("a project name must match [a-zA-Z0-9][a-zA-Z0-9_.-]*"));
                }
                project = Some(n);
            }
            other => return Err(usage(&format!("unknown compose flag {other}"))),
        }
    }
    let sub = it.next().ok_or_else(|| usage("usage: docker compose [-f file] [-p project] up|down|ps|logs|config|build"))?;
    let rest: Vec<String> = it.collect();
    let flags = |allowed: &[&str]| -> Result<Vec<String>> {
        rest.iter().try_for_each(|a| if allowed.contains(&a.as_str()) { Ok(()) } else { Err(usage(&format!("unexpected argument {a}"))) })?;
        Ok(rest.clone())
    };
    let sub = match sub.as_str() {
        "up" => {
            let f = flags(&["-d", "--detach", "--build"])?;
            ComposeSub::Up { detach: f.iter().any(|a| a == "-d" || a == "--detach"), build: f.iter().any(|a| a == "--build") }
        }
        "down" => ComposeSub::Down { volumes: !flags(&["-v", "--volumes"])?.is_empty() },
        "ps" => {
            flags(&[])?;
            ComposeSub::Ps
        }
        "config" => {
            flags(&[])?;
            ComposeSub::Config
        }
        "build" => {
            flags(&[])?;
            ComposeSub::Build
        }
        "logs" => {
            let follow = rest.iter().any(|a| a == "-f" || a == "--follow");
            let names: Vec<&String> = rest.iter().filter(|a| !a.starts_with('-')).collect();
            if rest.iter().any(|a| a.starts_with('-') && a != "-f" && a != "--follow") || names.len() > 1 {
                return Err(usage("usage: docker compose logs [-f] [service]"));
            }
            ComposeSub::Logs { follow, service: names.first().map(|s| s.to_string()) }
        }
        other => return Err(usage(&format!("unknown compose command {other:?}"))),
    };
    Ok(ComposeArgs { file, project, sub })
}

fn parse_network(args: Vec<String>) -> Result<NetCmd> {
    let mut it = args.into_iter();
    let sub = it.next().ok_or_else(|| usage("usage: docker network create|ls|rm|inspect"))?;
    let rest: Vec<String> = it.collect();
    let one = |what: &str| -> Result<String> {
        match <[String; 1]>::try_from(rest.clone()) {
            Ok([n]) if is_volume_name(&n) => Ok(n),
            Ok(_) => Err(usage(&format!("bad network name for {what}"))),
            Err(_) => Err(usage(&format!("usage: docker network {what} <name>"))),
        }
    };
    match sub.as_str() {
        "create" => one("create").map(NetCmd::Create),
        "rm" | "remove" => one("rm").map(NetCmd::Rm),
        "inspect" => one("inspect").map(NetCmd::Inspect),
        "ls" | "list" if rest.is_empty() => Ok(NetCmd::Ls),
        other => Err(usage(&format!("unknown network command {other:?}"))),
    }
}

fn parse_volume(args: Vec<String>) -> Result<VolumeCmd> {
    let mut it = args.into_iter();
    let sub = it.next().ok_or_else(|| usage("usage: docker volume create|ls|rm|inspect|prune"))?;
    let rest: Vec<String> = it.collect();
    let one = |what: &str| -> Result<String> {
        match <[String; 1]>::try_from(rest.clone()) {
            Ok([n]) if is_volume_name(&n) => Ok(n),
            Ok(_) => Err(usage(&format!("bad volume name for {what}"))),
            Err(_) => Err(usage(&format!("usage: docker volume {what} <name>"))),
        }
    };
    match sub.as_str() {
        "create" => one("create").map(VolumeCmd::Create),
        "rm" | "remove" => one("rm").map(VolumeCmd::Rm),
        "inspect" => one("inspect").map(VolumeCmd::Inspect),
        "ls" | "list" if rest.is_empty() => Ok(VolumeCmd::Ls),
        "prune" if rest.is_empty() => Ok(VolumeCmd::Prune),
        other => Err(usage(&format!("unknown volume command {other:?}"))),
    }
}

fn parse_vm(args: Vec<String>) -> Result<VmCommand> {
    let mut it = args.into_iter().peekable();
    let sub = it.next().ok_or_else(|| usage("usage: docker vm init|start|stop|status"))?;
    match sub.as_str() {
        "init" => {
            let (mut distro, mut use_existing, mut dry_run) = (None, false, false);
            while let Some(flag) = it.next() {
                match flag.as_str() {
                    "--distro" => distro = Some(it.next().ok_or_else(|| usage("--distro needs a name"))?),
                    "--use-existing" => use_existing = true,
                    "--dry-run" => dry_run = true,
                    other => return Err(usage(&format!("unknown flag {other}"))),
                }
            }
            Ok(VmCommand::Init { distro, use_existing, dry_run })
        }
        "start" | "stop" | "status" => {
            if it.next().is_some() {
                return Err(usage("too many arguments"));
            }
            Ok(match sub.as_str() {
                "start" => VmCommand::Start,
                "stop" => VmCommand::Stop,
                _ => VmCommand::Status,
            })
        }
        other => Err(usage(&format!("unknown vm command {other:?}"))),
    }
}

fn parse_save(args: Vec<String>) -> Result<Command> {
    let mut it = args.into_iter().peekable();
    let mut out = None;
    while let Some(f) = it.next_if(|a| a.starts_with('-')) {
        match f.as_str() {
            "-o" | "--output" => out = Some(it.next().ok_or_else(|| usage("-o needs a file"))?),
            other => return Err(usage(&format!("unknown flag {other}"))),
        }
    }
    let images: Vec<String> = it.collect();
    if images.is_empty() {
        return Err(usage("usage: docker save -o <file.tar> <image>..."));
    }
    Ok(Command::Save { out: out.ok_or_else(|| usage("save needs -o <file.tar> (binary output cannot go through a terminal pipe)"))?, images })
}

fn parse_login(args: Vec<String>) -> Result<Command> {
    let mut it = args.into_iter().peekable();
    let (mut user, mut password) = (None, None);
    while let Some(flag) = it.next_if(|a| a.starts_with('-')) {
        let mut value = |what: &str| it.next().ok_or_else(|| usage(&format!("{flag} needs {what}")));
        match flag.as_str() {
            "-u" | "--username" => user = Some(value("a username")?),
            "-p" | "--password" => password = Some(Password::Arg(value("a password")?)),
            "--password-stdin" => password = Some(Password::Stdin),
            other => return Err(usage(&format!("unknown flag {other}"))),
        }
    }
    let server = it.next();
    if it.next().is_some() {
        return Err(usage("too many arguments"));
    }
    Ok(Command::Login {
        server,
        user: user.ok_or_else(|| usage("login needs -u <user>"))?,
        password: password.ok_or_else(|| usage("login needs -p <password> or --password-stdin"))?,
    })
}

fn parse_build(args: Vec<String>) -> Result<BuildArgs> {
    let mut it = args.into_iter().peekable();
    let mut b = BuildArgs::default();
    while let Some(flag) = it.next_if(|a| a.starts_with('-')) {
        let mut value = |what: &str| it.next().ok_or_else(|| usage(&format!("{flag} needs {what}")));
        match flag.as_str() {
            "-t" | "--tag" => b.tag = value("a name[:tag]")?,
            "-f" | "--file" => b.file = Some(value("a Dockerfile path")?),
            "--no-cache" => b.no_cache = true,
            "--target" => b.target = Some(value("a stage name")?),
            "--platform" => b.platform = Some(value("os/arch")?),
            "-q" | "--quiet" => b.quiet = true,
            "--pull" | "--rm" | "--force-rm" | "--load" => {} // accepted, nothing to do
            "--progress" | "--provenance" | "--sbom" => drop(value("a value")?), // accepted, ignored
            "--label" => {
                let kv = value("KEY=VALUE")?;
                let (k, v) = kv.split_once('=').filter(|(k, _)| !k.is_empty()).ok_or_else(|| usage("--label needs KEY=VALUE"))?;
                b.labels.push((k.into(), v.into()));
            }
            "--secret" => {
                let spec = value("id=<id>,src=<file>")?;
                let get = |name: &str| spec.split(',').find_map(|p| p.strip_prefix(name)?.strip_prefix('=')).map(String::from);
                match (get("id"), get("src").or_else(|| get("source"))) {
                    (Some(id), Some(src)) => b.secrets.push((id, src)),
                    _ => return Err(usage("--secret needs id=<id>,src=<file>")),
                }
            }
            "--build-arg" => {
                let kv = value("KEY=VALUE")?;
                let (k, v) = kv.split_once('=').filter(|(k, _)| !k.is_empty()).ok_or_else(|| usage("--build-arg needs KEY=VALUE"))?;
                b.args.push((k.into(), v.into()));
            }
            other => return Err(usage(&format!("unknown flag {other}"))),
        }
    }
    if b.tag.is_empty() {
        return Err(usage("build needs -t <name[:tag]>"));
    }
    b.context = it.next().ok_or_else(|| usage("missing <context>"))?;
    if it.next().is_some() {
        return Err(usage("too many arguments"));
    }
    Ok(b)
}

fn valid_name(n: &str) -> bool {
    n.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// `--health-cmd`, `--health-interval`, `--health-timeout`, `--health-start-period`, `--health-retries`
type HealthOpts = (Option<String>, Option<u64>, Option<u64>, Option<u64>, Option<u32>);

/// Flags of `run` that take a value (so `-e A=1 -it` is not mistaken for the end of the flags).
const RUN_VALUE_FLAGS: &[&str] = &[
    "--name", "-e", "--env", "-v", "--volume", "--network", "--net", "-p", "--publish", "--cap-add", "--cap-drop",
    "--security-opt", "--network-alias", "--entrypoint", "-w", "--workdir", "-u", "--user", "--mem", "--cpu", "--procs", "--restart", "--health-cmd", "--health-interval", "--health-timeout",
    "--health-retries", "--health-start-period",
];

/// `-dit` -> `-d -i -t` (only for the single-letter boolean flags d, i, t), in the leading flag region.
fn expand_short_flags(args: Vec<String>, value_flags: &[&str]) -> Vec<String> {
    let mut out = vec![];
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if !a.starts_with('-') {
            out.push(a);
            break; // first positional: the rest is the target and its command
        }
        let combo = a.len() > 2 && !a.starts_with("--") && a[1..].chars().all(|c| "dit".contains(c));
        if combo {
            out.extend(a[1..].chars().map(|c| format!("-{c}")));
        } else {
            let takes_value = value_flags.contains(&a.as_str());
            out.push(a);
            if takes_value {
                out.extend(it.next());
            }
        }
    }
    out.extend(it);
    out
}

fn parse_run(args: Vec<String>) -> Result<Config> {
    let mut it = expand_short_flags(args, RUN_VALUE_FLAGS).into_iter().peekable();
    let mut cfg = Config::default();
    let (mut h_cmd, mut h_interval, mut h_timeout, mut h_start, mut h_retries): HealthOpts = (None, None, None, None, None);
    let dur = |s: String, flag: &str| docker_image::dockerfile::parse_duration(&s).ok_or_else(|| usage(&format!("{flag} needs a duration like 10s or 1m")));
    while let Some(flag) = it.next_if(|a| a.starts_with('-')) {
        let mut value = |what: &str| it.next().ok_or_else(|| usage(&format!("{flag} needs {what}")));
        match flag.as_str() {
            "--rw" => cfg.rw = true,
            "--rm" => cfg.auto_remove = true,
            "--privileged" => cfg.privileged = true,
            "--cap-add" => cfg.cap_add.push(value("a capability")?),
            "--cap-drop" => cfg.cap_drop.push(value("a capability")?),
            "--security-opt" => cfg.security_opt.push(value("an option")?),
            "-d" | "--detach" => cfg.detach = true,
            "-t" | "--tty" => cfg.tty = true,
            "-i" | "--interactive" => cfg.interactive = true,
            "--no-healthcheck" => cfg.no_healthcheck = true,
            "--entrypoint" => cfg.entrypoint = Some(value("a program")?),
            "-w" | "--workdir" => {
                let w = value("a directory")?;
                if !w.starts_with('/') {
                    return Err(usage("--workdir must be an absolute path"));
                }
                cfg.workdir = Some(w);
            }
            "-u" | "--user" => cfg.user = Some(value("a user")?),
            "--restart" => cfg.restart = Restart::parse(&value("a policy")?)?,
            "--health-cmd" => h_cmd = Some(value("a command")?),
            "--health-interval" => h_interval = Some(dur(value("a duration")?, "--health-interval")?),
            "--health-timeout" => h_timeout = Some(dur(value("a duration")?, "--health-timeout")?),
            "--health-start-period" => h_start = Some(dur(value("a duration")?, "--health-start-period")?),
            "--health-retries" => h_retries = Some(value("a number")?.parse().ok().filter(|n| *n > 0).ok_or_else(|| usage("--health-retries needs a positive number"))?),
            "--name" => {
                let n = value("a name")?;
                if !valid_name(&n) {
                    return Err(usage("--name must match [a-zA-Z0-9][a-zA-Z0-9_.-]*"));
                }
                cfg.name = Some(n);
            }
            "-e" | "--env" => {
                let e = value("KEY=VALUE or KEY")?;
                if e.is_empty() || e.starts_with('=') {
                    return Err(usage("-e needs KEY=VALUE or KEY"));
                }
                cfg.env.push(e);
            }
            "--network" | "--net" => {
                let v = value("bridge, host, none or a network name")?;
                cfg.network = Some(match v.as_str() {
                    "bridge" => Network::Bridge,
                    "host" => Network::Host,
                    "none" => Network::None,
                    name if is_volume_name(name) => {
                        cfg.network_name = Some(name.to_string());
                        Network::Bridge
                    }
                    _ => return Err(usage("--network must be bridge, host, none or a network name")),
                });
            }
            "--network-alias" => {
                let a = value("a name")?;
                if !is_volume_name(&a) {
                    return Err(usage("--network-alias must match [a-zA-Z0-9][a-zA-Z0-9_.-]*"));
                }
                cfg.aliases.push(a);
            }
            "-p" | "--publish" => cfg.ports.push(Port::parse(&value("host:container")?)?),
            "-v" | "--volume" => cfg.volumes.push(Volume::parse(&value("host:container[:ro]")?)?),
            "--mem" => {
                let mb: u64 = value("a number of MB")?
                    .parse()
                    .ok()
                    .filter(|&v| v > 0)
                    .ok_or_else(|| usage("--mem needs a positive number of MB"))?;
                cfg.mem_bytes = Some(mb << 20);
            }
            "--cpu" => {
                cfg.cpu_percent = Some(
                    value("a percentage")?
                        .parse()
                        .ok()
                        .filter(|v| (1..=100).contains(v))
                        .ok_or_else(|| usage("--cpu needs a percentage from 1 to 100"))?,
                );
            }
            "--procs" => {
                cfg.procs = Some(
                    value("a number")?
                        .parse()
                        .ok()
                        .filter(|&v| v > 0)
                        .ok_or_else(|| usage("--procs needs a positive number"))?,
                );
            }
            other => return Err(usage(&format!("unknown flag {other}"))),
        }
    }
    cfg.target = it.next().ok_or_else(|| usage("missing <image|dir>"))?;
    cfg.cmd = it.collect();
    match h_cmd {
        Some(c) => {
            let mut h = Health::new(vec!["/bin/sh".into(), "-c".into(), c]);
            h.interval_s = h_interval.unwrap_or(h.interval_s);
            h.timeout_s = h_timeout.unwrap_or(h.timeout_s);
            h.start_period_s = h_start.unwrap_or(0);
            h.retries = h_retries.unwrap_or(h.retries);
            cfg.health = Some(h);
        }
        None if h_interval.is_some() || h_timeout.is_some() || h_start.is_some() || h_retries.is_some() => {
            return Err(usage("--health-interval/--health-timeout/--health-start-period/--health-retries need --health-cmd"));
        }
        None => {}
    }
    Ok(cfg)
}

impl Volume {
    fn to_arg(&self) -> String {
        format!("{}:{}{}", self.host, self.container, if self.ro { ":ro" } else { "" })
    }
}

impl Command {
    /// The command line that parses back into this command (used to forward it to the VM).
    pub fn to_args(&self) -> Vec<String> {
        let s = |x: &str| x.to_string();
        match self {
            Command::Run(c) => {
                let mut a = vec![s("run")];
                let mut flag = |f: &str, v: Option<String>| {
                    a.push(s(f));
                    a.extend(v);
                };
                if c.detach {
                    flag("-d", None);
                }
                if c.tty {
                    flag("-t", None);
                }
                if c.interactive {
                    flag("-i", None);
                }
                if c.auto_remove {
                    flag("--rm", None);
                }
                if c.restart != Restart::No {
                    flag("--restart", Some(c.restart.to_arg()));
                }
                if c.no_healthcheck {
                    flag("--no-healthcheck", None);
                }
                if let Some(x) = &c.entrypoint {
                    flag("--entrypoint", Some(x.clone()));
                }
                if let Some(x) = &c.workdir {
                    flag("-w", Some(x.clone()));
                }
                if let Some(x) = &c.user {
                    flag("-u", Some(x.clone()));
                }
                if let Some(h) = &c.health {
                    let cmd = if h.test.len() == 3 && h.test[0] == "/bin/sh" && h.test[1] == "-c" { h.test[2].clone() } else { h.test.join(" ") };
                    flag("--health-cmd", Some(cmd));
                    flag("--health-interval", Some(format!("{}s", h.interval_s)));
                    flag("--health-timeout", Some(format!("{}s", h.timeout_s)));
                    flag("--health-start-period", Some(format!("{}s", h.start_period_s)));
                    flag("--health-retries", Some(h.retries.to_string()));
                }
                if c.rw {
                    flag("--rw", None);
                }
                if c.privileged {
                    flag("--privileged", None);
                }
                if let Some(n) = &c.name {
                    flag("--name", Some(n.clone()));
                }
                if let Some(name) = &c.network_name {
                    flag("--network", Some(name.clone()));
                } else if let Some(n) = c.network {
                    flag("--network", Some(s(match n { Network::Bridge => "bridge", Network::Host => "host", Network::None => "none" })));
                }
                for a in &c.aliases {
                    flag("--network-alias", Some(a.clone()));
                }
                for p in &c.ports {
                    flag("-p", Some(format!("{}:{}", p.host, p.container)));
                }
                for e in &c.env {
                    flag("-e", Some(e.clone()));
                }
                for v in &c.volumes {
                    flag("-v", Some(v.to_arg()));
                }
                if let Some(m) = c.mem_bytes {
                    flag("--mem", Some((m >> 20).to_string()));
                }
                if let Some(p) = c.cpu_percent {
                    flag("--cpu", Some(p.to_string()));
                }
                if let Some(p) = c.procs {
                    flag("--procs", Some(p.to_string()));
                }
                for x in &c.cap_add {
                    flag("--cap-add", Some(x.clone()));
                }
                for x in &c.cap_drop {
                    flag("--cap-drop", Some(x.clone()));
                }
                for x in &c.security_opt {
                    flag("--security-opt", Some(x.clone()));
                }
                a.push(c.target.clone());
                a.extend(c.cmd.iter().cloned());
                a
            }
            Command::Exec { id, cmd, tty } => [vec![s("exec")], if *tty { vec![s("-t")] } else { vec![] }, vec![id.clone()], cmd.clone()].concat(),
            Command::Ps { all } => if *all { vec![s("ps"), s("-a")] } else { vec![s("ps")] },
            Command::Logs { follow, id } => if *follow { vec![s("logs"), s("-f"), id.clone()] } else { vec![s("logs"), id.clone()] },
            Command::Stop { id, timeout } => vec![s("stop"), s("-t"), timeout.to_string(), id.clone()],
            Command::Restart { id, timeout } => vec![s("restart"), s("-t"), timeout.to_string(), id.clone()],
            Command::Start(id) => vec![s("start"), id.clone()],
            Command::Kill(id) => vec![s("kill"), id.clone()],
            Command::Update { id, mem_mb, cpu, procs } => {
                let mut v = vec![s("update")];
                for (flag, val) in [("--mem", mem_mb.map(|n| n.to_string())), ("--cpu", cpu.map(|n| n.to_string())), ("--procs", procs.map(|n| n.to_string()))] {
                    if let Some(val) = val {
                        v.extend([s(flag), val]);
                    }
                }
                v.push(id.clone());
                v
            }
            Command::Pause(id) => vec![s("pause"), id.clone()],
            Command::Unpause(id) => vec![s("unpause"), id.clone()],
            Command::Wait(id) => vec![s("wait"), id.clone()],
            Command::Top(id) => vec![s("top"), id.clone()],
            Command::Rename { old, new } => vec![s("rename"), old.clone(), new.clone()],
            Command::Cp { src, dst } => vec![s("cp"), src.clone(), dst.clone()],
            Command::Stats { no_stream, ids } => {
                let mut a = vec![s("stats")];
                if *no_stream {
                    a.push(s("--no-stream"));
                }
                a.extend(ids.iter().cloned());
                a
            }
            Command::Volume(v) => match v {
                VolumeCmd::Create(n) => vec![s("volume"), s("create"), n.clone()],
                VolumeCmd::Ls => vec![s("volume"), s("ls")],
                VolumeCmd::Rm(n) => vec![s("volume"), s("rm"), n.clone()],
                VolumeCmd::Inspect(n) => vec![s("volume"), s("inspect"), n.clone()],
                VolumeCmd::Prune => vec![s("volume"), s("prune")],
            },
            Command::Network(n) => match n {
                NetCmd::Create(x) => vec![s("network"), s("create"), x.clone()],
                NetCmd::Ls => vec![s("network"), s("ls")],
                NetCmd::Rm(x) => vec![s("network"), s("rm"), x.clone()],
                NetCmd::Inspect(x) => vec![s("network"), s("inspect"), x.clone()],
            },
            Command::Compose(c) => {
                let mut a = vec![s("compose")];
                if let Some(f) = &c.file {
                    a.extend([s("-f"), f.clone()]);
                }
                if let Some(p) = &c.project {
                    a.extend([s("-p"), p.clone()]);
                }
                match &c.sub {
                    ComposeSub::Up { detach, build } => {
                        a.push(s("up"));
                        if *detach {
                            a.push(s("-d"));
                        }
                        if *build {
                            a.push(s("--build"));
                        }
                    }
                    ComposeSub::Down { volumes } => {
                        a.push(s("down"));
                        if *volumes {
                            a.push(s("-v"));
                        }
                    }
                    ComposeSub::Ps => a.push(s("ps")),
                    ComposeSub::Config => a.push(s("config")),
                    ComposeSub::Build => a.push(s("build")),
                    ComposeSub::Logs { follow, service } => {
                        a.push(s("logs"));
                        if *follow {
                            a.push(s("-f"));
                        }
                        a.extend(service.iter().cloned());
                    }
                }
                a
            }
            Command::Daemon { host } => [vec![s("daemon")], host.iter().flat_map(|h| [s("--host"), h.clone()]).collect()].concat(),
            Command::SystemDf => vec![s("system"), s("df")],
            Command::ContainerPrune => vec![s("container"), s("prune")],
            Command::Rm { force, id } => if *force { vec![s("rm"), s("-f"), id.clone()] } else { vec![s("rm"), id.clone()] },
            Command::Inspect(id) => vec![s("inspect"), id.clone()],
            Command::Build(b) => {
                let mut a = vec![s("build"), s("-t"), b.tag.clone()];
                if let Some(f) = &b.file {
                    a.extend([s("-f"), f.clone()]);
                }
                for (k, v) in &b.args {
                    a.extend([s("--build-arg"), format!("{k}={v}")]);
                }
                if b.no_cache {
                    a.push(s("--no-cache"));
                }
                if let Some(t) = &b.target {
                    a.extend([s("--target"), t.clone()]);
                }
                a.push(b.context.clone());
                a
            }
            Command::Save { out, images } => [vec![s("save"), s("-o"), out.clone()], images.clone()].concat(),
            Command::Load { input } => vec![s("load"), s("-i"), input.clone()],
            Command::Pull(n) => vec![s("pull"), n.clone()],
            Command::Push(n) => vec![s("push"), n.clone()],
            Command::Tag { src, dst } => vec![s("tag"), src.clone(), dst.clone()],
            Command::Login { server, user, password } => {
                let mut a = vec![s("login"), s("-u"), user.clone()];
                match password {
                    Password::Arg(p) => a.extend([s("-p"), p.clone()]),
                    Password::Stdin => a.push(s("--password-stdin")),
                }
                a.extend(server.iter().cloned());
                a
            }
            Command::Logout(server) => [vec![s("logout")], server.iter().cloned().collect()].concat(),
            Command::Images => vec![s("images")],
            Command::Rmi(n) => vec![s("rmi"), n.clone()],
            Command::Prune => vec![s("prune")],
            Command::Version => vec![s("version")],
            Command::Vm(_) => vec![s("vm")], // handled locally, never forwarded
            Command::Swarm(words) => words.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command> {
        parse(s.split_whitespace().map(String::from))
    }

    fn run(s: &str) -> Config {
        match p(s).unwrap() {
            Command::Run(c) => c,
            other => panic!("not run: {other:?}"),
        }
    }

    #[test]
    fn parses_run_flags_and_command() {
        let c = run("run --mem 64 --cpu 50 --procs 8 --rw ./alpine /bin/sh -c id");
        assert_eq!(c.mem_bytes, Some(64 << 20));
        assert_eq!((c.cpu_percent, c.procs, c.rw), (Some(50), Some(8), true));
        assert_eq!(c.target, "./alpine");
        assert_eq!(c.cmd, ["/bin/sh", "-c", "id"]);
    }

    #[test]
    fn phase2_run_flags() {
        let c = run("run -d --name web -e A=1 -e B -v ./data:/data:ro -v /x:/y alpine sleep 9");
        assert!(c.detach);
        assert_eq!(c.name.as_deref(), Some("web"));
        assert_eq!(c.env, ["A=1", "B"]);
        assert_eq!(c.volumes[0], Volume { host: "./data".into(), container: "/data".into(), ro: true });
        assert!(!c.volumes[1].ro);
        assert_eq!(c.cmd, ["sleep", "9"]);
    }

    #[test]
    fn command_is_optional_and_its_flags_are_not_ours() {
        assert!(run("run alpine").cmd.is_empty());
        assert_eq!(run("run alpine ls -a --all").cmd, ["ls", "-a", "--all"]);
    }

    #[test]
    fn other_subcommands() {
        assert_eq!(p("pull alpine:3.19").unwrap(), Command::Pull("alpine:3.19".into()));
        assert_eq!(p("rmi alpine").unwrap(), Command::Rmi("alpine".into()));
        assert_eq!(p("stop abc").unwrap(), Command::Stop { id: "abc".into(), timeout: 10 });
        assert_eq!(p("stop -t 2 abc").unwrap(), Command::Stop { id: "abc".into(), timeout: 2 });
        assert_eq!(p("images").unwrap(), Command::Images);
        assert_eq!(p("ps").unwrap(), Command::Ps { all: false });
        assert_eq!(p("ps -a").unwrap(), Command::Ps { all: true });
        assert_eq!(p("logs -f web").unwrap(), Command::Logs { follow: true, id: "web".into() });
        assert_eq!(p("rm -f web").unwrap(), Command::Rm { force: true, id: "web".into() });
        assert_eq!(p("inspect web").unwrap(), Command::Inspect("web".into()));
        assert_eq!(
            p("exec web ls -la").unwrap(),
            Command::Exec { id: "web".into(), cmd: vec!["ls".into(), "-la".into()], tty: false }
        );
    }

    #[test]
    fn security_flags() {
        let c = run("run --cap-drop ALL --cap-add net_admin --security-opt no-new-privileges --privileged alpine");
        assert_eq!(c.cap_drop, ["ALL"]);
        assert_eq!(c.cap_add, ["net_admin"]);
        assert_eq!(c.security_opt, ["no-new-privileges"]);
        assert!(c.privileged);
        assert!(p("run --cap-add").is_err());
    }

    #[test]
    fn to_args_round_trips_through_parse() {
        for line in [
            "run -d --name web -p 8080:80 -e A=1 -e B -v ./data:/data:ro --mem 64 --cpu 50 --procs 9 --cap-drop ALL --cap-add chown --security-opt no-new-privileges --privileged --rw --network host alpine sh -c id",
            "run alpine", "run --rm -d alpine sleep 1", "exec web ls -la", "ps", "ps -a", "logs web", "logs -f web", "stop web", "rm web", "rm -f web",
            "run --entrypoint /bin/sh -w /app -u 1000:1000 alpine -c id", "exec -t web sh", "run -d -t -i --restart on-failure:3 --health-cmd true --health-interval 5s --health-retries 2 alpine", "run --restart always --no-healthcheck alpine", "inspect web", "start web", "restart -t 3 web", "kill web", "pause web", "unpause web", "update --mem 64 --cpu 50 --procs 100 web", "wait web", "top web", "rename a b", "cp web:/x ./y", "cp ./y web:/x", "stats", "stats --no-stream a b", "daemon", "daemon --host tcp://127.0.0.1:2375", "compose up", "compose -f a.yml -p proj up -d --build", "compose down -v", "compose ps", "compose logs -f web", "compose config", "compose build", "network create n1", "network ls", "network rm n1", "network inspect n1", "run --network appnet --network-alias db --network-alias database alpine", "volume create data", "volume ls", "volume rm data", "volume inspect data", "volume prune", "system df", "container prune", "build -t app:1 -f Df --build-arg A=1 --build-arg B=2 --target final --no-cache .", "pull alpine", "push a:1",
            "tag a b", "save -o x.tar a b:1", "load -i x.tar", "login -u me -p x ghcr.io", "login -u me --password-stdin", "logout", "logout ghcr.io", "images", "rmi a", "prune", "version",
            "swarm init", "service create --name w --replicas 2 -p 80:80 alpine sleep 9", "service scale w=3 x=1", "stack deploy -c a.yml s",
        ] {
            let cmd = p(line).unwrap();
            assert_eq!(p(&cmd.to_args().join(" ")).unwrap(), cmd, "{line}");
        }
    }

    #[test]
    fn windows_drive_letter_volumes() {
        let v = Volume::parse("C:\\Users\\me\\data:/data:ro").unwrap();
        assert_eq!((v.host.as_str(), v.container.as_str(), v.ro), ("C:\\Users\\me\\data", "/data", true));
        assert_eq!(Volume::parse("D:/x:/y").unwrap().host, "D:/x");
        assert!(!Volume::parse("c:\\a:/b").unwrap().ro);
        assert!(Volume::parse("C:\\a").is_err()); // no container path
        assert!(Volume::parse("relative:/x").unwrap().is_named()); // a volume name, not a path
        assert!(!Volume::parse("./rel:/x").unwrap().is_named());
    }

    #[test]
    fn vm_and_version_commands() {
        assert_eq!(p("version").unwrap(), Command::Version);
        assert_eq!(p("vm status").unwrap(), Command::Vm(VmCommand::Status));
        assert_eq!(p("vm start").unwrap(), Command::Vm(VmCommand::Start));
        assert_eq!(
            p("vm init --distro Ubuntu --use-existing --dry-run").unwrap(),
            Command::Vm(VmCommand::Init { distro: Some("Ubuntu".into()), use_existing: true, dry_run: true })
        );
        for bad in ["vm", "vm bogus", "vm start now", "vm init --nope", "vm init --distro", "version x"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn registry_commands() {
        assert_eq!(
            p("login -u me -p secret localhost:5000").unwrap(),
            Command::Login { server: Some("localhost:5000".into()), user: "me".into(), password: Password::Arg("secret".into()) }
        );
        assert_eq!(
            p("login -u me --password-stdin").unwrap(),
            Command::Login { server: None, user: "me".into(), password: Password::Stdin }
        );
        assert_eq!(p("logout").unwrap(), Command::Logout(None));
        assert_eq!(p("logout ghcr.io").unwrap(), Command::Logout(Some("ghcr.io".into())));
        assert_eq!(p("push a:1").unwrap(), Command::Push("a:1".into()));
        assert_eq!(p("tag a b").unwrap(), Command::Tag { src: "a".into(), dst: "b".into() });
        for bad in ["login", "login -u me", "login -p x", "login -u me -p x a b", "push", "tag a", "tag a b c", "logout a b"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn build_and_prune() {
        assert_eq!(
            p("build -t app:1 -f Df.dev --build-arg A=1 --no-cache .").unwrap(),
            Command::Build(BuildArgs {
                tag: "app:1".into(),
                file: Some("Df.dev".into()),
                args: vec![("A".into(), "1".into())],
                no_cache: true,
                target: None,
                labels: vec![],
                secrets: vec![],
                platform: None,
                quiet: false,
                context: ".".into()
            })
        );
        let Command::Build(b) = p("build -q --label k=v --label a=b=c --secret id=tok,src=/f --platform linux/arm64 --pull --progress plain -t x .").unwrap() else { panic!() };
        assert_eq!((b.quiet, b.platform.as_deref()), (true, Some("linux/arm64")));
        assert_eq!(b.labels, [("k".to_string(), "v".to_string()), ("a".to_string(), "b=c".to_string())]);
        assert_eq!(b.secrets, [("tok".to_string(), "/f".to_string())]);
        assert!(p("build -t x --secret id=only .").is_err() && p("build -t x --label novalue .").is_err());
        assert_eq!(p("prune").unwrap(), Command::Prune);
        for bad in ["build .", "build -t x", "build -t x . .", "build -t x --build-arg A .", "build -t x --nope ."] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tty_restart_and_health_flags() {
        let c = run("run -dit --restart unless-stopped -e A=1 --health-cmd pg_isready --health-interval 10s --health-retries 5 alpine");
        assert!(c.detach && c.interactive && c.tty);
        assert_eq!(c.restart, Restart::UnlessStopped);
        let h = c.health.unwrap();
        assert_eq!((h.interval_s, h.retries, h.test[2].as_str()), (10, 5, "pg_isready"));
        assert_eq!(run("run -ti alpine sh").cmd, ["sh"]);
        assert!(run("run -e A=1 -it alpine").tty, "a value flag before -it must not end flag parsing");
        assert_eq!(run("run -it alpine ls -it").cmd, ["ls", "-it"], "flags after the image belong to the command");
        assert_eq!(p("exec -it web sh").unwrap(), Command::Exec { id: "web".into(), cmd: vec!["sh".into()], tty: true });
        for (s, want) in [("no", Restart::No), ("always", Restart::Always), ("on-failure", Restart::OnFailure(0)), ("on-failure:4", Restart::OnFailure(4))] {
            assert_eq!(Restart::parse(s).unwrap(), want);
            assert_eq!(Restart::parse(&want.to_arg()).unwrap(), want);
        }
        for bad in ["run --restart sometimes alpine", "run --restart on-failure:x alpine", "run --health-interval 5s alpine", "run --health-cmd x --health-retries 0 alpine", "run --health-cmd x --health-interval zz alpine"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn restart_policy_decisions() {
        assert!(!Restart::No.should_restart(1, 0, false));
        assert!(Restart::Always.should_restart(0, 5, false));
        assert!(!Restart::Always.should_restart(0, 5, true), "docker stop wins over the policy");
        assert!(Restart::OnFailure(0).should_restart(1, 99, false) && !Restart::OnFailure(0).should_restart(0, 0, false));
        assert!(Restart::OnFailure(2).should_restart(1, 1, false) && !Restart::OnFailure(2).should_restart(1, 2, false));
    }

    #[test]
    fn compose_commands() {
        assert_eq!(
            p("compose -f x.yml -p demo up -d --build").unwrap(),
            Command::Compose(ComposeArgs { file: Some("x.yml".into()), project: Some("demo".into()), sub: ComposeSub::Up { detach: true, build: true } })
        );
        assert_eq!(
            p("compose logs -f web").unwrap(),
            Command::Compose(ComposeArgs { file: None, project: None, sub: ComposeSub::Logs { follow: true, service: Some("web".into()) } })
        );
        for bad in ["compose", "compose bogus", "compose up --nope", "compose down x", "compose -p ../x up", "compose logs a b", "compose -f"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn entrypoint_workdir_user_flags() {
        let c = run("run --entrypoint /bin/echo -w /srv -u app:app alpine hello");
        assert_eq!((c.entrypoint.as_deref(), c.workdir.as_deref(), c.user.as_deref()), (Some("/bin/echo"), Some("/srv"), Some("app:app")));
        assert_eq!(c.cmd, ["hello"]);
        assert!(p("run -w relative alpine").is_err() && p("run --entrypoint").is_err());
    }

    #[test]
    fn named_networks() {
        let c = run("run --network appnet --network-alias web alpine");
        assert_eq!((c.network, c.network_name.as_deref(), c.aliases.as_slice()), (Some(Network::Bridge), Some("appnet"), ["web".to_string()].as_slice()));
        assert_eq!(run("run --network host alpine").network_name, None);
        assert_eq!(p("network create n").unwrap(), Command::Network(NetCmd::Create("n".into())));
        assert_eq!(p("network ls").unwrap(), Command::Network(NetCmd::Ls));
        for bad in ["network", "network create", "network create ../x", "network ls x", "network bogus", "run --network-alias ../x alpine", "run --network a/b alpine"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn group_aliases_and_new_commands() {
        assert_eq!(p("container ls -a").unwrap(), Command::Ps { all: true });
        assert_eq!(p("buildx build --load --progress=plain --platform linux/arm64 -t a:1 .").unwrap(), p("build -t a:1 .").unwrap());
        assert_eq!(p("container rm -f web").unwrap(), Command::Rm { force: true, id: "web".into() });
        assert_eq!(p("container prune").unwrap(), Command::ContainerPrune);
        assert_eq!(p("image ls").unwrap(), Command::Images);
        assert_eq!(p("image rm a").unwrap(), Command::Rmi("a".into()));
        assert_eq!(p("system df").unwrap(), Command::SystemDf);
        assert_eq!(p("system prune").unwrap(), Command::Prune);
        assert_eq!(p("volume create data").unwrap(), Command::Volume(VolumeCmd::Create("data".into())));
        assert_eq!(p("stats --no-stream web").unwrap(), Command::Stats { no_stream: true, ids: vec!["web".into()] });
        assert_eq!(p("rename a b").unwrap(), Command::Rename { old: "a".into(), new: "b".into() });
        let c = run("run -v data:/var/lib/x -v ./p:/p alpine");
        assert!(c.volumes[0].is_named() && !c.volumes[1].is_named());
        assert_eq!(p("save -o a.tar x y").unwrap(), Command::Save { out: "a.tar".into(), images: vec!["x".into(), "y".into()] });
        assert_eq!(p("load --input a.tar").unwrap(), Command::Load { input: "a.tar".into() });
        for bad in ["save x", "save -o a.tar", "load", "load a.tar", "volume", "volume create", "volume create ../x", "volume ls x", "rename a", "cp a", "stop -t", "stats -x"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn network_flags() {
        let c = run("run --network host -p 8080:80 -p 9000:90/tcp alpine");
        assert_eq!(c.network, Some(Network::Host));
        assert_eq!(c.ports, [Port { host: 8080, container: 80 }, Port { host: 9000, container: 90 }]);
        assert_eq!(run("run alpine").network, None);
        for bad in ["8080", "0:80", "70000:80", "80:80/udp", "a:b"] {
            assert!(Port::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(run("run --network wifi alpine").network_name.as_deref(), Some("wifi")); // any other word names a network
        assert!(p("run --network wi/fi alpine").is_err());
    }

    #[test]
    fn volume_validation() {
        for bad in ["./a", "./a:rel", "./a:/b:zz", "./a:/b/../c", "a:b:c:d", "-x:/d"] {
            assert!(Volume::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        for s in [
            "", "bogus", "run", "pull", "pull a b", "images x", "exec web", "exec", "ps -z", "rm", "logs",
            "run --mem 0 r", "run --mem x r", "run --cpu 0 r", "run --cpu 101 r", "run --procs 0 r",
            "run --nope r", "run --name -bad r", "run --name r", "run -e", "run -e =x r",
        ] {
            assert!(p(s).is_err(), "{s}");
        }
    }
}
