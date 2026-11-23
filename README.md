# Sasta Docker

A Docker replacement written in Rust. It runs real Linux containers, builds and pulls images from Docker Hub,
speaks the Docker Engine API, and has a desktop GUI. Small static binary, no always-running daemon, and
low RAM and disk use.

The command is still called `docker`, so existing scripts and tools keep working. The desktop app is `docker-gui`.

## Install

Installers are built by the `release` workflow (`.github/workflows/release.yml`: run it from the Actions tab, or push a tag like `v0.1.0` to publish a GitHub release):

| OS | Installer | What you get |
|---|---|---|
| **Windows** (x64, arm64) | `SastaDocker-Setup-<arch>.exe` (Inno Setup, per-user, no admin) | `docker` and `docker-gui`, `docker` on your PATH, a Start menu shortcut, and an optional "set up the Linux engine now" step (`docker vm init`, needs WSL2) |
| **Linux** (x86_64, aarch64) | `sasta-docker-<version>-linux-<arch>.tar.gz` | `sudo ./install.sh` puts `docker` and `docker-gui` in `/usr/local/bin` and adds a menu entry |
| **macOS** (Intel, Apple silicon) | `SastaDocker-macos-<arch>.pkg` | `docker` and `docker-gui` in `/usr/local/bin`; needs Lima (`brew install lima`) for the Linux VM |

The command is the same everywhere: `docker run`, `docker compose up`, `docker build`, ... See [Platforms](#platforms) for how each system runs containers.

## What it can do

**Containers**
- `run` (`-d`, `-it`, `--name`, `-e`, `--env-file`, `-v` (also anonymous `-v /data`), `--mount`, `-p`, `-P`, `--network`, `--hostname`/`-h`, `--add-host` (including `host-gateway`), `--mem`, `--cpus`, `--pids-limit`, `--restart`, `--rm`, `--entrypoint`, `-w`, `-u`, `--health-*`, `--cap-add/--cap-drop`, `--security-opt`, `--privileged`, `--label`, `--dns`, `--init`, `--read-only`, `--ulimit`, `--tmpfs`, `--shm-size`, `--volumes-from`, `--stop-signal`, `--stop-timeout`, `--pull`, `--platform`). Options that only tune resources or logging (`--log-driver`, `--memory-swap`, `--oom-kill-disable`, `--cpu-shares`, ...) are accepted and ignored; options this engine cannot honour (`--ipc`, `--pid`, `--device`, `--gpus`, `--sysctl`, `--link`, ...) are accepted with a warning
- `ps`, `images`, `inspect`, `logs`, `exec` take Docker's options and look like Docker's output: `ps -a -q -n --filter name=/status=/label=/ancestor= --no-trunc --format '{{.Names}}'` (Go templates, `table ...`, `json`), `images -q --filter reference=* --digests --format`, `inspect --format '{{.State.Running}}'` (Docker-shaped JSON, for containers, images, volumes and networks), `logs --tail N -f`, `exec -e -w -u -d`
- More commands: `info`, `events` (with `--filter` and `--format`), `create`, `diff`, `commit`, `export`, `import`, `port`, `attach`, `stop -s`, `kill -s`, `system prune`, `network prune`, `builder prune`, `manifest inspect`, `context ls`, `version --format`, and `--help` / `--version`. `diff`, `commit` and `export` need a running container (writes live in RAM and are gone at exit)
- `rm`, `stop`, `start`, `restart`, `kill`, `pause`, `unpause` and `rmi` take several names (`docker rm -f a b c`). Anonymous volumes (`-v /data`) are removed together with the container when it was started with `--rm`
- Like Docker, containers get `HOME` and `PATH`, `/tmp` is ordinary container storage with no small size limit, and on Windows your own environment variables reach `${VAR}` in compose files and `-e VAR`
- `wait`, `top`, `rename`, `rm [-f]`, `cp`, `stats`, `update --mem/--cpu/--procs`, restart policies (`no`, `always`, `unless-stopped`, `on-failure[:n]`) and healthchecks

**Images**
- `pull` from Docker Hub and other registries, `push`, `login`/`logout`, `tag`, `images`, `rmi`, `save`/`load`, `prune`
- `build` from a Dockerfile: multi-stage builds, `.dockerignore`, `--target`, `--build-arg`, `--no-cache`, `COPY --from`, `COPY --chown`, `HEALTHCHECK`, content-hash layer cache, wildcards in `COPY`/`ADD` (`COPY package*.json ./`, `COPY --from=build /app/dist/* ./`)
- Also: `ADD` from a URL or a tar archive, `COPY --chmod`, `RUN --mount` (cache, bind, tmpfs, secret), heredocs, `SHELL`, `ONBUILD`, `VOLUME`, `STOPSIGNAL`, `--label`, `--secret`, `-q`
- `buildx build` is accepted as an alias for `build`
- Pull by digest, `pull --platform`, zstd-compressed layers; push with resumable chunked uploads, retries, and cross-repository layer reuse; credential helpers and `~/.docker/config.json` logins
- `search`, `history`, `image inspect`
- Layers are stored once and shared between images

**Networking and storage**
- A bridge with one veth per container (pure-Rust netlink) and NAT to the internet
- `-p [ip:]host:container[/tcp|udp]` port publishing, host-IP binding and port ranges, `--network bridge|host|none`; the same options in compose (`"5353:53/udp"`, `"127.0.0.1:8080:80"`, a bare `"80"`, the long form with `target`/`published`/`protocol`)
- IPv6 addresses for containers (bridge, NAT66). IPv6 to the internet needs IPv6 on the host: `scripts/e2e-ipv6.sh` checks it where the host has it and skips on a host without (this PC has none, so that part has never run against the real internet)
- The bridge and its firewall rules are removed when the last container exits
- User-defined networks with container-name DNS and aliases, `network create --subnet`, `network connect|disconnect`
- Named volumes and bind mounts: `volume create|ls|rm|inspect|prune`, `network create|ls|rm|inspect`

**Swarm-lite (single node)**
- `swarm init|leave|status`, `service create|ls|ps|inspect|logs|scale|update|rollback|rm` with replicas, restart conditions and rolling updates, `stack deploy|ls|ps|services|rm|config` from a compose file (`deploy:` honoured), round-robin load balancing of published ports across replicas
- `node ls|inspect|ps|update` (this machine is the one node; `--availability drain` stops its tasks), `secret` and `config` (`create|ls|inspect|rm`, mounted into replicas with `--secret`/`--config`, `/run/secrets/<name>`), service options such as `--user`, `--label`, `--mount`, `--limit-memory`, `--limit-cpu`, `--env-file`, `--mode`
- Not supported: more than one node (`swarm join`). A swarm that spans machines needs networking between them and cannot be tested on one PC

**Rootless mode (Linux)**
- `docker --rootless ...`, selected automatically when not running as root: user namespaces (full uid/gid mapping with `newuidmap`, or a single-uid fallback), its own layer store, networking through `slirp4netns` or `pasta` (or `--network none|host`), cgroup limits when a delegated cgroup exists
- `run`, `ps`, `logs`, `exec`, `stop`, `rm` work as an unprivileged user; `-p` publishes TCP and UDP ports (slirp4netns or pasta)
- `docker build` works rootless too: `RUN`, `COPY` (wildcards, `--chown`, `--from`), multi-stage builds and the layer cache, with the layers kept in the user's own store
- User-defined networks (`network create`, `run --network <name> --name <svc>`, `-p`, compose): without root there is no bridge, so all containers of a network share one network namespace. They reach each other by name (every name points at `127.0.0.1`) and share one set of ports, so two containers cannot both listen on port 80. Needs `slirp4netns`

**Compose**
- `docker compose up|down|ps|logs|config|build|exec|run|stop|start|restart|kill|pause|unpause|rm|pull|push|top|port|images|ls|cp|events|create|version`, with `up -d <services>`, `--build`, `--no-start`, `--force-recreate`, `--remove-orphans`, `--wait`, `--abort-on-container-exit`, `run --rm -e -v -p --entrypoint`, `down -v --rmi`, `logs --tail -f --no-log-prefix`, `ps --format/--status/-q/--services`, `config --services/--volumes/--format json`
- Several `-f` files and the automatic `docker-compose.override.yml`, `--profile` and `profiles:`, `--env-file`, `--project-directory`, `COMPOSE_PROJECT_NAME`, `secrets:` and `configs:` (file, environment or content), compose labels on every container (`docker ps --filter label=com.docker.compose.project=...`); services, networks, volumes, `depends_on` and healthchecks. `compose watch` is not supported
- `env_file` (one or several files, `required: false`), `${VAR:-default}` and `.env`, `$$` escapes, YAML anchors and `<<` merge keys, long-form volumes (`type: bind`), anonymous volumes (`- /app/node_modules`), `hostname`, `extra_hosts`, `restart`, `container_name`, `command`/`entrypoint`, `build` with `context`/`dockerfile`/`args`
- Tested on real projects: Node apps with MongoDB, Postgres, Redis and OpenResty; Bun, pnpm and Prisma builds; multi-stage frontend builds; 33 of 42 Dockerfiles and compose files from existing projects build as written (the rest need a `.env` file that is not in the folder, or fail on their own for reasons in the project, such as a missing `python`); `scripts/build-all-projects.ps1` builds every Dockerfile and compose file under a folder and prints one PASS/FAIL line each. Database data in named volumes survives `compose down` and `up`

**Security (Linux)**
- Docker's default capability set, a seccomp denylist, `no_new_privs`, `USER` honoured, hardening re-applied by `exec`

**Engine API (`docker daemon`)**
- Unix socket or TCP. Ping, version, info, containers (create, start, stop, restart, kill, pause, wait, logs, attach with stdin, inspect, top, stats, changes, export, archive, commit, update, rename, resize, prune, remove, filters), exec (create, start, inspect, resize, with stdin and a terminal), `POST /build`, `GET /events` (with filters), images (list, inspect, history, pull, tag, load, save, prune, remove), volumes, networks, `system/df`
- Works with the real Docker CLI and the Docker SDK for Python (see Testing)

**Resource usage**
- `docker stats [--no-stream] [--json]` shows RAM, CPU, process count and the writable-layer size of every running container (container writes live in RAM by default)

**Desktop GUI (`docker-gui`)**
- Containers (start, stop, restart, pause, kill, remove, logs) with live RAM, disk and CPU bars and a two-minute history chart per container, a Live tab with all containers side by side, images (pull, remove, prune, run, build), running several images together as a group on a shared network, an image catalog, volumes and networks, run and build dialogs, dark and light themes. Every tab, dialog and button was clicked through on Windows (run, group run, pull, build, volumes, networks, confirm dialogs, theme)

**Any image works.** `docker run <image>` pulls it automatically if it is not on the machine yet, from Docker Hub or any other registry. Intel-only (x86-64) images run on ARM64 machines through built-in emulation (`docker emulation enable|disable|status`, switched on by itself when needed). Emulation translates every instruction: a CPU-bound test ran about 6 times slower than the same program on the native ARM64 image (1.0 s against 5.9 s). On an Intel or AMD PC these images run natively.

**Images run and checked on this engine (43):**
- Base systems: alpine, ubuntu, debian, busybox, fedora, kali, archlinux (Intel, emulated)
- Languages: node, python, golang, rust, php, ruby, java (temurin), .NET
- Web servers and proxies: nginx, httpd, caddy, traefik, haproxy
- Databases: postgres, mysql, mariadb, mongo, redis, memcached, cassandra, couchdb, influxdb
- Messaging: rabbitmq, nats, redpanda, mosquitto
- Apps and tools: wordpress, ghost, grafana, prometheus, vault, registry, mailhog (Intel, emulated), adminer
- CI and dev: jenkins, gitea

`scripts/live-images.sh` pulls and runs each of them again and records the result.

**Programming languages checked (compile and run a program in each):** Python, Node.js, Bun, Deno, Go, Rust, Java, PHP, Ruby, Perl, C, C++, Lua, Elixir, .NET (C#), R, Haskell, and Kotlin (an Intel image, run through emulation). `scripts/live-languages.sh` repeats the check.

## Platforms

| OS | How containers run |
|---|---|
| **Linux** | Native: namespaces, cgroup v2, overlayfs, `pivot_root`. Run as root. |
| **Windows** | The same engine inside a small hidden WSL2 distro: run `docker vm init` once, then use the CLI as on Linux (`docker run -v C:\data:/data ...` works; paths are translated). **Without WSL2** the app still runs, in native mode: `docker run <folder> <program>` runs a Windows program under a Job Object (memory and CPU caps, `ps`/`logs`/`stop`/`stats` work), and commands that need Linux explain how to install WSL2. `docker --native ...` forces this mode. |
| **macOS** | The same engine in a Lima VM (`brew install lima`, then `docker vm init`). The driver is written and compiled but has not been tried on a real Mac. |

## Quick start

```
docker pull alpine
docker run --mem 64 --cpu 50 alpine echo hello
docker run -d --name web -p 8080:80 -e MODE=dev -v ./site:/data:ro nginx:alpine
docker ps
docker logs -f web
docker exec -it web sh
docker update --mem 128 web
docker build -t myapp:1 .
docker compose up -d
docker daemon --host unix:///tmp/docker.sock     # Engine API for other Docker tools
docker-gui                                       # desktop app
```

### Keeping WSL2's memory small (Windows)
The Linux engine runs in WSL2, whose VM takes RAM from Windows. You can cap it and let it give memory back by putting this in `%USERPROFILE%\.wslconfig` (then `wsl --shutdown`):
```
[wsl2]
memory=3GB
[experimental]
autoMemoryReclaim=gradual
```
Without WSL2 the app runs in native mode and uses no VM memory at all.

## Why it is light
- **RAM:** no daemon; `--mem` is a hard cgroup cap with swap off.
- **SSD:** layers are stored once (content-addressed) and shared; container writes go to RAM (tmpfs) by default, so a container writes nothing to disk unless you mount a volume.
- **Network:** no `ip` or `iptables` needed; one bridge, one veth per container, one NAT rule.
- **Binary:** static, `opt-level=z`, LTO, stripped (about 1.4 MB for the Linux engine).

## Build
```powershell
scripts\build.ps1    # tests, then the Windows CLI + GUI and the Linux engine (docker-linux-<arch>) that `docker vm init` installs
```
```sh
cargo build --release -p docker-cli --target aarch64-unknown-linux-musl   # or x86_64-unknown-linux-musl
```
`pull` uses the system `curl`, which is preinstalled on Windows 10+, macOS and most Linux systems.

## Testing
- Unit tests for every crate: `cargo test --workspace`
- End-to-end suites in `scripts/` (Linux, run as root, offline): containers, networking (TCP/UDP/IPv6/subnets), user-defined networks, builds (three suites), security, push and registry features, save/load, compose, terminals and healthchecks, Swarm-lite, Engine API, rootless (as an unprivileged user)
- `scripts/e2e-compat.sh`: Docker's CLI options and `--format`, `info`, `events`, `create`, `diff`, `commit`, `export`, `import`, `port`, prune commands
- `scripts/e2e-compose2.sh`: compose override files, profiles, `exec`, `run`, `stop`/`start`/`restart`/`kill`/`rm`, `ls`, `cp`, `down -v`
- `scripts/e2e-swarm2.sh`: `node`, `secret`, `config`, `service rollback`, drain, `--secret`/`--config`
- `scripts/e2e-flags.sh`: the `docker run` flags (`--dns`, `--read-only`, `--tmpfs`, `--ulimit`, `--init`, `--cpus`, `--label`, `--shm-size`) and anonymous volumes
- `scripts/e2e-vm.ps1`: Windows to the Linux VM
- `scripts/e2e-realcli.sh`: the real Docker CLI against our daemon (run, exec, `-it`, build, events, update and more)
- `scripts/e2e-sdk.sh`: the Docker SDK for Python against our daemon

## Layout
```
crates/image  OCI references, registry client, layer store, Dockerfile parsing (pure Rust, portable)
crates/core   container backends (linux/, windows/), registry of containers, networks, volumes, CLI parsing
crates/cli    the `docker` binary and the Engine API daemon
crates/gui    `docker-gui` desktop app (egui)
scripts/      build and end-to-end tests
```

## License
MIT, see [LICENSE](LICENSE). Design notes and progress log: [ROADMAP.md](ROADMAP.md).
