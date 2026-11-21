//! Applied to the container's first process (and `exec`'d ones) right before exec:
//! seccomp filter, capability bounding set, user switch, no-new-privileges.
use super::sys;
use crate::{
    error::{Error, Result},
    security::{deny_program, Security},
};
use std::fs;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

// not defined by libc for every target
#[cfg(target_arch = "x86_64")]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 320;
#[cfg(target_arch = "aarch64")]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 294;

/// Syscalls a container never needs. Most are also gated by capabilities; this is the second lock.
fn denied() -> Vec<u32> {
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
    let mut v = vec![
        libc::SYS_kexec_load,
        SYS_KEXEC_FILE_LOAD,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        libc::SYS_adjtimex,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_userfaultfd,
        libc::SYS_open_by_handle_at,
        libc::SYS_quotactl,
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend([libc::SYS_iopl, libc::SYS_ioperm]);
    v.into_iter().map(|n| n as u32).collect()
}

fn install_seccomp() -> Result<()> {
    let mut prog: Vec<libc::sock_filter> =
        deny_program(&denied(), AUDIT_ARCH).iter().map(|i| libc::sock_filter { code: i.code, jt: i.jt, jf: i.jf, k: i.k }).collect();
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_mut_ptr() };
    let r = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog as *const libc::sock_fprog) };
    if r < 0 { Err(Error::sys("install seccomp filter")) } else { Ok(()) }
}

fn last_cap() -> u32 {
    fs::read_to_string("/proc/sys/kernel/cap_last_cap").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(40)
}

/// Remove every capability not in `keep` from the bounding set: a root process loses them at its next
/// exec, a non-root process never had them. (Needs CAP_SETPCAP, so this runs before the user switch.)
fn drop_bounding(keep: u64) -> Result<()> {
    for cap in 0..=last_cap() {
        if keep >> cap & 1 == 0 && unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) } < 0 {
            return Err(Error::sys(format!("drop capability {cap}")));
        }
    }
    Ok(())
}

/// Order matters: seccomp needs CAP_SYS_ADMIN (so before caps go), the bounding set needs CAP_SETPCAP
/// (before the uid change), and setuid/setgid need to run before no_new_privs.
pub fn finalize(user: &str, sec: &Security) -> Result<()> {
    if sec.seccomp {
        install_seccomp()?;
    }
    drop_bounding(sec.caps)?;
    if !user.is_empty() {
        // resolved inside the container's own root: its /etc/passwd decides what a name means
        let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
        let group = fs::read_to_string("/etc/group").unwrap_or_default();
        let ids = crate::user::resolve(user, &passwd, &group)?;
        sys::become_user(ids.uid, ids.gid)?;
    }
    if sec.no_new_privs && unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        return Err(Error::sys("set no_new_privs"));
    }
    Ok(())
}
