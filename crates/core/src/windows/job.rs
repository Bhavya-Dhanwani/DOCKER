//! Named Win32 Job Object: hard memory/CPU caps and kill-on-close. All `unsafe` lives here.
use crate::{cli::Config, error::{Error, Result}, stats::Stats};
use std::{mem::{size_of, zeroed}, ptr::{null, null_mut}};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{JobObjects::*, SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE}},
};

pub struct Job(HANDLE);

pub fn job_name(id: &str) -> String {
    format!("rust-docker-{id}")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn set<T>(h: HANDLE, class: JOBOBJECTINFOCLASS, info: &T, what: &str) -> Result<()> {
    let ok = unsafe { SetInformationJobObject(h, class, (info as *const T).cast(), size_of::<T>() as u32) };
    if ok == 0 { Err(Error::sys(what)) } else { Ok(()) }
}

fn query<T>(h: HANDLE, class: JOBOBJECTINFOCLASS) -> Option<T> {
    let mut out: T = unsafe { zeroed() };
    let ok = unsafe { QueryInformationJobObject(h, class, (&mut out as *mut T).cast(), size_of::<T>() as u32, null_mut()) };
    (ok != 0).then_some(out)
}

impl Job {
    pub fn create(id: &str, cfg: &Config) -> Result<Job> {
        let h = unsafe { CreateJobObjectW(null(), wide(&job_name(id)).as_ptr()) };
        if h.is_null() {
            return Err(Error::sys("CreateJobObject"));
        }
        let job = Job(h); // Drop closes it from here on

        let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        ext.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(n) = cfg.procs {
            ext.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            ext.BasicLimitInformation.ActiveProcessLimit = n;
        }
        if let Some(m) = cfg.mem_bytes {
            ext.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            ext.JobMemoryLimit = m as usize;
        }
        set(h, JobObjectExtendedLimitInformation, &ext, "set job limits")?;

        // no touching the host desktop: clipboard, display settings, global atoms, other windows, logoff
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_EXITWINDOWS,
        };
        set(h, JobObjectBasicUIRestrictions, &ui, "set job ui limits")?;

        if let Some(p) = cfg.cpu_percent {
            let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { zeroed() };
            cpu.ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            cpu.Anonymous.CpuRate = p * 100; // 1/100 of a percent
            set(h, JobObjectCpuRateControlInformation, &cpu, "set job cpu cap")?;
        }
        Ok(job)
    }

    /// Open a running container's job by id (None once it has exited).
    pub fn open(id: &str) -> Option<Job> {
        let h = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, wide(&job_name(id)).as_ptr()) };
        (!h.is_null()).then_some(Job(h))
    }

    pub fn assign(&self, process: HANDLE) -> Result<()> {
        if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
            Err(Error::sys("AssignProcessToJobObject"))
        } else {
            Ok(())
        }
    }

    pub fn terminate(&self) -> Result<()> {
        if unsafe { TerminateJobObject(self.0, 1) } == 0 { Err(Error::sys("TerminateJobObject")) } else { Ok(()) }
    }

    pub fn stats(&self) -> Option<Stats> {
        let acct: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = query(self.0, JobObjectBasicAccountingInformation)?;
        let ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = query(self.0, JobObjectExtendedLimitInformation)?;
        Some(Stats {
            mem_bytes: ext.PeakJobMemoryUsed as u64,
            cpu_ms: (acct.TotalUserTime + acct.TotalKernelTime) as u64 / 10_000, // 100ns ticks
            procs: acct.ActiveProcesses,
            disk_bytes: 0,
        })
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
