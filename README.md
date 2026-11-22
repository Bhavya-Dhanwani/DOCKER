# Sasta Docker

A Docker replacement written in Rust. It runs real Linux containers, builds and pulls images from Docker Hub,
speaks the Docker Engine API, and has a desktop GUI. Small static binary, no always-running daemon, and
low RAM and disk use.

The command is still called `docker`, so existing scripts and tools keep working. The desktop app is `docker-gui`.

## What it can do

**Containers**
- `run` (`-d`, `-it`, `--name`, `-e`, `-v`, `-p`, `--network`, `--mem`, `--cpu`, `--procs`, `--restart`, `--rm`, `--entrypoint`, `-w`, `-u`, `--health-*`, `--cap-add/--cap-drop`, `--security-opt`, `--privileged`)
- `exec` (also `-it`), `logs [-f]`, `ps [-a]`, `inspect`, `stop`, `start`, `restart`, `kill`, `pause`, `unpause`, `wait`, `top`, `rename`, `rm [-f]`, `cp`, `stats`
- `update --mem/--cpu/--procs` changes the limits of a running container
- Restart policies (`no`, `always`, `unless-stopped`, `on-failure[:n]`) and healthchecks

**Images**
- `pull` from Docker Hub and other registries, `push`, `login`/`logout`, `tag`, `images`, `rmi`, `save`/`load`, `prune`
- `build` from a Dockerfile: multi-stage builds, `.dockerignore`, `--target`, `--build-arg`, `--no-cache`, `COPY --from`, `COPY --chown`, `HEALTHCHECK`, content-hash layer cache
- Also: `ADD` from a URL or a tar archive, `COPY --chmod`, `RUN --mount` (cache, bind, tmpfs, secret), heredocs, `SHELL`, `ONBUILD`, `VOLUME`, `STOPSIGNAL`, `--label`, `--secret`, `-q`
- `buildx build` is accepted as an alias for `build`
- Pull by digest, `pull --platform`, zstd-compressed layers; push with resumable chunked uploads, retries, and cross-repository layer reuse; credential helpers and `~/.docker/config.json` logins
- `search`, `history`, `image inspect`
- Layers are stored once and shared between images

**Networking and storage**
- A bridge with one veth per container (pure-Rust netlink) and NAT to the internet
- `-p [ip:]host:container[/tcp|udp]` port publishing, host-IP binding and port ranges, `--network bridge|host|none`
- IPv6 addresses for containers (bridge, NAT66)
- The bridge and its firewall rules are removed when the last container exits
- User-defined networks with container-name DNS and aliases, `network create --subnet`, `network connect|disconnect`
- Named volumes and bind mounts: `volume create|ls|rm|inspect|prune`, `network create|ls|rm|inspect`

**Swarm-lite (single node)**
- `swarm init|leave|status`, `service create|ls|ps|inspect|logs|scale|update|rm` with replicas, restart conditions and rolling updates, `stack deploy|ls|ps|services|rm` from a compose file (`deploy:` honoured), round-robin load balancing of published ports across replicas

**Rootless mode (Linux)**
- `docker --rootless ...`, selected automatically when not running as root: user namespaces (full uid/gid mapping with `newuidmap`, or a single-uid fallback), its own layer store, networking through `slirp4netns` or `pasta` (or `--network none|host`), cgroup limits when a delegated cgroup exists
- `run`, `ps`, `logs`, `exec`, `stop`, `rm` work as an unprivileged user

**Compose**
- `docker compose up|down|ps|logs|config|build` with services, networks, volumes, `depends_on` and healthchecks

**Security (Linux)**
- Docker's default capability set, a seccomp denylist, `no_new_privs`, `USER` honoured, hardening re-applied by `exec`

**Engine API (`docker daemon`)**
- Unix socket or TCP. Ping, version, info, containers (create, start, stop, restart, kill, pause, wait, logs, attach with stdin, inspect, top, stats, changes, export, archive, commit, update, rename, resize, prune, remove, filters), exec (create, start, inspect, resize, with stdin and a terminal), `POST /build`, `GET /events` (with filters), images (list, inspect, history, pull, tag, load, save, prune, remove), volumes, networks, `system/df`
- Works with the real Docker CLI and the Docker SDK for Python (see Testing)

**Desktop GUI (`docker-gui`)**
- Containers (start, stop, restart, pause, kill, remove, logs, live stats), images (pull, remove, prune, run, build), volumes and networks, run and build dialogs, dark and light themes

**Verified images:** `alpine`, `node`, `postgres`, `redis`, `nginx`, `python`, `mariadb`.

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
