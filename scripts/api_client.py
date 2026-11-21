#!/usr/bin/env python3
"""Exercise the Engine API over a unix socket (stdlib only). Prints PASS/FAIL lines like the shell suites."""
import http.client, io, json, socket, struct, sys, tarfile, threading, time

SOCK = sys.argv[1]
fails = 0

class Conn(http.client.HTTPConnection):
    def __init__(self):
        super().__init__("localhost")
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(SOCK)

def call(method, path, body=None, raw=False):
    c = Conn()
    data = body if isinstance(body, bytes) else (json.dumps(body) if body is not None else None)
    c.request(method, "/v1.43" + path, body=data, headers={"Content-Type": "application/x-tar" if isinstance(body, bytes) else "application/json"})
    r = c.getresponse()
    payload = r.read()
    c.close()
    if raw:
        return r.status, payload
    return r.status, (json.loads(payload) if payload and r.getheader("Content-Type", "").startswith("application/json") else payload.decode(errors="replace"))

def check(name, ok):
    global fails
    print(("PASS " if ok else "FAIL ") + name)
    if not ok:
        fails += 1

def frames(payload):
    out, i = b"", 0
    while i + 8 <= len(payload):
        _, size = struct.unpack(">BxxxI", payload[i:i + 8])
        out += payload[i + 8:i + 8 + size]
        i += 8 + size
    return out.decode()

def wait_state(cid, want, secs=15):
    end = time.time() + secs
    while time.time() < end:
        st, j = call("GET", f"/containers/{cid}/json")
        if st == 200 and j["State"]["Status"] == want:
            return True
        time.sleep(0.3)
    return False

st, body = call("GET", "/_ping")
check("_ping answers OK", st == 200 and body == "OK")
st, v = call("GET", "/version")
check("version reports an API version and linux", st == 200 and v["ApiVersion"] == "1.43" and v["Os"] == "linux")
st, i = call("GET", "/info")
check("info has the usual fields", st == 200 and i["OSType"] == "linux" and "Containers" in i and i["Driver"] == "overlay2")
st, imgs = call("GET", "/images/json")
check("images/json lists alpine", st == 200 and any("alpine:latest" in x["RepoTags"] for x in imgs))
st, img = call("GET", "/images/alpine:latest/json")
check("images/{name}/json inspects it", st == 200 and img["Os"] == "linux" and img["RootFS"]["Layers"])
st, e = call("GET", "/images/nosuch:1/json")
check("a missing image is a 404 with a message", st == 404 and "No such image" in e["message"])

# ---- containers
st, e = call("POST", "/containers/create?name=apitest", {"Image": "nosuch:1"})
check("create with a missing image is 404 (clients pull and retry)", st == 404)
body = {"Image": "alpine", "Cmd": ["sh", "-c", "echo hello-from-api; echo err-line >&2; sleep 300"], "Env": ["GREETING=hi"],
        "HostConfig": {"NetworkMode": "none", "Memory": 64 * 1024 * 1024}}
st, c = call("POST", "/containers/create?name=apitest", body)
cid = c.get("Id", "") if isinstance(c, dict) else ""
check("create returns 201 and an id", st == 201 and len(cid) >= 12)
st, e = call("POST", "/containers/create?name=apitest", body)
check("a duplicate name is 409", st == 409)
st, j = call("GET", f"/containers/{cid}/json")
check("a created container is 'created' and not running", st == 200 and j["State"]["Status"] == "created" and not j["State"]["Running"])
check("inspect shows the config (Image, Cmd, Env, Name)", j["Config"]["Image"] == "alpine" and "GREETING=hi" in j["Config"]["Env"] and j["Name"] == "/apitest")
st, lst = call("GET", "/containers/json?all=1")
check("it shows in the all=1 list as created, not in the running list", any(x["Id"] == cid and x["State"] == "created" for x in lst) and not any(x["Id"] == cid for x in call("GET", "/containers/json")[1]))
st, _ = call("POST", f"/containers/{cid}/start")
check("start returns 204", st == 204)
check("it becomes running", wait_state(cid, "running"))
st, _ = call("POST", f"/containers/{cid}/start")
check("starting a running container is 304", st == 304)
st, j2 = call("GET", f"/containers/{cid}/json")
check("the container id is stable across create and start", j2["Id"] == cid and j2["State"]["Pid"] > 0)
time.sleep(1)
st, raw = call("GET", f"/containers/{cid}/logs?stdout=1&stderr=1", raw=True)
text = frames(raw)
check("logs use the multiplexed stream format and have the output", st == 200 and "hello-from-api" in text and "err-line" in text)
st, lst = call("GET", "/containers/json")
check("it appears in the running list with Names and State", any(x["Id"] == cid and x["Names"] == ["/apitest"] and x["State"] == "running" for x in lst))
st, e = call("DELETE", f"/containers/{cid}")
check("removing a running container without force is 409", st == 409)
st, _ = call("POST", f"/containers/{cid}/pause")
check("pause works", st == 204)
st, _ = call("POST", f"/containers/{cid}/unpause")
check("unpause works", st == 204)
st, _ = call("POST", f"/containers/{cid}/stop?t=1")
check("stop returns 204", st == 204)
check("it becomes exited", wait_state(cid, "exited"))
st, w = call("POST", f"/containers/{cid}/wait")
check("wait returns the exit code", st == 200 and "StatusCode" in w)
st, _ = call("POST", f"/containers/{cid}/stop?t=1")
check("stopping a stopped container is 304", st == 304)
st, _ = call("DELETE", f"/containers/{cid}")
check("removal returns 204", st == 204)
st, e = call("GET", f"/containers/{cid}/json")
check("it is gone (404)", st == 404)

st, c = call("POST", "/containers/create", {"Image": "alpine", "Cmd": ["sh", "-c", "exit 7"], "HostConfig": {"NetworkMode": "none"}})
c2 = c["Id"]
call("POST", f"/containers/{c2}/start")
st, w = call("POST", f"/containers/{c2}/wait")
check("a container that exits 7 reports StatusCode 7", w.get("StatusCode") == 7)
call("DELETE", f"/containers/{c2}?force=1")

# ---- volumes and networks
st, v = call("POST", "/volumes/create", {"Name": "apivol"})
check("volume create", st == 201 and v["Name"] == "apivol")
st, vl = call("GET", "/volumes")
check("volume list", any(x["Name"] == "apivol" for x in vl["Volumes"]))
st, _ = call("DELETE", "/volumes/apivol")
check("volume delete", st == 204)
st, n = call("POST", "/networks/create", {"Name": "apinet"})
check("network create", st == 201 and n["Id"])
st, nl = call("GET", "/networks")
check("network list includes built-ins and the new one", {"bridge", "host", "none", "apinet"} <= {x["Name"] for x in nl})
st, _ = call("DELETE", "/networks/apinet")
check("network delete", st == 204)

# ---- images
st, _ = call("POST", "/images/alpine:latest/tag?repo=apitag&tag=v1")
check("image tag", st == 201)
st, imgs = call("GET", "/images/json")
check("the new tag is listed", any("apitag:v1" in x["RepoTags"] for x in imgs))
st, r = call("DELETE", "/images/apitag:v1")
check("image delete reports Untagged", st == 200 and r[0].get("Untagged") == "apitag:v1")

# ---- misc
st, e = call("GET", "/nope")
check("unknown paths are 404", st == 404)
st, e = call("POST", "/containers/abc/update", {"Memory": 1})
check("update of a missing container is 404", st == 404)
st, c = call("POST", "/containers/create?name=apiren", {"Image": "alpine", "Cmd": ["true"], "HostConfig": {"NetworkMode": "none"}})
rid = c["Id"]
st, _ = call("POST", f"/containers/{rid}/rename?name=apiren2")
st2, j = call("GET", f"/containers/{rid}/json")
check("rename works", st == 204 and j["Name"] == "/apiren2")
st, _ = call("POST", f"/containers/{rid}/resize?h=24&w=80")
check("resize is accepted", st == 200)
st, _ = call("POST", f"/containers/{rid}/rename")
check("rename without a name is 400", st == 400)
call("DELETE", f"/containers/{rid}?force=1")
st, c = call("POST", "/containers/create", {"Image": "alpine", "Cmd": ["sh", "-c", "echo auto-rm-out"], "HostConfig": {"NetworkMode": "none", "AutoRemove": True}})
arid = c["Id"]
call("POST", f"/containers/{arid}/start")
st, w = call("POST", f"/containers/{arid}/wait?condition=removed")
check("AutoRemove: wait returns", st == 200 and "StatusCode" in w)
time.sleep(3)
check("AutoRemove: the container is gone afterwards", call("GET", f"/containers/{arid}/json")[0] == 404)

# ---- exec
st, c = call("POST", "/containers/create?name=apiexec", {"Image": "alpine", "Cmd": ["sleep", "300"], "HostConfig": {"NetworkMode": "none"}})
xid = c["Id"]
st, e = call("POST", f"/containers/{xid}/exec", {"Cmd": ["echo", "x"]})
check("exec in a stopped container is 409", st == 409)
call("POST", f"/containers/{xid}/start")
wait_state(xid, "running")
st, e = call("POST", f"/containers/{xid}/exec", {"Cmd": ["sh", "-c", "echo out-$FOO; echo err >&2; pwd"], "Env": ["FOO=bar"], "WorkingDir": "/tmp", "AttachStdout": True, "AttachStderr": True})
check("exec create returns 201 and an id", st == 201 and e.get("Id"))
eid = e["Id"]
st, j = call("GET", f"/exec/{eid}/json")
check("a new exec is not finished", st == 200 and j["ExitCode"] is None and j["Running"])
st, raw = call("POST", f"/exec/{eid}/start", {"Detach": False, "Tty": False}, raw=True)
out = frames(raw)
check("exec start streams multiplexed output with env, stderr and workdir", st == 200 and "out-bar" in out and "err" in out and "/tmp" in out)
st, j = call("GET", f"/exec/{eid}/json")
check("exec inspect reports exit code 0 once done", j["ExitCode"] == 0 and not j["Running"])
st, e = call("POST", f"/containers/{xid}/exec", {"Cmd": ["sh", "-c", "exit 3"]})
call("POST", f"/exec/{e['Id']}/start", {})
st, j = call("GET", f"/exec/{e['Id']}/json")
check("a failing exec reports its exit code", j["ExitCode"] == 3)
st, e = call("POST", f"/containers/{xid}/exec", {"Cmd": ["true"]})
st, _ = call("POST", f"/exec/{e['Id']}/start", {"Detach": True})
check("a detached exec returns at once", st == 200)
st, e = call("GET", "/exec/nosuch/json")
check("a missing exec is 404", st == 404)
st, e = call("POST", f"/containers/{xid}/exec", {})
check("exec without Cmd is 400", st == 400)

# exec with a terminal and stdin over a hijacked connection (what `docker exec -it` does)
st, e = call("POST", f"/containers/{xid}/exec", {"Cmd": ["sh", "-c", "[ -t 0 ] && echo is-a-tty; read x; echo got-$x"], "AttachStdin": True, "AttachStdout": True, "AttachStderr": True, "Tty": True})
teid = e["Id"]
hs = socket.socket(socket.AF_UNIX); hs.connect(SOCK); hs.settimeout(10)
tb = json.dumps({"Detach": False, "Tty": True}).encode()
hs.sendall(b"POST /v1.43/exec/" + teid.encode() + b"/start HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Type: application/json\r\nContent-Length: " + str(len(tb)).encode() + b"\r\n\r\n" + tb)
time.sleep(1.5)
hs.sendall(b"hello\n")
got = b""
try:
    while True:
        d = hs.recv(4096)
        if not d:
            break
        got += d
except socket.timeout:
    pass
hs.close()
txt = got.decode(errors="replace")
check("a TTY exec is a real terminal and echoes stdin to the command", "101" in txt and "is-a-tty" in txt and "got-hello" in txt)
time.sleep(0.5)
st, j = call("GET", f"/exec/{teid}/json")
check("the TTY exec finished with exit code 0", j["ExitCode"] == 0)
st, _ = call("POST", f"/exec/{teid}/resize?h=40&w=100")
check("resize on a finished TTY exec is accepted", st == 200)

# ---- live update: the kernel limit changes and inspect follows
st, c = call("POST", "/containers/create?name=apiupd", {"Image": "alpine", "Cmd": ["sleep", "300"], "HostConfig": {"NetworkMode": "none", "Memory": 64 * 1024 * 1024}})
uid = c["Id"]
call("POST", f"/containers/{uid}/start")
wait_state(uid, "running")
st, r = call("POST", f"/containers/{uid}/update", {"Memory": 32 * 1024 * 1024, "PidsLimit": 50})
check("update returns 200", st == 200 and r.get("Warnings") == [])
try:
    mx = open(f"/sys/fs/cgroup/rust-docker-{uid}/memory.max").read().strip()
    px = open(f"/sys/fs/cgroup/rust-docker-{uid}/pids.max").read().strip()
except OSError:
    mx = px = "?"
check("update changed the cgroup limits", mx == str(32 * 1024 * 1024) and px == "50")
st, j = call("GET", f"/containers/{uid}/json")
check("inspect shows the new memory limit", j["HostConfig"]["Memory"] == 32 * 1024 * 1024)
call("DELETE", f"/containers/{uid}?force=1")

# ---- `docker run -it` over the API: attach (with a terminal) before start, type into the shell, exit
st, c = call("POST", "/containers/create?name=apiit", {"Image": "alpine", "Cmd": ["sh"], "Tty": True, "OpenStdin": True, "AttachStdin": True, "AttachStdout": True, "HostConfig": {"NetworkMode": "none"}})
itid = c["Id"]
at = socket.socket(socket.AF_UNIX); at.connect(SOCK); at.settimeout(15)
at.sendall(b"POST /v1.43/containers/" + itid.encode() + b"/attach?stream=1&stdin=1&stdout=1&stderr=1 HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 0\r\n\r\n")
head_resp = at.recv(4096)
call("POST", f"/containers/{itid}/start")
time.sleep(2)
at.sendall(b"echo it-$((6*7))\n")
time.sleep(1)
at.sendall(b"exit\n")
got = head_resp
try:
    while True:
        d = at.recv(4096)
        if not d:
            break
        got += d
except socket.timeout:
    pass
at.close()
check("run -it: typed input reaches the shell and its output comes back", b"101" in got and b"it-42" in got)
check("run -it: the container exits when the shell does", wait_state(itid, "exited"))
call("DELETE", f"/containers/{itid}?force=1")

# ---- events (a second client watches while we start and stop a container)
seen = []
def watch():
    c = Conn()
    c.request("GET", f"/v1.43/events?until={int(time.time()) + 8}")
    r = c.getresponse()
    for line in r.read().decode().splitlines():
        if line.strip():
            seen.append(json.loads(line))
t = threading.Thread(target=watch)
t.start()
time.sleep(1)
st, c = call("POST", "/containers/create?name=apievents", {"Image": "alpine", "Cmd": ["sleep", "300"], "HostConfig": {"NetworkMode": "none"}})
ev = c["Id"]
call("POST", f"/containers/{ev}/start")
wait_state(ev, "running")
call("POST", f"/containers/{ev}/stop?t=1")
wait_state(ev, "exited")
call("DELETE", f"/containers/{ev}")
t.join()
acts = [(x["Action"], x["id"]) for x in seen if x["Type"] == "container"]
check("events reports create, start, die and destroy", all((a, ev) in acts for a in ("create", "start", "die", "destroy")))
check("events carry the image and name", any(x["Actor"]["Attributes"].get("name") == "apievents" for x in seen))
call("DELETE", f"/containers/{xid}?force=1")

# ---- attach
st, c = call("POST", "/containers/create", {"Image": "alpine", "Cmd": ["sh", "-c", "sleep 2; echo attached-out"], "HostConfig": {"NetworkMode": "none"}})
at = c["Id"]
call("POST", f"/containers/{at}/start")
st, raw = call("POST", f"/containers/{at}/attach?stream=1&stdout=1&stderr=1", raw=True)
check("attach streams the container output until it exits", st == 200 and "attached-out" in frames(raw))
call("DELETE", f"/containers/{at}?force=1")

# ---- build
def tar_of(files):
    b = io.BytesIO()
    with tarfile.open(fileobj=b, mode="w") as t:
        for name, data in files.items():
            ti = tarfile.TarInfo(name)
            ti.size = len(data)
            t.addfile(ti, io.BytesIO(data))
    return b.getvalue()
ctx = tar_of({"Dockerfile": "\n".join(["FROM alpine", "COPY msg /msg", "RUN echo built-ok > /built", 'CMD ["cat", "/built", "/msg"]', ""]).encode(), "msg": b"from-context" + bytes([10])})
st, raw = call("POST", "/build?t=apibuilt:1", ctx, raw=True)
lines = [json.loads(l) for l in raw.decode().splitlines() if l.strip()]
check("build streams progress and ends with Successfully tagged", st == 200 and lines and "Successfully tagged apibuilt:1" in lines[-1].get("stream", "") and not any("error" in l for l in lines))
st, img = call("GET", "/images/apibuilt:1/json")
check("the built image exists", st == 200)
st, c = call("POST", "/containers/create", {"Image": "apibuilt:1", "HostConfig": {"NetworkMode": "none"}})
call("POST", f"/containers/{c['Id']}/start")
call("POST", f"/containers/{c['Id']}/wait")
st, raw = call("GET", f"/containers/{c['Id']}/logs?stdout=1", raw=True)
check("running the built image shows the context file and the RUN output", "built-ok" in frames(raw) and "from-context" in frames(raw))
call("DELETE", f"/containers/{c['Id']}?force=1")
call("DELETE", "/images/apibuilt:1")
bad = tar_of({"Dockerfile": "\n".join(["FROM alpine", "RUN exit 5", ""]).encode()})
st, raw = call("POST", "/build?t=apibad:1", bad, raw=True)
lines = [json.loads(l) for l in raw.decode().splitlines() if l.strip()]
check("a failing build reports an error object", any("error" in l for l in lines))
st, raw = call("POST", "/build", ctx, raw=True)
check("build without a tag is 400", st == 400)
st, e = call("GET", "/containers/zzz/json")
check("a missing container is 404 with a message", st == 404 and "No such container" in e["message"])

# ---- engine API completeness: top, stats, changes, archive, export, commit, prune, df, history, load/get, stdin, filters
import base64, urllib.parse

def exec_in(cid, cmd):
    st, e = call("POST", f"/containers/{cid}/exec", {"Cmd": cmd, "AttachStdout": True, "AttachStderr": True})
    st, raw = call("POST", f"/exec/{e['Id']}/start", {"Detach": False}, raw=True)
    return frames(raw)

def tar_members(data):
    with tarfile.open(fileobj=io.BytesIO(data)) as t:
        return {m.name: (t.extractfile(m).read() if m.isfile() else None) for m in t.getmembers()}

st, c = call("POST", "/containers/create?name=apifs", {"Image": "alpine", "Cmd": ["sleep", "300"], "Labels": {"suite": "api", "tier": "t1"}, "HostConfig": {"NetworkMode": "none"}})
fid = c["Id"]
st, e = call("GET", f"/containers/{fid}/top")
check("top on a stopped container is 409", st == 409)
st, e = call("GET", f"/containers/{fid}/changes")
check("changes on a stopped container is 409", st == 409)
call("POST", f"/containers/{fid}/start")
wait_state(fid, "running")

st, t = call("GET", f"/containers/{fid}/top")
check("top lists the processes with titles", st == 200 and t["Titles"][:2] == ["UID", "PID"] and any("sleep" in p[-1] for p in t["Processes"]))
st, s1 = call("GET", f"/containers/{fid}/stats?stream=false")
check("stats?stream=false returns one object with cpu, memory and pids", st == 200 and s1["memory_stats"]["usage"] > 0 and s1["cpu_stats"]["online_cpus"] >= 1 and s1["pids_stats"]["current"] >= 1 and s1["name"] == "/apifs" and s1["cpu_stats"]["system_cpu_usage"] > 0)
st, s2 = call("GET", f"/containers/{fid}/stats?stream=false&one-shot=true")
check("one-shot stats have no previous sample", st == 200 and s2["precpu_stats"]["cpu_usage"]["total_usage"] == 0)
c1 = Conn()
c1.request("GET", f"/v1.43/containers/{fid}/stats")
r1 = c1.getresponse()
buf, objs = b"", []
while len(objs) < 2:
    d = r1.fp.readline()
    if not d:
        break
    d = d.strip()
    if d.startswith(b"{"):
        objs.append(json.loads(d))
c1.close()
check("streaming stats keep sending objects (one per second)", len(objs) == 2 and objs[1]["read"] >= objs[0]["read"] and objs[1]["precpu_stats"]["cpu_usage"]["total_usage"] == objs[0]["cpu_stats"]["cpu_usage"]["total_usage"])

# filesystem changes, then archive / export / commit
exec_in(fid, ["sh", "-c", "echo changed > /newfile && mkdir -p /srv/d && echo deep > /srv/d/f && rm /etc/motd; echo more >> /etc/hostname-extra"])
st, ch = call("GET", f"/containers/{fid}/changes")
kinds = {x["Path"]: x["Kind"] for x in ch} if st == 200 else {}
check("changes reports added (1), and deleted (2) files, no runtime mounts", kinds.get("/newfile") == 1 and kinds.get("/srv/d/f") == 1 and kinds.get("/etc/motd") == 2 and "/proc" not in kinds and "/etc/resolv.conf" not in kinds and "/dev" not in kinds)
c2 = Conn()
c2.request("HEAD", f"/v1.43/containers/{fid}/archive?path=/newfile")
r2 = c2.getresponse(); r2.read()
stat = json.loads(base64.b64decode(r2.getheader("X-Docker-Container-Path-Stat", "e30=")))
check("HEAD archive returns the path stat header", r2.status == 200 and stat.get("name") == "newfile" and stat.get("size") == 8)
c2.close()
st, raw = call("GET", f"/containers/{fid}/archive?path=/newfile", raw=True)
check("GET archive of a file is a tar with it", st == 200 and tar_members(raw).get("newfile") == b"changed\n")
st, raw = call("GET", f"/containers/{fid}/archive?path=/srv", raw=True)
mem = tar_members(raw) if st == 200 else {}
check("GET archive of a directory has its contents under the dir name", mem.get("srv/d/f") == b"deep\n")
st, e = call("GET", f"/containers/{fid}/archive?path=/no/such/file")
check("GET archive of a missing path is 404", st == 404)
put = tar_of({"hello.txt": b"put-in\n"})
st, _ = call("PUT", f"/containers/{fid}/archive?path=/srv", put)
check("PUT archive extracts into the container", st == 200 and exec_in(fid, ["cat", "/srv/hello.txt"]).strip() == "put-in")
st, e = call("PUT", f"/containers/{fid}/archive?path=/newfile", put)
check("PUT archive to a non-directory is 404", st == 404)
st, raw = call("GET", f"/containers/{fid}/export", raw=True)
mem = tar_members(raw) if st == 200 else {}
check("export is the whole filesystem as a tar", st == 200 and mem.get("newfile") == b"changed\n" and any(n.startswith("bin/") or n == "bin" for n in mem) and "etc/motd" not in mem)

st, r = call("POST", f"/commit?container={fid}&repo=apicommitted&tag=v1&changes=" + urllib.parse.quote("CMD [\"cat\", \"/newfile\"]") + "&changes=" + urllib.parse.quote("ENV K=V"), {"Labels": {"made": "by-commit"}})
check("commit returns 201 and an image id", st == 201 and r.get("Id", "").startswith("sha256:"))
st, img = call("GET", "/images/apicommitted:v1/json")
check("the committed image exists and has the config changes", st == 200 and img["Config"]["Cmd"] == ["cat", "/newfile"] and "K=V" in img["Config"]["Env"] and img["Config"]["Labels"].get("made") == "by-commit" and len(img["RootFS"]["Layers"]) >= 2)
st, h = call("GET", "/images/apicommitted:v1/history")
check("history lists one row per layer, newest first with the tag", st == 200 and len(h) == len(img["RootFS"]["Layers"]) and h[0]["Tags"] == ["apicommitted:v1"])
st, cc = call("POST", "/containers/create", {"Image": "apicommitted:v1", "HostConfig": {"NetworkMode": "none"}})
cc_id = cc["Id"]
call("POST", f"/containers/{cc_id}/start")
call("POST", f"/containers/{cc_id}/wait")
st, raw = call("GET", f"/containers/{cc_id}/logs?stdout=1", raw=True)
check("a container from the committed image sees the committed file", "changed" in frames(raw))
call("DELETE", f"/containers/{cc_id}?force=1")
st, cc = call("POST", "/containers/create", {"Image": "apicommitted:v1", "Cmd": ["sh", "-c", "ls /srv/d /etc/motd 2>&1; cat /srv/d/f"], "HostConfig": {"NetworkMode": "none"}})
call("POST", f"/containers/{cc['Id']}/start")
call("POST", f"/containers/{cc['Id']}/wait")
st, raw = call("GET", f"/containers/{cc['Id']}/logs?stdout=1&stderr=1", raw=True)
out = frames(raw)
check("the committed layer keeps nested files and applies deletions (whiteout)", "deep" in out and "No such file" in out)
call("DELETE", f"/containers/{cc['Id']}?force=1")
st, e = call("POST", "/commit?container=nosuch&repo=x")
check("commit of a missing container is 404", st == 404)

# save / load round trip through /images/{name}/get and /images/load
st, tarball = call("GET", "/images/apicommitted:v1/get", raw=True)
check("images/{name}/get returns a tar archive", st == 200 and len(tarball) > 1000 and "index.json" in tar_members(tarball))
st, tarball2 = call("GET", "/images/get?names=apicommitted:v1&names=alpine:latest", raw=True)
check("images/get with several names", st == 200 and len(tarball2) > len(tarball) // 2)
call("DELETE", "/images/apicommitted:v1")
check("image gone before load", call("GET", "/images/apicommitted:v1/json")[0] == 404)
st, raw = call("POST", "/images/load", tarball, raw=True)
check("images/load reports Loaded image", st == 200 and b"Loaded image" in raw)
check("the loaded image is back", call("GET", "/images/apicommitted:v1/json")[0] == 200)
st, e = call("GET", "/images/nosuch:1/get")
check("get of a missing image is 404", st == 404)
st, e = call("GET", "/images/nosuch:1/history")
check("history of a missing image is 404", st == 404)
st, r = call("GET", "/images/search?term=alpine")
check("images/search answers with an (empty) list", st == 200 and r == [])

# filters on containers/json, labels, df
st, l = call("GET", "/containers/json?filters=" + urllib.parse.quote(json.dumps({"label": ["suite=api"]})))
check("containers/json filters by label", st == 200 and [x["Id"] for x in l] == [fid] and l[0]["Labels"]["tier"] == "t1")
st, l = call("GET", "/containers/json?filters=" + urllib.parse.quote(json.dumps({"label": ["suite=other"]})))
check("a label filter that matches nothing is empty", l == [])
st, l = call("GET", "/containers/json?filters=" + urllib.parse.quote(json.dumps({"name": ["apifs"], "status": ["running"], "ancestor": ["alpine"]})))
check("containers/json filters by name, status and ancestor", [x["Id"] for x in l] == [fid])
st, l = call("GET", "/containers/json?filters=" + urllib.parse.quote(json.dumps({"status": ["exited"], "name": ["apifs"]})))
check("a status filter that does not match is empty", l == [])
st, j = call("GET", f"/containers/{fid}/json")
check("inspect shows the labels", j["Config"]["Labels"].get("suite") == "api")
st, d = call("GET", "/system/df")
check("system/df has images, containers, volumes and sizes", st == 200 and d["Images"] and d["LayersSize"] > 0 and any(x["Id"] == fid for x in d["Containers"]) and "Volumes" in d and "BuildCache" in d)

# resize of a terminal container: the shell asks its own terminal for the size
st, c = call("POST", "/containers/create?name=apitty", {"Image": "alpine", "Cmd": ["sh", "-c", "sleep 3; stty size"], "Tty": True, "OpenStdin": True, "HostConfig": {"NetworkMode": "none"}})
tid = c["Id"]
call("POST", f"/containers/{tid}/start")
wait_state(tid, "running")
time.sleep(1)
st, _ = call("POST", f"/containers/{tid}/resize?h=33&w=101")
call("POST", f"/containers/{tid}/wait")
st2, raw = call("GET", f"/containers/{tid}/logs?stdout=1", raw=True)
call("DELETE", f"/containers/{tid}?force=1")
check("resize sets the size of the container's terminal", st == 200 and "33 101" in raw.decode(errors="replace"))

# stdin for a container started without a terminal (what `docker run -i` does): attach before start, send input, half-close
def attach_stdin(once):
    st, c = call("POST", "/containers/create", {"Image": "alpine", "Cmd": ["sh", "-c", "cat; echo cat-finished"], "OpenStdin": True, "StdinOnce": once, "AttachStdin": True, "AttachStdout": True, "HostConfig": {"NetworkMode": "none"}})
    sid = c["Id"]
    a = socket.socket(socket.AF_UNIX); a.connect(SOCK); a.settimeout(20)
    a.sendall(b"POST /v1.43/containers/" + sid.encode() + b"/attach?stream=1&stdin=1&stdout=1&stderr=1 HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 0\r\n\r\n")
    got = a.recv(4096)
    call("POST", f"/containers/{sid}/start")
    time.sleep(1.5)
    a.sendall(b"typed-into-stdin\n")
    time.sleep(0.5)
    a.shutdown(socket.SHUT_WR)  # the client is done typing
    try:
        while True:
            d = a.recv(4096)
            if not d:
                break
            got += d
    except socket.timeout:
        pass
    a.close()
    return sid, got
sid, got = attach_stdin(True)
check("attach stdin: input reaches a container without a terminal, EOF ends cat, and the stream closes", b"101" in got and b"typed-into-stdin" in got and b"cat-finished" in got and wait_state(sid, "exited", 10))
st, j = call("GET", f"/containers/{sid}/json")
check("OpenStdin shows in inspect", j["Config"].get("OpenStdin") is True)
call("DELETE", f"/containers/{sid}?force=1")
st, c = call("POST", "/containers/create", {"Image": "alpine", "Cmd": ["cat"], "OpenStdin": True, "HostConfig": {"NetworkMode": "none"}})
kid = c["Id"]
call("POST", f"/containers/{kid}/start")
time.sleep(2)
check("a container with OpenStdin and nobody attached keeps running (stdin is open, not /dev/null)", wait_state(kid, "running", 3))
call("DELETE", f"/containers/{kid}?force=1")

# events filters
seen2 = []
def watch2():
    c = Conn()
    c.request("GET", f"/v1.43/events?until={int(time.time()) + 6}&filters=" + urllib.parse.quote(json.dumps({"type": ["container"], "event": ["die"], "container": ["apiefilter"]})))
    r = c.getresponse()
    for line in r.read().decode().splitlines():
        if line.strip():
            seen2.append(json.loads(line))
t2 = threading.Thread(target=watch2)
t2.start()
time.sleep(1)
for nm in ("apiefilter", "apiother"):
    st, c = call("POST", f"/containers/create?name={nm}", {"Image": "alpine", "Cmd": ["true"], "HostConfig": {"NetworkMode": "none"}})
    call("POST", f"/containers/{c['Id']}/start")
    call("POST", f"/containers/{c['Id']}/wait")
t2.join()
check("events filters (type, event, container) pass only the matching events", len(seen2) == 1 and seen2[0]["Action"] == "die" and seen2[0]["Actor"]["Attributes"]["name"] == "apiefilter")
st, ev = call("GET", f"/events?until={int(time.time())}&since=0&filters=" + urllib.parse.quote(json.dumps({"type": ["network"]})))
check("an events filter on another type yields nothing", st == 200 and ev == "")

# prune endpoints
call("DELETE", f"/containers/{fid}?force=1")
st, r = call("POST", "/containers/prune")
check("containers/prune removes stopped containers", st == 200 and len(r["ContainersDeleted"]) >= 2 and call("GET", "/containers/json?all=1")[1] == [])
call("POST", "/volumes/create", {"Name": "apiprunevol"})
st, r = call("POST", "/volumes/prune")
check("volumes/prune removes unused volumes", st == 200 and "apiprunevol" in r["VolumesDeleted"])
call("POST", "/networks/create", {"Name": "apiprunenet"})
st, r = call("POST", "/networks/prune")
check("networks/prune removes unused networks", st == 200 and "apiprunenet" in r["NetworksDeleted"])
st, r = call("POST", "/images/prune")
check("images/prune (dangling) removes nothing here", st == 200 and r["ImagesDeleted"] == [])
st, r = call("POST", "/images/prune?filters=" + urllib.parse.quote(json.dumps({"dangling": ["false"]})))
names_left = [t for x in call("GET", "/images/json")[1] for t in x["RepoTags"]]
check("images/prune with dangling=false removes unused images", st == 200 and any(x["Untagged"] == "apicommitted:v1" for x in r["ImagesDeleted"]) and names_left == [])

print("ALL PASSED" if fails == 0 else f"{fails} FAILED")
sys.exit(1 if fails else 0)
