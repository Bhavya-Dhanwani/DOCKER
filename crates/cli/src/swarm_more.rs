//! The rest of the single-node swarm surface: `docker node ...`, `docker secret ...`, `docker config ...`,
//! `docker service rollback`, the extra `service create/update` flags (`--secret`, `--config`, `--mount`, `--label`, `--user`,
//! `--limit-memory`, ...), `service ls --format`, `stack config`. (Child module of `swarm`.)
use super::*;
use crate::{extra, format};
use docker_core::cli::Volume;
use serde_json::json;

// ---- secrets and configs: files in the swarm's data directory, mounted read-only into the replicas

fn kind_dir(kind: &str) -> PathBuf {
    dir().join(kind)
}

/// Where the data of swarm secret/config `name` is stored (None when there is none of that name).
pub fn object_path(kind: &str, name: &str) -> Option<PathBuf> {
    let p = kind_dir(kind).join(name).join("data");
    p.is_file().then_some(p)
}

fn object_meta(kind: &str, name: &str) -> Option<serde_json::Value> {
    serde_json::from_slice(&fs::read(kind_dir(kind).join(name).join("meta.json")).ok()?).ok()
}

fn object_names(kind: &str) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(kind_dir(kind)).map(|rd| rd.flatten().filter(|e| e.path().join("data").is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// `docker secret|config create|ls|inspect|rm`.
pub fn object_cmd(kind: &str, args: &[String]) -> Res {
    let (what, plural) = (if kind == "secrets" { "secret" } else { "config" }, kind);
    match args.first().map(String::as_str) {
        Some("create") => {
            let f = extra::flags(&args[1..], &["d"], &["l", "label", "driver", "template-driver"], false)?;
            let [name, src] = f.pos.as_slice() else { return Err(format!("usage: docker {what} create [-l key=value] NAME FILE|-").into()) };
            if !is_volume_name(name) {
                return Err(format!("a {what} name must match [a-zA-Z0-9][a-zA-Z0-9_.-]*").into());
            }
            with_state(|_| Ok(()))?; // there has to be a swarm
            let _guard = lock()?;
            if object_path(plural, name).is_some() {
                return Err(format!("{what} {name} already exists").into());
            }
            let data = if src == "-" {
                let mut b = vec![];
                std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut b)?;
                b
            } else {
                fs::read(src).map_err(|e| format!("{src}: {e}"))?
            };
            let d = kind_dir(plural).join(name);
            fs::create_dir_all(&d)?;
            fs::write(d.join("data"), &data)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(d.join("data"), fs::Permissions::from_mode(0o444))?; // readable by the service's user, whoever it runs as
            }
            let id = registry::new_id()[..25.min(registry::new_id().len())].to_string();
            let meta = json!({ "ID": id, "Name": name, "CreatedAt": registry::now_unix(), "Size": data.len(), "Labels": f.all(&["l", "label"]) });
            fs::write(d.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
            println!("{id}");
            Ok(())
        }
        Some("ls" | "list") => {
            let f = extra::flags(&args[1..], &["q", "quiet"], &["format", "filter"], false)?;
            with_state(|_| Ok(()))?;
            let now = registry::now_unix();
            let rows: Vec<serde_json::Value> = object_names(plural)
                .iter()
                .filter_map(|n| object_meta(plural, n))
                .map(|m| {
                    let age = now.saturating_sub(m["CreatedAt"].as_u64().unwrap_or(now));
                    json!({ "ID": m["ID"], "Name": m["Name"], "CreatedAt": m["CreatedAt"], "CreatedSince": format!("{} ago", extra::human_duration(age)), "UpdatedAt": m["CreatedAt"], "Labels": "" })
                })
                .collect();
            if f.has(&["q", "quiet"]) {
                for r in &rows {
                    println!("{}", r["ID"].as_str().unwrap_or(""));
                }
                return Ok(());
            }
            let t = f.get(&["format"]).cloned().unwrap_or_else(|| "table {{.ID}}\t{{.Name}}\t{{.CreatedSince}}\t{{.CreatedSince}}".into());
            extra::print_rows(&t, &rows, &[("CreatedSince", "CREATED")])
        }
        Some("inspect") => {
            let f = extra::flags(&args[1..], &["pretty"], &["f", "format"], false)?;
            let mut found = vec![];
            for n in &f.pos {
                match object_meta(plural, n) {
                    Some(m) => found.push(json!({ "ID": m["ID"], "Spec": { "Name": n, "Labels": {} }, "Version": { "Index": 1 }, "CreatedAt": m["CreatedAt"] })),
                    None => return Err(format!("{what} {n} not found").into()),
                }
            }
            match f.get(&["f", "format"]) {
                Some(t) => extra::print_rows(t, &found, &[]),
                None => {
                    println!("{}", serde_json::to_string_pretty(&found)?);
                    Ok(())
                }
            }
        }
        Some("rm" | "remove") if args.len() > 1 => {
            let in_use: Vec<String> = load().map(|st| st.services.values().filter(|s| s.cfg.volumes.iter().any(|v| v.host.contains(&format!("/{plural}/")))).map(|s| s.name.clone()).collect()).unwrap_or_default();
            for n in &args[1..] {
                if object_path(plural, n).is_none() {
                    return Err(format!("{what} {n} not found").into());
                }
                let used = load().is_some_and(|st| st.services.values().any(|s| s.cfg.volumes.iter().any(|v| v.host == object_path(plural, n).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default())));
                if used {
                    return Err(format!("{what} {n} is in use by service {}", in_use.first().cloned().unwrap_or_default()).into());
                }
                fs::remove_dir_all(kind_dir(plural).join(n))?;
                println!("{n}");
            }
            Ok(())
        }
        _ => Err(format!("usage: docker {what} create|ls|inspect|rm").into()),
    }
}

/// `--secret src[,target=x]` / `--config src[:target]`: mount the object read-only into every replica.
fn attach(spec: &mut Spec, kind: &str, v: &str) -> Res {
    let (src, target) = if v.contains("source=") || v.contains("target=") {
        let get = |k: &str| v.split(',').find_map(|p| p.strip_prefix(&format!("{k}=")).map(String::from));
        (get("source").ok_or("--secret needs source=")?, get("target"))
    } else {
        match v.split_once(':') {
            Some((s, t)) => (s.to_string(), Some(t.to_string())),
            None => (v.split(',').next().unwrap_or("").to_string(), None),
        }
    };
    let path = object_path(kind, &src).ok_or_else(|| format!("{} {src} not found: create it with `docker {} create`", kind.trim_end_matches('s'), kind.trim_end_matches('s')))?;
    let dest = match target {
        Some(t) if t.starts_with('/') => t,
        Some(t) => format!("{}{t}", if kind == "secrets" { "/run/secrets/" } else { "/" }),
        None => format!("{}{src}", if kind == "secrets" { "/run/secrets/" } else { "/" }),
    };
    spec.cfg.volumes.retain(|x| x.container != dest);
    spec.cfg.volumes.push(Volume { host: path.to_string_lossy().into_owned(), container: dest, ro: true });
    Ok(())
}

/// `1g`, `512m`, `64M`, plain bytes -> megabytes.
fn size_mb(flag: &str, v: &str) -> Res<u64> {
    docker_core::cli::parse_size_mb(v).ok_or_else(|| format!("{flag} needs a size like 512m").into())
}

/// More service flags. Returns Ok(false) when `flag` is not one of them. (`create`: only valid at creation.)
pub(super) fn apply_extra(spec: &mut Spec, flag: &str, v: &str, create: bool) -> Res<bool> {
    match flag {
        "--secret" => attach(spec, "secrets", v)?,
        "--secret-add" if !create => attach(spec, "secrets", v)?,
        "--secret-rm" if !create => spec.cfg.volumes.retain(|x| x.container != format!("/run/secrets/{v}")),
        "--config" => attach(spec, "configs", v)?,
        "--config-add" if !create => attach(spec, "configs", v)?,
        "--config-rm" if !create => spec.cfg.volumes.retain(|x| x.container != format!("/{v}")),
        "-l" | "--label" | "--container-label" | "--label-add" | "--container-label-add" => spec.cfg.labels.push(v.to_string()),
        "--label-rm" | "--container-label-rm" => spec.cfg.labels.retain(|l| l.split('=').next() != Some(v)),
        "-u" | "--user" => spec.cfg.user = Some(v.to_string()),
        "-w" | "--workdir" => spec.cfg.workdir = Some(v.to_string()),
        "--entrypoint" => spec.cfg.entrypoint = Some(v.to_string()),
        "--hostname" => spec.cfg.hostname = Some(v.to_string()),
        "--dns" | "--dns-add" => spec.cfg.dns.push(v.to_string()),
        "--cap-add" => spec.cfg.cap_add.push(v.to_string()),
        "--cap-drop" => spec.cfg.cap_drop.push(v.to_string()),
        "--env-file" => spec.cfg.env.extend(docker_core::env::parse_env_file(&fs::read_to_string(v).map_err(|e| format!("--env-file {v}: {e}"))?)),
        "--limit-memory" | "--limit-memory-reservation" if flag == "--limit-memory" => spec.cfg.mem_bytes = Some(size_mb(flag, v)? << 20),
        "--limit-cpu" => {
            let c: f64 = v.parse().ok().filter(|c: &f64| *c > 0.0).ok_or("--limit-cpu needs a number of CPUs")?;
            spec.cfg.cpu_percent = Some(((c * 100.0).round() as u32).max(1));
        }
        "--limit-pids" | "--pids-limit" => spec.cfg.procs = Some(v.parse().map_err(|_| "--limit-pids needs a number")?),
        "--mount" => {
            let get = |k: &str| v.split(',').find_map(|p| p.strip_prefix(&format!("{k}=")).map(String::from));
            let ro = v.split(',').any(|p| p == "readonly" || p == "ro" || p == "readonly=true" || p == "ro=true");
            let target = get("target").or_else(|| get("destination")).or_else(|| get("dst")).ok_or("--mount needs target=")?;
            let source = get("source").or_else(|| get("src")).ok_or("--mount needs source=")?;
            spec.cfg.volumes.retain(|x| x.container != target);
            spec.cfg.volumes.push(Volume { host: source, container: target, ro });
        }
        "--mount-rm" if !create => spec.cfg.volumes.retain(|x| x.container != v),
        "--mode" if create => {
            if !matches!(v, "replicated" | "global") {
                return Err("--mode must be replicated or global".into());
            }
            if v == "global" {
                spec.replicas = 1; // one node: one task
            }
        }
        // accepted: scheduling hints, registry auth, rolling-update tuning and the like that need more than one node or a registry session
        "--constraint" | "--constraint-add" | "--constraint-rm" | "--placement-pref" | "--placement-pref-add" | "--placement-pref-rm" | "--endpoint-mode"
        | "--stop-grace-period" | "--stop-signal" | "--update-failure-action" | "--update-max-failure-ratio" | "--update-monitor" | "--update-order" | "--rollback-delay"
        | "--rollback-failure-action" | "--rollback-max-failure-ratio" | "--rollback-monitor" | "--rollback-order" | "--rollback-parallelism" | "--restart-delay"
        | "--restart-max-attempts" | "--restart-window" | "--log-driver" | "--log-opt" | "--reserve-cpu" | "--reserve-memory" | "--limit-memory-reservation"
        | "--health-timeout" | "--health-retries" | "--health-start-period" | "--health-start-interval" | "--isolation" | "--generic-resource" | "--network-add" | "--network-rm"
        | "--publish-add" | "--publish-rm" | "--replicas-max-per-node" | "--credential-spec" | "--runtime" | "--sysctl" | "--ulimit" | "--ip" => {}
        _ => return Ok(false),
    }
    Ok(true)
}

/// Flags that take no value.
pub(super) fn is_bool_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-d" | "--detach" | "-q" | "--quiet" | "--init" | "--read-only" | "--with-registry-auth" | "--no-resolve-image" | "--no-healthcheck" | "--force" | "--tty" | "-t" | "--no-trunc"
            | "--update-failure-action-skip" | "--no-hostname" | "--rollback" | "--stdin" | "-i" | "--resolve-image-skip"
    )
}

pub(super) fn apply_bool(spec: &mut Spec, flag: &str) {
    match flag {
        "--init" => spec.cfg.init = true,
        "--read-only" => spec.cfg.read_only = true,
        "-t" | "--tty" => spec.cfg.tty = true,
        "-i" | "--stdin" => spec.cfg.interactive = true,
        "--no-healthcheck" => spec.cfg.no_healthcheck = true,
        _ => {}
    }
}

// ---- nodes (this machine is the only one)

fn node_row(st: &State) -> serde_json::Value {
    let host = std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()).unwrap_or_else(|_| "node".into());
    json!({
        "ID": st.id, "Hostname": host, "Status": "Ready", "Availability": if st.availability.is_empty() { "active" } else { st.availability.as_str() },
        "ManagerStatus": "Leader", "EngineVersion": env!("CARGO_PKG_VERSION"), "TLSStatus": "Ready", "Self": true,
    })
}

fn node_matches(st: &State, q: &str) -> bool {
    q == "self" || st.id.starts_with(q) || node_row(st)["Hostname"] == q
}

pub fn node_cmd(args: &[String]) -> Res {
    let st = load().ok_or(NOT_ACTIVE)?;
    match args.first().map(String::as_str) {
        Some("ls" | "list") => {
            let f = extra::flags(&args[1..], &["q", "quiet"], &["format", "filter"], false)?;
            let row = node_row(&st);
            if f.has(&["q", "quiet"]) {
                println!("{}", st.id);
                return Ok(());
            }
            let t = f.get(&["format"]).cloned().unwrap_or_else(|| "table {{.ID}} *\t{{.Hostname}}\t{{.Status}}\t{{.Availability}}\t{{.ManagerStatus}}\t{{.EngineVersion}}".into());
            extra::print_rows(&t, &[row], &[("ID", "ID"), ("EngineVersion", "ENGINE VERSION"), ("ManagerStatus", "MANAGER STATUS")])
        }
        Some("inspect") => {
            let f = extra::flags(&args[1..], &["pretty"], &["f", "format"], false)?;
            let mut out = vec![];
            for q in &f.pos {
                if !node_matches(&st, q) {
                    return Err(format!("Error: No such node: {q}").into());
                }
                let r = node_row(&st);
                out.push(json!({
                    "ID": st.id, "Version": { "Index": 1 }, "Spec": { "Role": "manager", "Availability": r["Availability"] },
                    "Description": { "Hostname": r["Hostname"], "Engine": { "EngineVersion": r["EngineVersion"] } },
                    "Status": { "State": "ready", "Addr": "127.0.0.1" }, "ManagerStatus": { "Leader": true, "Reachability": "reachable" },
                }));
            }
            match f.get(&["f", "format"]) {
                Some(t) => extra::print_rows(t, &out, &[]),
                None => {
                    println!("{}", serde_json::to_string_pretty(&out)?);
                    Ok(())
                }
            }
        }
        Some("ps") => {
            let names: Vec<String> = services(None)?.into_iter().map(|s| s.name).collect();
            service_ps(&names.iter().map(String::as_str).collect::<Vec<_>>())
        }
        Some("update") => {
            let f = extra::flags(&args[1..], &[], &["availability", "role", "label-add", "label-rm"], false)?;
            let q = f.pos.first().ok_or("usage: docker node update [options] NODE")?;
            if !node_matches(&st, q) {
                return Err(format!("Error: No such node: {q}").into());
            }
            if let Some(a) = f.get(&["availability"]) {
                if !matches!(a.as_str(), "active" | "pause" | "drain") {
                    return Err("--availability must be active, pause or drain".into());
                }
                with_state(|st| {
                    st.availability = a.clone();
                    Ok(())
                })?;
                ensure_supervisor()?;
            }
            println!("{q}");
            Ok(())
        }
        Some("promote" | "demote") => Err("this node is the only manager: there is nothing to promote or demote".into()),
        Some("rm" | "remove") => Err("cannot remove the node you are on (use `docker swarm leave`)".into()),
        _ => Err("usage: docker node ls|inspect|ps|update".into()),
    }
}

// ---- services

pub(super) fn rollback(name: &str) -> Res {
    with_state(|st| {
        let spec = st.services.get_mut(name).ok_or_else(|| format!("no such service: {name}"))?;
        let prev = spec.prev.take().ok_or_else(|| format!("service {name} has no previous version to roll back to"))?;
        let current = Spec { prev: None, ..spec.clone() };
        *spec = Spec { prev: Some(Box::new(current)), ..*prev };
        Ok(())
    })?;
    ensure_supervisor()?;
    println!("{name}\nrollback: manually requested rollback");
    Ok(())
}

pub(super) fn service_ls(stack: Option<&str>, args: &[String]) -> Res {
    let f = extra::flags(args, &["q", "quiet"], &["format", "filter"], false)?;
    let filters = extra::parse_filters(&f.all(&["filter"]));
    let mut rows = vec![];
    for s in services(stack)? {
        if !filters.iter().all(|(k, v)| match k.as_str() {
            "name" => s.name.contains(v.as_str()),
            "label" => s.cfg.labels.iter().any(|l| l == v || l.split('=').next() == Some(v.as_str())),
            _ => true,
        }) {
            continue;
        }
        let up = tasks_of(&s.name).iter().filter(|(_, e)| e.running()).count();
        let ports: Vec<String> = s.cfg.ports.iter().map(|p| format!("*:{}->{}/tcp", p.host, p.container)).collect();
        rows.push(json!({ "ID": s.name.clone(), "Name": s.name, "Mode": "replicated", "Replicas": format!("{up}/{}", s.replicas), "Image": s.cfg.target, "Ports": ports.join(", ") }));
    }
    if f.has(&["q", "quiet"]) {
        for r in &rows {
            println!("{}", r["ID"].as_str().unwrap_or(""));
        }
        return Ok(());
    }
    let t = f.get(&["format"]).cloned().unwrap_or_else(|| "table {{.ID}}\t{{.Name}}\t{{.Mode}}\t{{.Replicas}}\t{{.Image}}\t{{.Ports}}".into());
    print!("{}", format::render_rows(&if t == "json" { "{{json .}}".to_string() } else { t }, &rows, &[])?);
    Ok(())
}

/// `docker stack config -c file`: the merged, resolved compose file.
pub fn stack_config(args: &[String]) -> Res {
    let f = extra::flags(args, &["skip-interpolation"], &["c", "compose-file"], false)?;
    let file = f.get(&["c", "compose-file"]).ok_or("usage: docker stack config -c COMPOSE_FILE")?;
    let words = vec!["-f".to_string(), file.clone(), "config".to_string()];
    let parsed = docker_core::cli::parse(["compose".to_string()].into_iter().chain(words))?;
    match parsed {
        Command::Compose(c) => compose::run(&c).map(drop),
        _ => Err("could not read the compose file".into()),
    }
}
