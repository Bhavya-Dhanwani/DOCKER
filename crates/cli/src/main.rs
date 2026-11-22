mod api;
mod commands;
mod compose;
mod swarm;
mod vm;

use docker_core::cli::{self, Command};
use docker_core::platform;
use std::{error::Error, process::ExitCode};

/// Commands that end with the container's own exit code.
fn exit_with(r: Result<i32, Box<dyn Error>>) -> Result<ExitCode, Box<dyn Error>> {
    r.map(|c| ExitCode::from(c as u8))
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `docker --native ...`: use the Windows Job Object backend instead of the Linux VM
    let native = args.first().is_some_and(|a| a == "--native");
    if native {
        args.remove(0);
        if !cfg!(windows) {
            eprintln!("docker: --native is only available on Windows");
            return ExitCode::from(2);
        }
    }
    // `docker --rootless ...`: unprivileged mode (also selected automatically for any non-root Linux user)
    if args.first().is_some_and(|a| a == "--rootless") {
        args.remove(0);
        docker_core::registry::set_rootless(true);
    }
    let cmd = match cli::parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("docker: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    let ok = |r: Result<(), Box<dyn Error>>| r.map(|()| ExitCode::SUCCESS);
    // Windows without WSL2: keep working in native mode instead of failing on every command
    let fallback = if native { None } else { vm::native_reason() };
    let result = match cmd {
        Command::Vm(v) => ok(vm::manage(&v)),
        Command::Version => ok(commands::version()),
        // Windows and macOS run Linux containers inside the VM, unless there is none (Windows without WSL2)
        cmd if !cfg!(target_os = "linux") && !native && fallback.is_none() => exit_with(vm::forward(cmd)),
        // native Windows mode: only a folder can be "run" (as a Job Object); an image needs a Linux engine
        Command::Run(cfg) if cfg!(windows) && !std::path::Path::new(&cfg.target).is_dir() => Err(format!(
            "{:?} is an image, and images need a Linux engine (none is available: {})",
            cfg.target,
            fallback.as_deref().unwrap_or("--native was given")
        )
        .into()),
        Command::Run(cfg) => {
            if cfg!(target_os = "linux") {
                if let Err(e) = commands::ensure_image(&cfg.target) {
                    eprintln!("docker: {e}");
                    return ExitCode::FAILURE;
                }
            }
            exit_with(platform::run(&cfg).map_err(Into::into))
        }
        Command::Exec { id, cmd, tty } => exit_with(commands::exec(&id, &cmd, tty)),
        Command::Ps { all } => ok(commands::ps(all)),
        Command::Logs { follow, id } => ok(commands::logs(follow, &id)),
        Command::Stop { id, timeout } => ok(commands::stop(&id, timeout)),
        Command::Restart { id, timeout } => ok(commands::restart(&id, timeout)),
        Command::Start(id) => ok(commands::start(&id)),
        Command::Kill(id) => ok(commands::kill(&id)),
        Command::Update { id, mem_mb, cpu, procs } => ok(commands::update(&id, mem_mb, cpu, procs)),
        Command::Pause(id) => ok(commands::pause(&id, true)),
        Command::Unpause(id) => ok(commands::pause(&id, false)),
        Command::Wait(id) => ok(commands::wait(&id)),
        Command::Top(id) => ok(commands::top(&id)),
        Command::Rename { old, new } => ok(commands::rename(&old, &new)),
        Command::Stats { no_stream, json, ids } => ok(commands::stats(no_stream, json, &ids)),
        Command::Cp { src, dst } => ok(commands::cp(&src, &dst)),
        Command::Volume(v) => ok(commands::volume(&v)),
        Command::Network(n) => ok(commands::network(&n)),
        Command::Compose(c) => ok(compose::run(&c)),
        Command::Daemon { host } => ok(api::serve(host.as_deref())),
        Command::SystemDf => ok(commands::system_df()),
        Command::ContainerPrune => ok(commands::container_prune()),
        Command::Rm { force, id } => ok(commands::rm(force, &id)),
        Command::Inspect(id) => ok(commands::inspect(&id)),
        Command::Login { server, user, password } => ok(commands::login(server.as_deref(), &user, password)),
        Command::Logout(server) => ok(commands::logout(server.as_deref())),
        Command::Push(n) => ok(commands::push(&n)),
        Command::Tag { src, dst } => ok(commands::tag(&src, &dst)),
        Command::Build(args) => ok(commands::build(&args)),
        Command::Save { out, images } => ok(commands::save(&out, &images)),
        Command::Load { input } => ok(commands::load(&input)),
        Command::Prune => ok(commands::prune()),
        Command::Pull(n) => ok(commands::pull(&n)),
        Command::Images => ok(commands::images()),
        Command::Rmi(n) => ok(commands::rmi(&n)),
        Command::Swarm(words) => ok(swarm::run(&words)),
        Command::PullPlatform { image, platform } => ok(commands::pull_platform(&image, Some(&platform))),
        Command::Search(t) => ok(commands::search(&t)),
        Command::History(n) => ok(commands::history(&n)),
        Command::ImageInspect(n) => ok(commands::image_inspect(&n)),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("docker: {e}");
            if fallback.is_some() || native {
                eprintln!("
{}", vm::NATIVE_HINT);
            }
            ExitCode::FAILURE
        }
    }
}
