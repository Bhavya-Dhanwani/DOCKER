//! Run Intel (x86-64) images on an ARM64 Linux engine: `qemu-user` registered with the kernel's `binfmt_misc`
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
//! translates every x86-64 program the moment it starts, so a container does not know the difference.
//! It is slower than native and a few programs misbehave under it, but most images just run.
//!
//! `docker emulation enable` does the one-time setup (it fetches the static qemu binary from the
//! `tonistiigi/binfmt` image, about 15 MB, and registers it); `docker pull` and `docker run` call
//! it by themselves when an image has no ARM64 build.
use std::error::Error;

type Res = Result<(), Box<dyn Error>>;

#[cfg(target_os = "linux")]
use docker_core::registry;
#[cfg(target_os = "linux")]
use docker_image::{Reference, Store};
#[cfg(target_os = "linux")]
use std::{fs, path::Path};

const BINFMT: &str = "/proc/sys/fs/binfmt_misc";
const NAME: &str = "qemu-x86_64";
const QEMU_PATH: &str = "/usr/local/bin/qemu-x86_64-static";
// multi-arch (it has an ARM64 variant whose qemu-* binaries translate the other CPUs), unlike multiarch/qemu-user-static
const QEMU_IMAGE: &str = "tonistiigi/binfmt:latest";

/// The line written to `binfmt_misc/register`: `:name:type:offset:magic:mask:interpreter:flags`.
/// The magic and mask are the bytes of an x86-64 ELF header, written as `\xNN` escapes the kernel decodes.
/// Flags P (keep argv[0]: busybox picks its applet from it), O (open the file for the interpreter), C (credentials) and F (open the interpreter now, so it
/// also works inside containers' mount namespaces).
pub fn register_line() -> String {
    let esc = |bytes: &[u8]| bytes.iter().map(|b| format!("\\x{b:02x}")).collect::<String>();
    let magic = [0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0x3e, 0];
    let mask = [0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 0xfe, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 0xff, 0xff, 0xff];
    format!(":{NAME}:M::{}:{}:{QEMU_PATH}:POCF", esc(&magic), esc(&mask))
}

/// Is this pull error "the image only exists for another CPU" on a host where emulation can help?
/// (An ARM64 Linux engine asked for an Intel-only image.)
pub fn applies(error: &str) -> bool {
    cfg!(all(target_os = "linux", target_arch = "aarch64")) && error.contains("has no linux/arm64 build")
}

#[cfg(target_os = "linux")]
pub fn is_enabled() -> bool {
    fs::read_to_string(format!("{BINFMT}/{NAME}")).is_ok_and(|s| s.starts_with("enabled"))
}

#[cfg(not(target_os = "linux"))]
pub fn is_enabled() -> bool {
    false
}

/// Set up emulation if it is not active yet. Needs root (the engine in the VM is root).
#[cfg(target_os = "linux")]
pub fn enable() -> Res {
    if !cfg!(target_arch = "aarch64") {
        return Err("x86-64 emulation is only needed (and offered) on ARM64 engines".into());
    }
    if is_enabled() {
        return Ok(());
    }
    if !Path::new(&format!("{BINFMT}/register")).exists() {
        let ok = std::process::Command::new("mount").args(["-t", "binfmt_misc", "binfmt_misc", BINFMT]).status().is_ok_and(|s| s.success());
        if !ok {
            return Err(format!("{BINFMT} is not available: this kernel has no binfmt_misc support").into());
        }
    }
    if !Path::new(QEMU_PATH).exists() {
        install_qemu()?;
    }
    let entry = format!("{BINFMT}/{NAME}");
    if Path::new(&entry).exists() {
        let _ = fs::write(&entry, "-1"); // drop a stale or disabled registration first
    }
    fs::write(format!("{BINFMT}/register"), register_line()).map_err(|e| format!("cannot register x86-64 emulation: {e} (needs root)"))?;
    eprintln!("x86-64 emulation is on: Intel images will run (slower than native)");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn enable() -> Res {
    Err("x86-64 emulation runs inside the Linux engine (on Windows: in the WSL2 VM, use `docker emulation enable` after `docker vm init`)".into())
}

/// Fetch the static qemu binary out of the `tonistiigi/binfmt` image, then drop that image again.
#[cfg(target_os = "linux")]
fn install_qemu() -> Res {
    use std::os::unix::fs::PermissionsExt;
    eprintln!("Installing x86-64 emulation (one time, about 15 MB)...");
    let store = Store::open(registry::data_dir().join("store"));
    let r = Reference::parse(QEMU_IMAGE)?;
    let meta = docker_image::pull(&store, &r, &mut |_| {})?;
    let mut installed = false;
    for layer in &meta.layers {
        let dir = store.ensure_layer(layer)?;
        let Some(bin) = ["usr/bin/qemu-x86_64", "usr/bin/qemu-x86_64-static"].iter().map(|p| dir.join(p)).find(|p| p.is_file()) else { continue };
        if let Some(parent) = Path::new(QEMU_PATH).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&bin, QEMU_PATH)?;
        fs::set_permissions(QEMU_PATH, fs::Permissions::from_mode(0o755))?;
        installed = true;
        break;
    }
    let _ = store.remove(&r); // the 15 MB binary is all we wanted
    if installed { Ok(()) } else { Err(format!("{QEMU_IMAGE} does not contain a qemu-x86_64 binary for this CPU").into()) }
}

/// Turn emulation off again (the qemu binary stays, `enable` switches it back on).
#[cfg(target_os = "linux")]
pub fn disable() -> Res {
    let entry = format!("{BINFMT}/{NAME}");
    if Path::new(&entry).exists() {
        fs::write(&entry, "-1").map_err(|e| format!("cannot remove the registration: {e}"))?;
    }
    println!("x86-64 emulation is off");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn disable() -> Res {
    enable()
}

pub fn status() -> Res {
    let arch = std::env::consts::ARCH;
    if arch != "aarch64" {
        println!("engine CPU: {arch}: native Intel images run without emulation");
    } else if is_enabled() {
        println!("x86-64 emulation: on (Intel images run under qemu)");
    } else {
        println!("x86-64 emulation: off (it turns on by itself when an image has no ARM64 build, or run `docker emulation enable`)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_line_has_the_right_shape() {
        let l = register_line();
        let parts: Vec<&str> = l.split(':').collect();
        // "" name type offset magic mask interpreter flags
        assert_eq!(parts.len(), 8, "{l}");
        assert_eq!((parts[1], parts[2], parts[3], parts[7]), ("qemu-x86_64", "M", "", "POCF"));
        assert!(parts[4].starts_with("\\x7f\\x45\\x4c\\x46\\x02\\x01\\x01"), "ELF magic: {}", parts[4]);
        assert_eq!(parts[4].len(), 20 * 4); // 20 bytes, 4 characters each
        assert_eq!(parts[5].len(), 20 * 4);
        assert_eq!(parts[6], QEMU_PATH);
    }

    #[test]
    fn only_the_missing_arm_build_error_triggers_it() {
        let hit = applies("foo/bar:latest has no linux/arm64 build");
        assert_eq!(hit, cfg!(all(target_os = "linux", target_arch = "aarch64")));
        assert!(!applies("manifests/latest: HTTP 401"));
        assert!(!applies("foo has no linux/riscv64 build"));
    }
}
