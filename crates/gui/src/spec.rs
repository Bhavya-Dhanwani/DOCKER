//! Turns dialog fields into `docker` CLI arguments (pure, so it is unit-tested).

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Restart {
    #[default]
    No,
    Always,
    UnlessStopped,
    OnFailure,
}

impl Restart {
    pub const ALL: [Restart; 4] = [Restart::No, Restart::Always, Restart::UnlessStopped, Restart::OnFailure];
    pub fn flag(self) -> &'static str {
        match self {
            Restart::No => "no",
            Restart::Always => "always",
            Restart::UnlessStopped => "unless-stopped",
            Restart::OnFailure => "on-failure",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunSpec {
    pub image: String,
    pub name: String,
    /// one `host:container` per line (or comma separated)
    pub ports: String,
    /// one `KEY=value` per line
    pub env: String,
    /// one `host-path-or-volume:container-path[:ro]` per line
    pub volumes: String,
    pub mem_mb: String,
    pub restart: Restart,
    pub detach: bool,
    pub command: String,
}

impl Default for RunSpec {
    fn default() -> Self {
        RunSpec { image: String::new(), name: String::new(), ports: String::new(), env: String::new(), volumes: String::new(), mem_mb: String::new(), restart: Restart::No, detach: true, command: String::new() }
    }
}

fn lines(s: &str) -> impl Iterator<Item = &str> {
    s.lines().map(str::trim).filter(|l| !l.is_empty())
}

fn valid_name(n: &str) -> bool {
    n.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && n.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

impl RunSpec {
    pub fn args(&self) -> Result<Vec<String>, String> {
        let image = self.image.trim();
        if image.is_empty() {
            return Err("Image is required".into());
        }
        let mut a: Vec<String> = vec!["run".into()];
        if self.detach {
            a.push("-d".into());
        }
        let name = self.name.trim();
        if !name.is_empty() {
            if !valid_name(name) {
                return Err("Name may only use letters, digits, _ . - and must start with a letter or digit".into());
            }
            a.extend(["--name".into(), name.into()]);
        }
        if self.restart != Restart::No {
            a.extend(["--restart".into(), self.restart.flag().into()]);
        }
        for p in self.ports.split([',', '\n', ' ']).map(str::trim).filter(|p| !p.is_empty()) {
            let ok = p.split_once(':').is_some_and(|(h, c)| h.parse::<u16>().is_ok() && c.parse::<u16>().is_ok());
            if !ok {
                return Err(format!("Port mapping {p:?} must look like 8080:80"));
            }
            a.extend(["-p".into(), p.into()]);
        }
        for e in lines(&self.env) {
            if e.split('=').next().is_none_or(str::is_empty) {
                return Err(format!("Environment entry {e:?} must look like KEY=value"));
            }
            a.extend(["-e".into(), e.into()]);
        }
        for v in lines(&self.volumes) {
            if !v.contains(':') {
                return Err(format!("Volume {v:?} must look like source:/path/in/container"));
            }
            a.extend(["-v".into(), v.into()]);
        }
        let mem = self.mem_mb.trim();
        if !mem.is_empty() {
            match mem.parse::<u64>() {
                Ok(m) if m > 0 => a.extend(["--mem".into(), m.to_string()]),
                _ => return Err("Memory limit must be a whole number of MB".into()),
            }
        }
        a.push(image.into());
        // ponytail: plain whitespace split, no quote handling; add shell-style parsing if people need quoted args
        a.extend(self.command.split_whitespace().map(String::from));
        Ok(a)
    }
}

#[derive(Clone, Debug, Default)]
pub struct BuildSpec {
    pub context: String,
    pub tag: String,
    /// optional path of the Dockerfile if it is not `Dockerfile` in the context
    pub dockerfile: String,
    pub no_cache: bool,
}

impl BuildSpec {
    pub fn args(&self) -> Result<Vec<String>, String> {
        let (ctx, tag) = (self.context.trim(), self.tag.trim());
        if ctx.is_empty() {
            return Err("Pick a folder that contains a Dockerfile".into());
        }
        if tag.is_empty() || tag.contains(char::is_whitespace) {
            return Err("Tag is required, e.g. myapp:latest".into());
        }
        let mut a: Vec<String> = vec!["build".into(), "-t".into(), tag.into()];
        if !self.dockerfile.trim().is_empty() {
            a.extend(["-f".into(), self.dockerfile.trim().into()]);
        }
        if self.no_cache {
            a.push("--no-cache".into());
        }
        a.push(ctx.into());
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_args_full() {
        let s = RunSpec {
            image: "nginx".into(),
            name: "web".into(),
            ports: "8080:80\n9090:90, 7000:70".into(),
            env: "A=1\n\nB=two words".into(),
            volumes: "data:/var/lib".into(),
            mem_mb: "256".into(),
            restart: Restart::UnlessStopped,
            detach: true,
            command: "nginx -g x".into(),
        };
        let a = s.args().unwrap().join(" ");
        assert_eq!(a, "run -d --name web --restart unless-stopped -p 8080:80 -p 9090:90 -p 7000:70 -e A=1 -e B=two words -v data:/var/lib --mem 256 nginx nginx -g x");
    }

    #[test]
    fn run_args_minimal_and_foreground() {
        let s = RunSpec { image: " alpine ".into(), detach: false, ..Default::default() };
        assert_eq!(s.args().unwrap(), ["run", "alpine"]);
    }

    #[test]
    fn run_args_rejects_bad_input() {
        let base = RunSpec { image: "a".into(), ..Default::default() };
        assert!(RunSpec { image: "".into(), ..base.clone() }.args().is_err());
        assert!(RunSpec { name: "-bad".into(), ..base.clone() }.args().is_err());
        assert!(RunSpec { ports: "80".into(), ..base.clone() }.args().is_err());
        assert!(RunSpec { env: "=x".into(), ..base.clone() }.args().is_err());
        assert!(RunSpec { volumes: "nocolon".into(), ..base.clone() }.args().is_err());
        assert!(RunSpec { mem_mb: "abc".into(), ..base.clone() }.args().is_err());
        assert!(RunSpec { mem_mb: "0".into(), ..base }.args().is_err());
    }

    #[test]
    fn build_args() {
        let b = BuildSpec { context: "C:\\src".into(), tag: "app:1".into(), dockerfile: "Dockerfile.dev".into(), no_cache: true };
        assert_eq!(b.args().unwrap(), ["build", "-t", "app:1", "-f", "Dockerfile.dev", "--no-cache", "C:\\src"]);
        assert!(BuildSpec::default().args().is_err());
        assert!(BuildSpec { context: "x".into(), tag: "a b".into(), ..Default::default() }.args().is_err());
    }
}
