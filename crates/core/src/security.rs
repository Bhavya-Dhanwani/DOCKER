//! Container hardening policy: Linux capabilities, seccomp, no-new-privileges.
//! This file is portable (pure policy + the BPF program builder); `linux/harden.rs` applies it.
use crate::{cli::Config, error::{Error, Result}};
use serde::{Deserialize, Serialize};

/// Capability names in kernel numbering (bit n of a mask = capability n).
pub const CAP_NAMES: [&str; 41] = [
    "chown", "dac_override", "dac_read_search", "fowner", "fsetid", "kill", "setgid", "setuid", "setpcap",
    "linux_immutable", "net_bind_service", "net_broadcast", "net_admin", "net_raw", "ipc_lock", "ipc_owner",
    "sys_module", "sys_rawio", "sys_chroot", "sys_ptrace", "sys_pacct", "sys_admin", "sys_boot", "sys_nice",
    "sys_resource", "sys_time", "sys_tty_config", "mknod", "lease", "audit_write", "audit_control", "setfcap",
    "mac_override", "mac_admin", "syslog", "wake_alarm", "block_suspend", "audit_read", "perfmon", "bpf",
    "checkpoint_restore",
];

/// What Docker gives a container by default.
const DEFAULT_CAPS: [&str; 14] = [
    "chown", "dac_override", "fsetid", "fowner", "mknod", "net_raw", "setgid", "setuid", "setfcap", "setpcap",
    "net_bind_service", "sys_chroot", "kill", "audit_write",
];

fn bit(name: &str) -> Result<u64> {
    let n = name.trim().to_ascii_lowercase();
    let n = n.strip_prefix("cap_").unwrap_or(&n);
    CAP_NAMES
        .iter()
        .position(|c| *c == n)
        .map(|i| 1u64 << i)
        .ok_or_else(|| Error::Usage(format!("unknown capability {name:?}")))
}

fn mask(names: &[&str]) -> u64 {
    names.iter().map(|n| bit(n).expect("known capability")).fold(0, |a, b| a | b)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Security {
    /// Capability bounding set for the container's processes.
    pub caps: u64,
    /// Block dangerous syscalls (see `linux/seccomp.rs`).
    pub seccomp: bool,
    pub no_new_privs: bool,
}

impl Default for Security {
    fn default() -> Self {
        Security { caps: mask(&DEFAULT_CAPS), seccomp: true, no_new_privs: false }
    }
}

/// Docker semantics: start from the default set, apply `--cap-drop`, then `--cap-add`
/// (so `--cap-drop ALL --cap-add X` gives exactly X). `--privileged` = everything, no seccomp.
pub fn resolve(cfg: &Config) -> Result<Security> {
    let mut s = Security::default();
    for opt in &cfg.security_opt {
        match opt.as_str() {
            "seccomp=unconfined" => s.seccomp = false,
            "no-new-privileges" | "no-new-privileges=true" => s.no_new_privs = true,
            other => return Err(Error::Usage(format!("unsupported --security-opt {other:?}"))),
        }
    }
    let list = |names: &[String]| -> Result<u64> {
        names.iter().try_fold(0u64, |acc, n| Ok(acc | if n.eq_ignore_ascii_case("all") { u64::MAX } else { bit(n)? }))
    };
    s.caps &= !list(&cfg.cap_drop)?;
    s.caps |= list(&cfg.cap_add)?;
    if cfg.privileged {
        s.caps = u64::MAX;
        s.seccomp = false;
    }
    Ok(s)
}

// ---- seccomp BPF program (classic BPF, as the kernel's `struct sock_filter`)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

const LD_ABS_W: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K
pub const RET_KILL: u32 = 0x8000_0000; // SECCOMP_RET_KILL_PROCESS
pub const RET_ALLOW: u32 = 0x7fff_0000;
pub const RET_EPERM: u32 = 0x0005_0001; // SECCOMP_RET_ERRNO | EPERM

/// Allow everything except `denied` syscall numbers (which fail with EPERM).
/// A call made with a different ABI than `arch` (e.g. 32-bit on a 64-bit kernel) kills the process,
/// since the numbers would mean something else.
pub fn deny_program(denied: &[u32], arch: u32) -> Vec<Insn> {
    let n = denied.len();
    let mut p = vec![
        Insn { code: LD_ABS_W, jt: 0, jf: 0, k: 4 }, // seccomp_data.arch
        Insn { code: JEQ_K, jt: 1, jf: 0, k: arch },
        Insn { code: RET_K, jt: 0, jf: 0, k: RET_KILL },
        Insn { code: LD_ABS_W, jt: 0, jf: 0, k: 0 }, // seccomp_data.nr
    ];
    for (i, nr) in denied.iter().enumerate() {
        // on a match, skip the remaining checks and RET_ALLOW to land on RET_EPERM
        p.push(Insn { code: JEQ_K, jt: (n - i) as u8, jf: 0, k: *nr });
    }
    p.push(Insn { code: RET_K, jt: 0, jf: 0, k: RET_ALLOW });
    p.push(Insn { code: RET_K, jt: 0, jf: 0, k: RET_EPERM });
    p
}

/// Tiny interpreter for exactly the opcodes `deny_program` emits (used by the tests).
pub fn run_program(p: &[Insn], arch: u32, nr: u32) -> u32 {
    let (mut pc, mut acc) = (0usize, 0u32);
    loop {
        let i = p[pc];
        match i.code {
            LD_ABS_W => acc = if i.k == 4 { arch } else { nr },
            JEQ_K => pc += if acc == i.k { i.jt } else { i.jf } as usize,
            RET_K => return i.k,
            c => panic!("unexpected opcode {c:#x}"),
        }
        pc += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(add: &[&str], drop: &[&str]) -> Config {
        Config {
            cap_add: add.iter().map(|s| s.to_string()).collect(),
            cap_drop: drop.iter().map(|s| s.to_string()).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn default_caps_match_dockers() {
        assert_eq!(Security::default().caps, 0xa80425fb); // the well-known CapEff of a Docker container
    }

    #[test]
    fn add_drop_semantics() {
        let base = Security::default().caps;
        assert_eq!(resolve(&cfg(&[], &["ALL"])).unwrap().caps, 0);
        assert_eq!(resolve(&cfg(&["net_bind_service"], &["all"])).unwrap().caps, 1 << 10);
        assert_eq!(resolve(&cfg(&["CAP_NET_ADMIN"], &[])).unwrap().caps, base | 1 << 12);
        assert_eq!(resolve(&cfg(&[], &["chown", "KILL"])).unwrap().caps, base & !1 & !(1 << 5));
        assert!(resolve(&cfg(&["nope"], &[])).is_err());
    }

    #[test]
    fn privileged_and_security_opts() {
        let p = resolve(&Config { privileged: true, ..Config::default() }).unwrap();
        assert!(p.caps == u64::MAX && !p.seccomp);
        let s = resolve(&Config { security_opt: vec!["seccomp=unconfined".into(), "no-new-privileges".into()], ..Config::default() }).unwrap();
        assert!(!s.seccomp && s.no_new_privs);
        assert!(resolve(&Config { security_opt: vec!["apparmor=x".into()], ..Config::default() }).is_err());
    }

    #[test]
    fn every_capability_name_resolves() {
        for (i, n) in CAP_NAMES.iter().enumerate() {
            assert_eq!(bit(n).unwrap(), 1 << i);
        }
    }

    #[test]
    fn bpf_program_denies_only_listed_syscalls() {
        const ARCH: u32 = 0xC000_00B7;
        let p = deny_program(&[100, 200, 300], ARCH);
        for nr in [100, 200, 300] {
            assert_eq!(run_program(&p, ARCH, nr), RET_EPERM, "nr {nr}");
        }
        for nr in [0, 99, 101, 299, 301] {
            assert_eq!(run_program(&p, ARCH, nr), RET_ALLOW, "nr {nr}");
        }
        assert_eq!(run_program(&p, 0x4000_0003, 100), RET_KILL); // wrong ABI
        assert_eq!(run_program(&deny_program(&[], ARCH), ARCH, 5), RET_ALLOW);
    }
}
