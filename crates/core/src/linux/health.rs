//! Container health checks. The monitor runs in its own helper process (never a thread of the supervisor,
//! which must stay single-threaded so it can safely fork the container, including on restarts).
use crate::registry;
use docker_image::Health;
use std::{
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

/// Run one probe inside the container (`docker exec <id> <test...>`), killed after `timeout_s`.
fn probe(exe: &std::path::Path, id: &str, test: &[String], timeout_s: u64) -> bool {
    let Ok(mut child) = Command::new(exe).arg("exec").arg(id).args(test).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The next status given the result of a probe. `consecutive_failures` counts failures that already count.
pub fn next_status(ok: bool, in_start_period: bool, failures: &mut u32, retries: u32, current: &str) -> String {
    if ok {
        *failures = 0;
        return "healthy".into();
    }
    if in_start_period {
        return current.into(); // failures during the start period do not count
    }
    *failures += 1;
    if *failures >= retries { "unhealthy".into() } else { current.into() }
}

/// Never returns: loops until the container's record says it is gone.
pub fn monitor(id: &str, h: &Health) -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| "docker".into());
    let started = Instant::now();
    let mut failures = 0;
    let mut status = "starting".to_string();
    let _ = registry::update(id, |e| e.health = Some("starting".into()));
    loop {
        sleep(Duration::from_secs(h.interval_s.max(1)));
        match registry::find(id) {
            Ok(e) if e.exit_code.is_none() => {}
            _ => std::process::exit(0), // the container was removed or finished for good
        }
        let ok = probe(&exe, id, &h.test, h.timeout_s);
        let next = next_status(ok, started.elapsed() < Duration::from_secs(h.start_period_s), &mut failures, h.retries, &status);
        if next != status {
            status = next;
            let s = status.clone();
            let _ = registry::update(id, |e| e.health = Some(s));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_transitions() {
        let mut f = 0;
        assert_eq!(next_status(true, false, &mut f, 3, "starting"), "healthy");
        assert_eq!(next_status(false, false, &mut f, 3, "healthy"), "healthy"); // 1 failure
        assert_eq!(next_status(false, false, &mut f, 3, "healthy"), "healthy"); // 2
        assert_eq!(next_status(false, false, &mut f, 3, "healthy"), "unhealthy"); // 3 in a row
        assert_eq!(next_status(true, false, &mut f, 3, "unhealthy"), "healthy"); // recovers, streak resets
        assert_eq!(f, 0);
        // failures in the start period do not count
        let mut f = 0;
        for _ in 0..5 {
            assert_eq!(next_status(false, true, &mut f, 2, "starting"), "starting");
        }
        assert_eq!(f, 0);
    }
}
