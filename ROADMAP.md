# Roadmap

Goal: a Docker-compatible CLI in Rust that uses less RAM and disk. Containers are Linux-kernel features, so:
**Linux runs them natively; macOS and Windows run the same engine inside a tiny hidden Linux VM**
(Apple Virtualization.framework / WSL2 or Hyper-V), exactly how Docker Desktop, Podman and OrbStack do it.

## Status at a glance

**Made:** the container engine (namespaces, cgroup v2, overlayfs, seccomp), pull/push/build (multi-stage, `.dockerignore`, `ADD` URL/tar, `RUN --mount`, heredocs, `ONBUILD`, `SHELL`, `VOLUME`, `STOPSIGNAL`),
registry features (digest pulls, zstd, resumable push, credential helpers, search/history), compose, networks (UDP, IPv6, subnets, name DNS) and volumes, restart policies, healthchecks,
terminals (`run -it`, `exec -it`), `docker update`, Swarm-lite (single node), rootless mode (user namespaces, slirp4netns/pasta), the Engine API (real Docker CLI and Docker SDK for Python pass their checks),
Windows and macOS forwarding into a hidden Linux VM, the `Sasta Docker` desktop GUI, and the node, postgres, redis, nginx, python and mariadb images.

**Remaining:**
- macOS has never been run on a real Mac (the Lima driver is written and compiled only)
- The GUI was clicked through on Windows (every tab, dialog and button); it has no Swarm, compose or rootless views, and has not been run on Linux or macOS
- Rootless: `build` and user-defined networks work (the containers of one network share a single network namespace, so they share ports); no `rmi`/`prune` of layers owned by mapped uids, no `push`/`save` of rootless-built layers, no UDP `-p`, limits only inside a delegated cgroup, user-defined networks need slirp4netns; the systemd `user@UID.service` delegation path untested
- Swarm: single node only (no `swarm join`), `service create/update` do not wait for convergence, `service logs -f` prints a snapshot, `depends_on` ignored, `stack deploy` does not prune removed services
- IPv6 to the internet (NAT66) untested (no IPv6 route on the test host); plain `-p` does not also listen on `[::]`
- Registry: zstd tested only with hand-made frames, parallel upload/mount/chunk resume only against a mock registry, `docker manifest inspect` not implemented
- Engine API: `changes`/`export`/`archive`/`commit` need a running container; `images/search` returns an empty list; `commit` does not carry the container's run-time cmd/env
- Build: `ADD --checksum/--keep-git-dir/--exclude`, git URLs, `COPY --parents`, `.tar.zst` are rejected; anonymous `VOLUME`s are never garbage-collected
- Windows-native containers beyond the Job Object backend (`--native`)
- Only 43 popular images have been run (see the README); other images may hit bugs. Intel-only images run under emulation, which is slower and not fully compatible
- Android and macOS images cannot run: Android needs a kernel feature (binder) that the stock WSL kernel lacks, and macOS cannot legally run in a container
- minio/minio is not pullable from Docker Hub or quay.io any more

| Phase | What | State |
|---|---|---|
| 1 | Pull images (Docker Hub / OCI), dedup layer store, run from overlayfs, cgroup limits | **done**, verified end-to-end with `alpine` on Linux ARM64 (pull, run, RAM-only writes, OOM kill, pids limit, clean teardown) |
| 1b | `ps`, `stop`, `images`, `rmi`, GUI dashboard | done |
| 2 | `-e`, `-v` bind mounts, `--name`, `-d`, `logs [-f]`, `exec`, `rm [-f]`, `ps -a`, `inspect`, id prefixes | **done**, 31-check e2e script passes on Linux ARM64 (`scripts/e2e.sh`) |
| 3 | Networking: bridge `rdbr0` + veth per container (pure-Rust netlink), nftables masquerade, `-p H:C` (TCP), generated resolv.conf, `--network bridge/host/none` | **done**, 20-check e2e (`scripts/e2e-net.sh`) passes on Linux ARM64; includes DNS and HTTP from inside a container |
| 4 | `docker build`: FROM (auto-pull, `scratch`), RUN, COPY/ADD (local), ENV, ARG, WORKDIR, USER, CMD, ENTRYPOINT, EXPOSE, LABEL; content-hash layer cache; `--build-arg`, `--no-cache`, `-f`; `USER` at run time; `docker prune` | **done**, 41-check e2e (`scripts/e2e-build.sh`) passes on Linux ARM64 |
| 5 | Capabilities (Docker's default set, `--cap-add/--cap-drop/--privileged`), seccomp denylist, `--security-opt no-new-privileges`, hardening re-applied by `exec`; `login`/`logout`/`tag`/`push` (bearer + basic auth, loopback registries over HTTP) | **done**: `scripts/e2e-security.sh` (29 checks) and `scripts/e2e-push.sh` (59 checks, against `scripts/mock-registry.py`) pass on Linux ARM64 |
| 5b | Rootless mode (user namespaces) | **not started**, see below |
| 6 | Windows + macOS: the CLI forwards every Linux command into a hidden VM (WSL2 distro / Lima instance), translating host paths (`-v`, `build` context) and keeping exit codes; `docker vm init/start/stop/status`, `docker version`; `docker --native` keeps the Windows Job Object backend | **Windows done**: `scripts/e2e-vm.ps1` (25 checks, driven through a real WSL distro) plus a real `rust-docker` VM created with `docker vm init`. **macOS**: Lima driver written, compiles and is unit-tested, but has **never been run on a Mac** |
| 7 | Docker Engine API (`docker daemon`): ping/version/info, containers (list, create, start, stop, restart, kill, pause, wait, logs, attach, inspect, remove), exec (create, start, inspect), `POST /build` (tar context, streamed JSON progress), `GET /events` (create/start/die/destroy), images (list, inspect, pull, tag, remove), volumes, networks | **done**, tested with a stdlib client (`scripts/e2e-api.sh`, 55 checks); also rename, resize (accepted, no-op), `exec -i` stdin and `--rm`/AutoRemove. Tested against the **real Docker CLI 29.10** (`scripts/e2e-realcli.sh`, 18 checks: version, images, run/--rm/-d with exit codes, ps, logs, exec incl. `-i` and exit codes, inspect, rename, info, stop, rm, build, events), run on Linux over the unix socket (WSL's localhost relay drops half-closed TCP connections, so the Windows CLI over TCP can't do exec/attach). `exec -it` works (a pty between the stream and the command, with resize), `run -it`/attach works for terminal containers (via a per-container attach socket), and `docker update --mem/--cpu/--procs` (CLI and API) changes the cgroup limits live. Real Docker CLI 29.10: 21 checks incl. `run -it`, `exec -it`, `update`. No stdin on attach for containers without a terminal; `docker update --restart` is not supported. `docker buildx build` is accepted as an alias for `build` (its output flags are ignored) |
| 8 | Everything after the first 7 phases: `-it`/exec terminals, restart policies, HEALTHCHECK, multi-stage builds, `.dockerignore`, `COPY --chown`, named volumes, user-defined networks with name DNS, `cp`, `top`, `stats`, `save`/`load`, `compose` | **done**, one e2e suite each (`scripts/e2e-*.sh`) |

## Decisions
- Layers are stored **once, unpacked, content-addressed**; images share them. The compressed blob is deleted after unpack.
- Container writes go to a **tmpfs upper layer** (RAM, counted against `--mem`): zero SSD writes by default.
- No daemon: each `docker run` is its own supervisor. A daemon arrives only when the Engine API (phase 7) needs one.
- HTTP uses the system `curl` for now (no C toolchain needed to build); swap for a Rust client later.

## Rootless (5b): why it is separate
Running without root is not a flag on the current design, it changes four foundations:
1. **Layer format.** Layers are unpacked with `trusted.overlay.*` xattrs and `mknod` whiteouts, which need real root. Unprivileged overlayfs uses `userxattr` (`user.overlay.*`), so the store needs a second layer flavour (or unpacking inside a user namespace).
2. **Ownership.** One mapped uid (`/proc/<pid>/uid_map`) cannot own the many uids in image layers; real images need `newuidmap`/`newgidmap` with a `/etc/subuid` range.
3. **Networking.** An unprivileged user cannot create a bridge/veth on the host; it needs a user-space stack (slirp4netns/pasta-style) inside the container's network namespace.
4. **Resources.** cgroup v2 limits need systemd delegation to the user.
Doing it half-way would hand out a "rootless" mode with silent ownership and network gaps, so it stays unimplemented until those four are designed together.

## VM backend notes (phase 6)
- `docker vm init` on Windows imports a new WSL2 distro from Alpine's root filesystem (pulled with our own image code), installs the Linux engine, and runs `apk add curl` inside it. Verified for real on this machine: it created the `rust-docker` distro, after which `docker pull`, `run`, `build` (with a `RUN apk add jq` over the internet), container DNS and a published port reachable from Windows all worked.
- The engine lives in the VM's own disk: images and layers never touch the Windows drive. Bind mounts from `C:\...` go through WSL's drive mount (slower, and Linux file modes on those files are approximate).
- WSL shuts an idle VM down, which would kill detached containers, so every command makes sure a hidden `sleep` keeps it alive.
- The GUI talks to the VM through the same `docker` CLI, so it shows the containers and images that live in the VM.
- Not done: a TTY (`-it`), UNC paths (`\\server\share`), macOS Virtualization.framework (the macOS driver uses Lima, which must be installed).

## Known gaps
- Security: seccomp is a denylist of dangerous syscalls (not Docker's full allowlist profile); no AppArmor/SELinux; no user-namespace remapping. Do not run untrusted code.
- Registry: push uses monolithic blob uploads (no chunked/resumable, no cross-repo blob mount); layers are re-tarred on every push; no zstd; credentials are stored in `<store>/auth.json` (mode 0600, not encrypted). Built layers are overlay directories, not tarballs, so a built image cannot be pushed as is without repacking.
- Build: no `ADD` of URLs/tarballs, `COPY --chmod`, `RUN --mount`, `SHELL`, `ONBUILD`; `VOLUME` and `STOPSIGNAL` are ignored. `buildx build` is only an alias (no multi-platform builds, no BuildKit cache).
- Networking: no UDP `-p`, no IPv6; the NAT rule and bridge are left in place after the last container exits.
- Engine API over Windows-to-WSL TCP: WSL's localhost forwarding drops half-closed connections, so exec/attach are only reliable over the Linux unix socket.
- Container attach has no window-resize message.
