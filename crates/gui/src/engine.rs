//! Which engine is behind the CLI: a Linux VM, or native Windows mode (no WSL2).

pub const INSTALL_COMMAND: &str = "wsl --install";
pub const NATIVE_MARKER: &str = "Engine: native Windows mode";
pub const BANNER: &str = "No Linux engine (WSL2) found: running in native Windows mode. Linux images cannot run. Install WSL2: `wsl --install` (as administrator, restart), then `docker vm init`.";
pub const WHY_DISABLED: &str = "Needs the Linux engine (WSL2). Not available in native Windows mode: only “Run a Windows folder” works.";

/// Some(reason) when `docker version` says the engine is native Windows mode.
pub fn parse_native(version_output: &str) -> Option<String> {
    let line = version_output.lines().find(|l| l.contains(NATIVE_MARKER))?;
    let reason = line.split_once('(').and_then(|(_, r)| r.rsplit_once(')')).map(|(r, _)| r.trim().to_string()).unwrap_or_default();
    Some(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_native_mode() {
        let out = "Client: 0.1.0\nEngine: native Windows mode, no Linux VM (WSL2 is not installed)\n";
        assert_eq!(parse_native(out).as_deref(), Some("WSL2 is not installed"));
        assert_eq!(parse_native("Engine: native Windows mode, no Linux VM").as_deref(), Some(""));
        assert_eq!(parse_native("Client: 1\nVM: sasta (running)\n"), None);
        assert_eq!(parse_native(""), None);
    }
}
