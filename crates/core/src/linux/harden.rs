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
/// Home directory of `uid` from the text of /etc/passwd; `/` when there is no entry (like Docker).
fn home_of(passwd: &str, uid: u32) -> String {
    passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 6 && f[2].parse() == Ok(uid)).then(|| f[5].to_string())
        })
        .next()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "/".into())
}

pub fn finalize(user: &str, sec: &Security) -> Result<()> {
    // before the capability set shrinks: raising a hard limit needs CAP_SYS_RESOURCE
    for &(res, soft, hard) in &sec.ulimits {
        let lim = libc::rlimit { rlim_cur: soft, rlim_max: hard };
        if unsafe { libc::setrlimit(res as _, &lim) } < 0 {
            return Err(Error::sys(format!("set resource limit {res}")));
        }
    }
    if sec.seccomp {
        install_seccomp()?;
    }
    drop_bounding(sec.caps)?;
    // resolved inside the container's own root: its /etc/passwd decides what a name means
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let uid = if user.is_empty() { 0 } else { crate::user::resolve(user, &passwd, &fs::read_to_string("/etc/group").unwrap_or_default())?.uid };
    // tools such as Go, .NET and npm refuse to run without a home directory
    if std::env::var_os("HOME").is_none() {
        // SAFETY: the container's init is single-threaded here, just before exec
        unsafe { std::env::set_var("HOME", home_of(&passwd, uid)) };
    }
    if !user.is_empty() {
        let group = fs::read_to_string("/etc/group").unwrap_or_default();
        let ids = crate::user::resolve(user, &passwd, &group)?;
        if crate::registry::rootless() && super::rootless::single_map() && (ids.uid != 0 || ids.gid != 0) {
            // only id 0 exists in this user namespace; the process stays "root" (= the calling user)
            super::rootless::warn(&format!("USER {user} ignored: single-uid mapping has no uid {}", ids.uid));
        } else {
            sys::become_user(ids.uid, ids.gid)?;
        }
    }
    if sec.no_new_privs && unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        return Err(Error::sys("set no_new_privs"));
    }
    Ok(())
}

#[cfg(test)]
mod home_tests {
    use super::home_of;

    #[test]
    fn home_comes_from_passwd_or_is_root_of_the_filesystem() {
        let p = "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1000::/home/node:/bin/sh\nnobody:x:65534:65534:::/sbin/nologin\n";
        assert_eq!(home_of(p, 0), "/root");
        assert_eq!(home_of(p, 1000), "/home/node");
        assert_eq!(home_of(p, 65534), "/");
        assert_eq!(home_of(p, 4242), "/");
    }
}
