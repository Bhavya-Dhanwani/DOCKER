//! Windows-only end-to-end: run a command under a Job Object, check exit code and cleanup.
#![cfg(windows)]
use docker_core::{cli, platform, registry};

fn cfg(args: &str) -> cli::Config {
    match cli::parse(args.split_whitespace().map(String::from)).unwrap() {
        cli::Command::Run(c) => c,
        other => panic!("not a run command: {other:?}"),
    }
}

#[test]
fn exit_code_propagates_and_registry_is_cleaned() {
    let code = platform::run(&cfg("run --mem 64 --cpu 50 . cmd /c exit 3")).unwrap();
    assert_eq!(code, 3);
    let e = registry::list().into_iter().rev().find(|e| e.cmd.last().map(String::as_str) == Some("3")).unwrap();
    assert_eq!(e.exit_code, Some(3)); // record is kept with its exit code
    registry::delete(&e.id);
}

#[test]
fn procs_limit_blocks_extra_processes_and_temp_is_isolated() {
    // control: without a limit cmd can start a child and TEMP points at the scratch dir
    assert_eq!(platform::run(&cfg("run . cmd /c ping -n 1 127.0.0.1")).unwrap(), 0);
    assert_eq!(platform::run(&cfg(r#"run . cmd /c if not "%TEMP%"=="" if exist %TEMP%\ exit 7"#)).unwrap(), 7);
    // limit 1: cmd itself is the only allowed process, so it cannot start ping
    assert_ne!(platform::run(&cfg("run --procs 1 . cmd /c ping -n 1 127.0.0.1")).unwrap(), 0);
}

#[test]
fn stats_and_stop_work_on_a_live_container() {
    let t = std::thread::spawn(|| platform::run(&cfg("run --mem 64 . ping -n 30 127.0.0.1")));
    let id = loop {
        assert!(!t.is_finished(), "runner exited early: {:?}", t.join().unwrap());
        if let Some(e) = registry::list().into_iter().find(|e| e.exit_code.is_none() && e.cmd.first().is_some_and(|c| c == "ping")) {
            break e.id;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(platform::stats(&id).unwrap().procs >= 1);
    platform::stop(&id).unwrap();
    assert_eq!(t.join().unwrap().unwrap(), 1);
    assert!(platform::stats(&id).is_none());
    registry::delete(&id);
}
