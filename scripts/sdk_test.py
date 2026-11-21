import sys, time, io, tarfile
sys.path.insert(0, sys.argv[2])
import docker
fails = 0
def check(name, ok):
    global fails
    print(("PASS " if ok else "FAIL ") + name)
    if not ok: fails += 1
c = docker.DockerClient(base_url="unix://" + sys.argv[1])
try:
    v = c.version()
    check("version", v.get("Os") == "linux")
    check("ping", c.ping())
    info = c.info()
    check("info", "Containers" in info)
    check("images.list", any("alpine:latest" in i.tags for i in c.images.list()))
    img = c.images.get("alpine")
    check("images.get", img.attrs["Os"] == "linux")
    out = c.containers.run("alpine", ["echo", "sdk-hello"], remove=True)
    check("containers.run returns output", b"sdk-hello" in out)
    ct = c.containers.run("alpine", ["sh", "-c", "echo booted; sleep 120"], name="sdkweb", detach=True, environment={"A": "1"})
    time.sleep(2)
    check("container listed running", any(x.name == "sdkweb" for x in c.containers.list()))
    ct.reload()
    check("status running", ct.status == "running")
    check("logs", b"booted" in ct.logs())
    r = ct.exec_run(["sh", "-c", "echo $A-exec; exit 3"])
    check("exec_run output and exit code", r.exit_code == 3 and b"1-exec" in r.output)
    ct.rename("sdkweb2")
    ct.reload()
    check("rename", ct.name == "sdkweb2")
    ct.update(mem_limit="64m")
    check("update accepted", True)
    ct.stop(timeout=1)
    ct.reload()
    check("stop", ct.status == "exited")
    ct.remove()
    check("remove", not any(x.name == "sdkweb2" for x in c.containers.list(all=True)))
    vol = c.volumes.create("sdkvol")
    check("volumes.create", vol.name == "sdkvol")
    vol.remove()
    net = c.networks.create("sdknet")
    check("networks.create", net.name == "sdknet")
    net.remove()
    ctx = io.BytesIO()
    with tarfile.open(fileobj=ctx, mode="w") as t:
        d = b"FROM alpine\nRUN echo built > /b\nCMD [\"cat\", \"/b\"]\n"
        ti = tarfile.TarInfo("Dockerfile"); ti.size = len(d); t.addfile(ti, io.BytesIO(d))
    ctx.seek(0)
    built, logs = c.images.build(fileobj=ctx, custom_context=True, tag="sdkbuilt:1")
    check("images.build", built is not None)
    check("built image runs", b"built" in c.containers.run("sdkbuilt:1", remove=True))
    c.images.remove("sdkbuilt:1")
    check("images.remove", True)
    evs = []
    import threading
    def watch():
        for e in c.events(decode=True, until=int(time.time()) + 6):
            evs.append(e)
    th = threading.Thread(target=watch); th.start(); time.sleep(1)
    c.containers.run("alpine", ["true"], remove=True)
    th.join()
    check("events stream", any(e.get("Action") in ("create", "start", "die") for e in evs))
except Exception as e:
    import traceback; traceback.print_exc()
    fails += 1
print("ALL PASSED" if fails == 0 else f"{fails} FAILED")
sys.exit(1 if fails else 0)
