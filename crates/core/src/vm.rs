//! The hidden Linux VM that runs the real engine on Windows (WSL2) and macOS (Lima).
//! Everything here is pure (builds argv, translates paths), so it is unit tested on every platform.
use crate::{
    cli::{Command, Volume},
    error::{Error, Result},
};
use std::path::Path;

/// Where the Linux `docker` binary lives inside the VM.
pub const DEFAULT_VM_BIN: &str = "/usr/local/bin/docker";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Driver {
    /// A dedicated WSL2 distribution.
    Wsl { distro: String },
    /// A Lima instance (https://lima-vm.io).
    Lima { instance: String },
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl Driver {
    /// The driver for this OS, if it needs one. `name` overrides the VM's name.
    pub fn detect(name: Option<&str>) -> Option<Driver> {
        if cfg!(windows) {
            let distro = name.map(String::from).or_else(|| var("RUST_DOCKER_DISTRO")).unwrap_or_else(|| "rust-docker".into());
            Some(Driver::Wsl { distro })
        } else if cfg!(target_os = "macos") {
            let instance = name.map(String::from).or_else(|| var("RUST_DOCKER_LIMA")).unwrap_or_else(|| "rust-docker".into());
            Some(Driver::Lima { instance })
        } else {
            None
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Driver::Wsl { distro } => distro,
            Driver::Lima { instance } => instance,
        }
    }

    pub fn vm_bin() -> String {
        var("RUST_DOCKER_VM_BIN").unwrap_or_else(|| DEFAULT_VM_BIN.into())
    }

    /// `RUST_DOCKER_VM_HOME` on the host becomes `RUST_DOCKER_HOME` inside the VM (used by tests).
    fn vm_env() -> Vec<String> {
        var("RUST_DOCKER_VM_HOME").map(|h| vec![format!("RUST_DOCKER_HOME={h}")]).unwrap_or_default()
    }

    /// argv that runs `program args...` as root inside the VM (no shell involved).
    pub fn run_argv(&self, program: &str, args: &[String], env: &[String]) -> Vec<String> {
        let mut inner: Vec<String> = vec![];
        if !env.is_empty() {
            inner.push("env".into());
            inner.extend(env.iter().cloned());
        }
        inner.push(program.into());
        inner.extend(args.iter().cloned());
        let s = |x: &str| x.to_string();
        let mut v = match self {
            Driver::Wsl { distro } => vec![s("wsl.exe"), s("-d"), distro.clone(), s("-u"), s("root"), s("--exec")],
            Driver::Lima { instance } => vec![s("limactl"), s("shell"), s("--workdir"), s("/"), instance.clone(), s("sudo")],
        };
        v.extend(inner);
        v
    }

    /// argv that runs the VM's `docker` with `args`.
    pub fn docker_argv(&self, args: &[String]) -> Vec<String> {
        self.run_argv(&Self::vm_bin(), args, &Self::vm_env())
    }

    /// argv for a `sh -c <script>` inside the VM.
    pub fn shell_argv(&self, script: &str) -> Vec<String> {
        self.run_argv("sh", &["-c".into(), script.into()], &[])
    }

    /// Translate a host path into the path the VM sees it at.
    pub fn path(&self, host: &str) -> Result<String> {
        let abs = std::path::absolute(host).map_err(|e| Error::io(format!("resolve {host}"), e))?;
        let abs = abs.to_string_lossy().into_owned();
        match self {
            Driver::Wsl { .. } => wsl_path(&abs),
            Driver::Lima { .. } => Ok(collapse(&abs)), // Lima mounts $HOME at the same path
        }
    }

    /// Rewrite every host path in `cmd` so the VM can find it.
    pub fn translate(&self, cmd: Command) -> Result<Command> {
        Ok(match cmd {
            Command::Run(mut c) => {
                if Path::new(&c.target).is_dir() {
                    return Err(Error::Usage(format!(
                        "{:?} is a directory. A Linux VM runs images (docker pull/build first); on Windows, `docker --native run` runs a directory as a Windows process tree",
                        c.target
                    )));
                }
                c.volumes = c
                    .volumes
                    .into_iter()
                    .map(|v| if v.is_named() { Ok(v) } else { Ok(Volume { host: self.path(&v.host)?, ..v }) })
                    .collect::<Result<_>>()?;
                Command::Run(c)
            }
            Command::Build(mut b) => {
                b.context = self.path(&b.context)?;
                b.file = b.file.map(|f| self.path(&f)).transpose()?;
                Command::Build(b)
            }
            Command::Compose(mut c) => {
                c.file = c.file.map(|f| self.path(&f)).transpose()?;
                // without -f the VM looks in its own working directory, so pass the host's one explicitly
                if c.file.is_none() {
                    c.file = Some(self.path("compose.yaml").and_then(|p| if std::path::Path::new("compose.yaml").exists() { Ok(p) } else { self.path("docker-compose.yml") })?);
                }
                Command::Compose(c)
            }
            Command::Save { out, images } => Command::Save { out: self.path(&out)?, images },
            Command::Load { input } => Command::Load { input: self.path(&input)? },
            Command::Cp { src, dst } => {
                // exactly one side is `<container>:<path>`; the other is a host path
                let fix = |s: String| if crate::cli::split_container_ref(&s).is_some() { Ok(s) } else { self.path(&s) };
                Command::Cp { src: fix(src)?, dst: fix(dst)? }
            }
            // `docker stack deploy -c <host file>`: the VM needs the file's VM path
            Command::Swarm(mut w) => {
                if let Some(i) = w.iter().position(|a| a == "-c" || a == "--compose-file").filter(|i| i + 1 < w.len()) {
                    w[i + 1] = self.path(&w[i + 1])?;
                }
                Command::Swarm(w)
            }
            other => other,
        })
    }
}

/// Resolve `.`/`..` and duplicate slashes in a POSIX path.
fn collapse(p: &str) -> String {
    let normalized = p.replace('\\', "/");
    let mut out: Vec<&str> = vec![];
    for c in normalized.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    format!("/{}", out.join("/"))
}

/// `C:\Users\me\x` -> `/mnt/c/Users/me/x` (how WSL mounts Windows drives).
pub fn wsl_path(win: &str) -> Result<String> {
    let p = win.strip_prefix("\\\\?\\").unwrap_or(win);
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let rest = &p[2..];
        Ok(collapse(&format!("/mnt/{}/{}", (b[0] as char).to_ascii_lowercase(), rest)))
    } else if p.starts_with('/') && !p.starts_with("//") {
        Ok(collapse(p)) // already a Linux-style path
    } else {
        Err(Error::Usage(format!("{win}: network (UNC) paths cannot be shared with the VM; use a local drive")))
    }
}

/// `wsl -l -q` prints UTF-16LE (with a BOM); decode it into distro names.
pub fn parse_wsl_list(bytes: &[u8]) -> Vec<String> {
    let text = if bytes.contains(&0) {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.lines().map(|l| l.trim_matches(|c: char| c == '\u{feff}' || c.is_whitespace())).filter(|l| !l.is_empty()).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;

    fn wsl() -> Driver {
        Driver::Wsl { distro: "rust-docker".into() }
    }

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn windows_paths_become_mnt_paths() {
        assert_eq!(wsl_path("C:\\Users\\me\\data").unwrap(), "/mnt/c/Users/me/data");
        assert_eq!(wsl_path("d:\\x\\..\\y\\.\\z").unwrap(), "/mnt/d/y/z");
        assert_eq!(wsl_path("\\\\?\\E:\\proj").unwrap(), "/mnt/e/proj");
        assert_eq!(wsl_path("C:/a/b").unwrap(), "/mnt/c/a/b");
        assert_eq!(wsl_path("/already/linux").unwrap(), "/already/linux");
        assert!(wsl_path("\\\\server\\share\\x").is_err());
    }

    #[test]
    fn wsl_listing_is_decoded_from_utf16() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "Ubuntu\r\nrust-docker\r\n".encode_utf16() {
            bytes.extend(u.to_le_bytes());
        }
        assert_eq!(parse_wsl_list(&bytes), ["Ubuntu", "rust-docker"]);
        assert_eq!(parse_wsl_list(b"plain\nlines\n"), ["plain", "lines"]);
        assert!(parse_wsl_list(b"").is_empty());
    }

    #[test]
    fn wsl_argv_uses_exec_not_a_shell() {
        let mut cmd = args("run -p 80:80 alpine sh -c");
        cmd.push("echo hi; echo \"quoted\"".into());
        let a = wsl().docker_argv(&cmd);
        assert_eq!(&a[..6], ["wsl.exe", "-d", "rust-docker", "-u", "root", "--exec"]);
        assert_eq!(a[6], DEFAULT_VM_BIN);
        assert_eq!(a.last().unwrap(), "echo hi; echo \"quoted\""); // one untouched argument: no shell in between
        let with_env = wsl().run_argv("/x/docker", &args("ps"), &["RUST_DOCKER_HOME=/t".into()]);
        assert_eq!(&with_env[6..], ["env", "RUST_DOCKER_HOME=/t", "/x/docker", "ps"]);
    }

    #[test]
    fn lima_argv_goes_through_limactl_and_sudo() {
        let l = Driver::Lima { instance: "rd".into() };
        let a = l.run_argv("/usr/local/bin/docker", &args("ps"), &[]);
        assert_eq!(a, ["limactl", "shell", "--workdir", "/", "rd", "sudo", "/usr/local/bin/docker", "ps"]);
        assert_eq!(l.shell_argv("true")[6..], ["sh", "-c", "true"]);
    }

    // needs the host's own path rules to understand `C:\\...` (std::path::absolute), so Windows only
    #[cfg(windows)]
    #[test]
    fn translate_rewrites_volumes_and_build_paths_for_wsl() {
        let cmd = cli::parse(args("run -v C:\\data:/d:ro -v D:/x/../y:/e alpine ls")).unwrap();
        let Command::Run(c) = wsl().translate(cmd).unwrap() else { panic!() };
        assert_eq!(c.volumes[0].host, "/mnt/c/data");
        assert!(c.volumes[0].ro);
        assert_eq!(c.volumes[1].host, "/mnt/d/y");
        assert_eq!(c.target, "alpine");

        let cmd = cli::parse(args("build -t a -f D:\\p\\Dockerfile.dev D:\\p")).unwrap();
        let Command::Build(b) = wsl().translate(cmd).unwrap() else { panic!() };
        assert_eq!((b.context.as_str(), b.file.as_deref()), ("/mnt/d/p", Some("/mnt/d/p/Dockerfile.dev")));
    }

    #[test]
    fn translate_leaves_other_commands_alone_and_explains_directories() {
        let cmd = cli::parse(args("ps -a")).unwrap();
        assert_eq!(wsl().translate(cmd).unwrap(), Command::Ps { all: true });
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        let e = wsl().translate(Command::Run(cli::Config { target: dir, cmd: vec!["x".into()], ..Default::default() })).unwrap_err();
        assert!(e.to_string().contains("--native"));
    }

    #[test]
    fn only_windows_and_macos_have_a_driver() {
        let d = Driver::detect(Some("x"));
        if cfg!(windows) {
            assert_eq!(d, Some(Driver::Wsl { distro: "x".into() }));
        } else if cfg!(target_os = "macos") {
            assert_eq!(d, Some(Driver::Lima { instance: "x".into() }));
        } else {
            assert_eq!(d, None);
        }
    }
}
