# Sasta Docker

**Sasta Docker** is a small Docker-style container tool in Rust. No daemon. See [ROADMAP.md](ROADMAP.md).
The command is still called `docker` (so existing scripts and tools work) and the desktop app is `docker-gui` ("Sasta Docker").

```
docker pull alpine            # download + verify + unpack layers (deduplicated)
docker login -u me --password-stdin ghcr.io
docker tag myapp:1 ghcr.io/me/myapp:1 && docker push ghcr.io/me/myapp:1
docker build -t myapp:1 .     # Dockerfile -> cached layers (Linux)
docker images
docker run --mem 64 --cpu 50 alpine echo hello
docker run -d --name web -p 8080:80 -e MODE=dev -v ./site:/data:ro alpine sleep 600
docker run --network none alpine ip addr      # bridge (default) | host | none
docker run --cap-drop ALL --cap-add NET_BIND_SERVICE --security-opt no-new-privileges myimage
docker ps [-a]                # -a includes exited containers
docker logs [-f] web
docker exec web sh -c 'echo $MODE; ls /data'
docker inspect web
docker stop web
docker rm [-f] web
docker rmi alpine             # frees only layers no other image uses
docker prune                  # delete unused layers and build leftovers
docker run -it alpine sh      # terminals; also: exec -it, --restart, --health-cmd, --entrypoint
docker compose up -d          # compose.yaml: services, networks, volumes, depends_on, healthchecks
docker save -o app.tar app && docker load -i app.tar
docker daemon --host unix:///tmp/docker.sock   # Docker Engine API (incl. exec, build, attach, events)
docker-gui                    # live dashboard
```

## Platforms
Linux runs containers natively. **Windows and macOS run the same engine in a small hidden Linux VM**: run `docker vm init` once, then use the CLI exactly as on Linux (`docker run -v C:\data:/data ...` works; paths are translated).

| OS | How containers run |
|---|---|
| **Linux** | Native: namespaces, cgroup v2, overlayfs, `pivot_root`. Run as root. |
| **Windows** | Linux containers in a hidden WSL2 distro (`docker vm init`). Separately, `docker --native run --mem 128 . <command>` runs a Windows process tree under a Job Object (memory/CPU caps, no isolation). |
| **macOS** | Linux containers in a Lima VM (`brew install lima`, then `docker vm init`). Written and compiled, **not yet tested on a real Mac**. |

## Security defaults (Linux)
Docker's default capability set (a80425fb), a seccomp denylist of kernel-attack syscalls, `USER` honoured, `exec` re-applies all of it. `--privileged` removes it. Rootless mode is not implemented (see ROADMAP).

## Why it's light
- **Network:** pure-Rust netlink (no `ip`/`iptables` needed): one bridge, one veth per container, one NAT rule; `-p` is a small user-space forwarder.
- **RAM:** no daemon; `--mem` is a hard cgroup cap with swap off.
- **SSD:** layers stored once (content-addressed) and shared between images; container writes go to RAM (tmpfs upper layer), so a container writes nothing to disk by default.
- Small static binary (`opt-level=z`, LTO, stripped).

## Build
```powershell
scripts\build.ps1   # tests, Windows cli+gui, and the Linux engine (docker-linux-<arch>) that `docker vm init` installs
```
Needs `curl` at runtime for `pull` (preinstalled on Windows 10+, macOS, most Linux).

## Layout
```
crates/image  OCI reference parsing, registry pull, layer store (pure Rust, portable)
crates/core   run/stats/stop backends (linux/, windows/), registry of running containers, CLI parsing
crates/cli    `docker` binary
crates/gui    `docker-gui` dashboard (egui)
scripts/      build, e2e*.sh (Linux, run as root), e2e-vm.ps1 (Windows -> VM), mock-registry.py
```

## Limits
No rootless mode; no multi-stage builds or `.dockerignore`; no UDP `-p`, no IPv6, no container-name DNS; no seccomp. Do not run untrusted code.
