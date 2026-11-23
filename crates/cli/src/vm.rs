//! Windows/macOS: forward commands to the hidden Linux VM and manage that VM.
use docker_core::{
    cli::{Command, VmCommand},
    registry,
    vm::{parse_wsl_list, Driver},
};
use docker_image::{Reference, Store};
use std::{error::Error, fs, path::{Path, PathBuf}, process::{Command as Proc, Stdio}};

type Res<T = ()> = Result<T, Box<dyn Error>>;

/// Why there is no Linux VM to forward to, if that is the case. Windows without WSL2 then runs in native
/// mode (Job Objects) instead of failing. `SASTA_ENGINE=native|vm` forces the choice; `SASTA_NO_WSL=1`
/// pretends WSL is not installed (used by the tests).
pub fn native_reason() -> Option<String> {
    static REASON: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    REASON.get_or_init(detect_native_reason).clone()
}

fn detect_native_reason() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    match std::env::var("SASTA_ENGINE").ok().as_deref() {
        Some("native") => return Some("SASTA_ENGINE=native".into()),
        Some("vm") => return None,
        _ => {}
    }
    if std::env::var_os("SASTA_NO_WSL").is_some() {
        return Some("WSL2 is not installed (SASTA_NO_WSL)".into());
    }
    let out = Proc::new("wsl.exe").args(["-l", "-q"]).stdin(Stdio::null()).output();
    let (spawned, ok, text) = match &out {
        Ok(o) => (true, o.status.success(), parse_wsl_text(&[o.stdout.as_slice(), o.stderr.as_slice()].concat())),
        Err(_) => (false, false, String::new()),
    };
    docker_core::vm::wsl_missing(spawned, ok, &text).then(|| "WSL2 is not installed".into())
}

/// wsl.exe prints UTF-16LE; decode it for matching.
fn parse_wsl_text(bytes: &[u8]) -> String {
    if bytes.contains(&0) {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The friendly hint printed after an error when running without a Linux engine.
pub const NATIVE_HINT: &str = "Linux images need a Linux engine, which on Windows is WSL2: run `wsl --install` (as administrator, then restart) and `docker vm init`.\n\
                               Without it only native Windows mode works: `docker run <folder> <program> [args]` runs a program under a Job Object with memory/CPU limits, and ps/logs/stop/rm/stats manage those.";

fn driver(name: Option<&str>) -> Res<Driver> {
    Driver::detect(name).ok_or_else(|| "this OS runs containers natively: no VM needed".into())
}

fn proc(argv: &[String]) -> Proc {
    let mut c = Proc::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

fn wsl_names(args: &[&str]) -> Vec<String> {
    Proc::new("wsl.exe").args(args).output().map(|o| parse_wsl_list(&o.stdout)).unwrap_or_default()
}

fn has(list: &[String], name: &str) -> bool {
    list.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Make sure the VM exists and is running (and stays running after this command ends).
fn ensure_up(d: &Driver) -> Res {
    match d {
        Driver::Wsl { distro } => {
            if has(&wsl_names(&["-l", "--running", "-q"]), distro) {
                return Ok(());
            }
            if !has(&wsl_names(&["-l", "-q"]), distro) {
                return Err(format!("the Linux VM {distro:?} does not exist yet: run `docker vm init`").into());
            }
            // A WSL VM shuts down when idle, which would kill detached containers: park a hidden `sleep` in it.
            let mut keepalive = proc(&d.run_argv("sleep", &["2147483647".into()], &[]));
            keepalive.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            #[cfg(windows)]
            std::os::windows::process::CommandExt::creation_flags(&mut keepalive, 0x0800_0000); // CREATE_NO_WINDOW
            spawn_detached(&mut keepalive).map_err(|e| format!("cannot start wsl.exe: {e}"))?;
            Ok(())
        }
        Driver::Lima { instance } => {
            let out = Proc::new("limactl").args(["list", "--format", "{{.Status}}", instance]).output().map_err(|e| format!("limactl not found ({e}); install Lima: brew install lima"))?;
            match String::from_utf8_lossy(&out.stdout).trim() {
                "Running" => Ok(()),
                "" => Err(format!("the Linux VM {instance:?} does not exist yet: run `docker vm init`").into()),
                _ => run_ok(&["limactl", "start", instance]),
            }
        }
    }
}

/// Start a long-lived child without letting it hold on to our stdio.
///
/// On Windows a child inherits every inheritable handle, including the pipe a caller (`$x = docker ps`,
/// a CI step, ...) is reading our output from. A background child that keeps that pipe open makes the
/// caller wait for end-of-output forever, so the std handles are made non-inheritable while it starts.
fn spawn_detached(cmd: &mut Proc) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT},
            System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
        };
        let std_handles = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|h| unsafe { GetStdHandle(h) });
        let set = |flag| {
            for h in std_handles {
                unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, flag) }; // invalid handles just fail
            }
        };
        set(0);
        let child = cmd.spawn();
        set(HANDLE_FLAG_INHERIT); // later children (the forwarded command) need them
        child.map(drop)
    }
    #[cfg(not(windows))]
    cmd.spawn().map(drop)
}

fn run_ok(argv: &[&str]) -> Res {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let status = proc(&argv).status().map_err(|e| format!("cannot run {}: {e}", argv[0]))?;
    if status.success() { Ok(()) } else { Err(format!("`{}` failed ({status})", argv.join(" ")).into()) }
}

/// Run `cmd` in the VM and return its exit code.
pub fn forward(cmd: Command) -> Res<i32> {
    let d = driver(None)?;
    let cmd = d.translate(cmd)?;
    ensure_up(&d)?;
    let argv = d.docker_argv(&cmd.to_args());
    let mut p = proc(&argv);
    if matches!(d, Driver::Wsl { .. }) {
        p.env("WSLENV", wslenv(std::env::vars().map(|(k, _)| k), std::env::var("WSLENV").ok().as_deref()));
    }
    let status = p.status().map_err(|e| format!("cannot run {}: {e}", argv[0]))?;
    trim_if_heavy(&d, &cmd.to_args());
    Ok(status.code().unwrap_or(1))
}

/// Commands that read or write a lot of image data: afterwards the VM's file cache is mostly dead weight.
fn is_heavy(args: &[String]) -> bool {
    let mut w = args.iter().map(String::as_str).filter(|a| !a.starts_with('-'));
    let first = match w.next() {
        Some("image" | "container") => w.next(),
        Some("compose") => w.next().filter(|c| matches!(*c, "build" | "pull" | "up" | "push")).map(|_| "pull"),
        other => other,
    };
    matches!(first, Some("pull" | "build" | "buildx" | "load" | "import" | "save" | "commit" | "push" | "export"))
}

/// Give the VM's file cache back: WSL2 and Lima keep it as host RAM (a pull left the idle VM at about 1 GB, 0.6 GB after dropping it).
/// Costs a moment when the same files are read again; the files themselves stay.
fn trim_if_heavy(d: &Driver, args: &[String]) {
    if is_heavy(args) {
        let _ = proc(&d.shell_argv("sync; echo 1 > /proc/sys/vm/drop_caches")).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
}

/// Forward raw arguments (the commands `extra` handles) into the VM, translating the host paths of the options that take one.
pub fn forward_args(args: Vec<String>) -> Res<i32> {
    let d = driver(None)?;
    let args = translate_raw(&d, args)?;
    ensure_up(&d)?;
    let argv = d.docker_argv(&args);
    let mut p = proc(&argv);
    if matches!(d, Driver::Wsl { .. }) {
        p.env("WSLENV", wslenv(std::env::vars().map(|(k, _)| k), std::env::var("WSLENV").ok().as_deref()));
    }
    let status = p.status().map_err(|e| format!("cannot run {}: {e}", argv[0]))?;
    trim_if_heavy(&d, &args);
    Ok(status.code().unwrap_or(1))
}

/// `-v C:\data:/data`, `--env-file x.env`, `-o out.tar` (export), `import file.tar`: the VM sees these files under another path.
fn translate_raw(d: &Driver, mut a: Vec<String>) -> Res<Vec<String>> {
    let importing = a.first().is_some_and(|w| w == "import") || (a.len() > 1 && a[0] == "image" && a[1] == "import");
    let mut i = 0;
    let mut first_positional_done = !importing;
    while i < a.len() {
        match a[i].as_str() {
            "-v" | "--volume" if i + 1 < a.len() => {
                if let Ok(v) = docker_core::cli::Volume::parse(&a[i + 1])
                    && !v.is_named()
                {
                    a[i + 1] = docker_core::cli::Volume { host: d.path(&v.host)?, ..v }.to_arg();
                }
                i += 1;
            }
            "--env-file" | "-o" | "--output" if i + 1 < a.len() => {
                a[i + 1] = d.path(&a[i + 1])?;
                i += 1;
            }
            x if !first_positional_done && !x.starts_with('-') && x != "image" && x != "import" => {
                if std::path::Path::new(x).is_file() {
                    a[i] = d.path(x)?;
                }
                first_positional_done = true;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(a)
}

/// Windows variables the forwarded command gets to see (`${TOKEN}` in a compose file, `docker run -e TOKEN`),
/// like Docker on Windows. System variables that mean something else on Linux stay behind.
fn wslenv(names: impl Iterator<Item = String>, existing: Option<&str>) -> String {
    // the engine itself reads some of these (LOCALAPPDATA picks its image store, XDG_*/DOCKER_* its settings)
    const SKIP: [&str; 36] = [
        "PATH", "PATHEXT", "HOME", "USER", "SHELL", "PWD", "TMPDIR", "TEMP", "TMP", "LANG", "TERM", "HOSTNAME", "WSLENV", "OS",
        "LOCALAPPDATA", "APPDATA", "USERPROFILE", "PROGRAMDATA", "PROGRAMFILES", "COMMONPROGRAMFILES", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR",
        "COMPUTERNAME", "USERNAME", "USERDOMAIN", "HOMEDRIVE", "HOMEPATH", "LOGONSERVER", "COMSPEC", "ALLUSERSPROFILE", "PUBLIC",
        "SESSIONNAME", "DRIVERDATA", "ONEDRIVE", "PSMODULEPATH",
    ];
    const SKIP_PREFIX: [&str; 6] = ["XDG_", "DOCKER_", "SASTA_", "PROCESSOR_", "NUMBER_OF_", "ONEDRIVE"];
    let ok = |n: &str| n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut list: Vec<String> = existing.map(|e| e.split(':').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default();
    for n in names {
        let up = n.to_ascii_uppercase();
        let system = SKIP.contains(&up.as_str()) || SKIP_PREFIX.iter().any(|p| up.starts_with(p)) || up.starts_with("PROGRAMFILES") || up.starts_with("COMMONPROGRAMFILES") || up.starts_with("USERDOMAIN");
        if ok(&n) && !system && !list.contains(&n) {
            list.push(n);
        }
    }
    list.join(":")
}

#[cfg(test)]
mod heavy_tests {
    use super::is_heavy;

    #[test]
    fn only_image_data_commands_trim() {
        let h = |s: &str| is_heavy(&s.split(' ').map(String::from).collect::<Vec<_>>());
        assert!(h("pull alpine") && h("image pull -q alpine") && h("build -t x .") && h("compose up -d") && h("load -i a.tar"));
        assert!(!h("ps -a") && !h("run alpine true") && !h("compose ps") && !h("logs web"));
    }
}

#[cfg(test)]
mod wslenv_tests {
    use super::wslenv;

    #[test]
    fn forwards_user_variables_but_not_system_ones() {
        let names = ["ACCESS_TOKEN", "Path", "ProgramFiles(x86)", "HOME", "A1"].map(String::from);
        assert_eq!(wslenv(names.into_iter(), Some("X:Y")), "X:Y:ACCESS_TOKEN:A1");
    }
}

/// First line of the VM's `docker version`, if the VM is up.
pub fn vm_version() -> Option<String> {
    let d = Driver::detect(None)?;
    ensure_up(&d).ok()?;
    let out = proc(&d.docker_argv(&["version".into()])).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string())
}

pub fn manage(v: &VmCommand) -> Res {
    if let Some(reason) = native_reason() {
        return match v {
            VmCommand::Status => {
                println!("VM:      none ({reason})");
                println!("engine:  native Windows mode (Job Objects); Linux images need WSL2");
                println!("
{NATIVE_HINT}");
                Ok(())
            }
            _ => Err(format!("there is no Linux VM to manage: {reason}

{NATIVE_HINT}").into()),
        };
    }
    match v {
        VmCommand::Init { distro, use_existing, dry_run } => init(&driver(distro.as_deref())?, *use_existing, *dry_run),
        VmCommand::Start => {
            let d = driver(None)?;
            ensure_up(&d)?;
            println!("VM {:?} is running", d.name());
            Ok(())
        }
        VmCommand::Stop => match driver(None)? {
            Driver::Wsl { distro } => run_ok(&["wsl.exe", "--terminate", &distro]),
            Driver::Lima { instance } => run_ok(&["limactl", "stop", &instance]),
        },
        VmCommand::Status => status(),
    }
}

fn status() -> Res {
    let d = driver(None)?;
    let (kind, state) = match &d {
        Driver::Wsl { distro } => (
            "WSL2 distribution",
            if has(&wsl_names(&["-l", "--running", "-q"]), distro) {
                "running"
            } else if has(&wsl_names(&["-l", "-q"]), distro) {
                "stopped"
            } else {
                "not created (run `docker vm init`)"
            },
        ),
        Driver::Lima { .. } => ("Lima instance", "see `limactl list`"),
    };
    println!("VM:      {kind} {:?}\nstate:   {state}", d.name());
    println!("engine:  {}", vm_version().unwrap_or_else(|| "not reachable".into()));
    Ok(())
}

/// The Linux `docker` binary to install into the VM: `docker-linux-<arch>` next to this executable.
fn linux_binary() -> Res<PathBuf> {
    if let Some(p) = std::env::var_os("RUST_DOCKER_LINUX_BIN") {
        return Ok(p.into());
    }
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => return Err(format!("unsupported CPU {other}").into()),
    };
    let exe = std::env::current_exe()?;
    let p = exe.with_file_name(format!("docker-linux-{arch}"));
    if p.is_file() {
        Ok(p)
    } else {
        Err(format!("{} not found: build it with scripts\\build.ps1 (or set RUST_DOCKER_LINUX_BIN)", p.display()).into())
    }
}

/// Run (or, with `--dry-run`, just print) one setup step.
fn step(dry: bool, what: &str, argv: &[String], stdin: Option<&Path>) -> Res {
    println!("-> {what}");
    println!("   {}", argv.join(" "));
    if dry {
        return Ok(());
    }
    let mut c = proc(argv);
    if let Some(f) = stdin {
        c.stdin(fs::File::open(f)?);
    }
    let status = c.status().map_err(|e| format!("cannot run {}: {e}", argv[0]))?;
    if status.success() { Ok(()) } else { Err(format!("step failed: {what} ({status})").into()) }
}

fn init(d: &Driver, use_existing: bool, dry: bool) -> Res {
    let bin = linux_binary()?;
    let vm_bin = Driver::vm_bin();
    let s = |x: &str| x.to_string();
    let mut fresh = false;

    match d {
        Driver::Wsl { distro } => {
            if !has(&wsl_names(&["-l", "-q"]), distro) {
                if use_existing {
                    return Err(format!("no WSL distribution named {distro:?} (drop --use-existing to create one)").into());
                }
                // The VM's root filesystem is Alpine's: pulled with our own image code, imported as a distro.
                let store = Store::open(registry::data_dir().join("store"));
                let r = Reference::parse("alpine")?;
                println!("-> downloading the Alpine root filesystem");
                let blob = if dry {
                    PathBuf::from("<alpine-rootfs.tar.gz>")
                } else {
                    let meta = docker_image::pull(&store, &r, &mut |l| println!("   {l}"))?;
                    store.blob_path(&meta.layers[0].digest)?
                };
                let dir = registry::data_dir().join("vm").join(distro);
                if !dry {
                    fs::create_dir_all(&dir)?;
                }
                step(dry, "create the VM", &[s("wsl.exe"), s("--import"), distro.clone(), dir.to_string_lossy().into_owned(), blob.to_string_lossy().into_owned(), s("--version"), s("2")], None)?;
                fresh = true;
            }
        }
        Driver::Lima { instance } => {
            let out = Proc::new("limactl").args(["list", "-q"]).output().map_err(|e| format!("limactl not found ({e}); install Lima: brew install lima"))?;
            if !String::from_utf8_lossy(&out.stdout).lines().any(|l| l.trim() == instance) {
                step(dry, "create the VM", &[s("limactl"), s("create"), format!("--name={instance}"), s("--tty=false"), s("template://default")], None)?;
            }
            step(dry, "start the VM", &[s("limactl"), s("start"), instance.clone()], None)?;
        }
    }

    let install = format!("mkdir -p \"$(dirname {vm_bin})\" && cat > {vm_bin} && chmod 755 {vm_bin}");
    step(dry, "install the Linux docker engine in the VM", &d.shell_argv(&install), Some(&bin))?;
    if fresh {
        // Alpine's root filesystem has no curl; `docker pull` needs it inside the VM.
        let apk = d.shell_argv("apk add --no-cache curl ca-certificates");
        if let Err(e) = step(dry, "install curl in the VM (needs internet)", &apk, None) {
            eprintln!("warning: {e}\n         `docker pull` needs curl inside the VM; run: wsl -d {} -u root -- apk add curl", d.name());
        }
    }
    if !dry {
        println!("engine: {}", vm_version().unwrap_or_else(|| "installed, but not answering yet".into()));
        println!("Done. Try: docker pull alpine && docker run alpine echo hello");
    }
    Ok(())
}
