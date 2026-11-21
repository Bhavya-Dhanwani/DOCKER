#!/usr/bin/env python3
"""Run a command on a real pseudo-terminal, type into it, print what comes back and the exit code.

usage: ptydrive.py <rows> <cols> <keystrokes-file> -- <command...>
Keystrokes are sent one line at a time with a short pause (like a person typing).
"""
import fcntl, os, pty, select, struct, sys, termios, time

rows, cols, keys = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
cmd = sys.argv[sys.argv.index("--") + 1:]
lines = open(keys).read().splitlines(keepends=True) if keys != "-" else []

pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    os.environ["TERM"] = "xterm"
    os.execvp(cmd[0], cmd)

out = b""
deadline = time.time() + 40
pending = list(lines)
last_send = time.time() + 1.0
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 0.1)
    if r:
        try:
            data = os.read(fd, 4096)
        except OSError:
            break
        if not data:
            break
        out += data
    if pending and time.time() - last_send > 0.5:
        os.write(fd, pending.pop(0).encode())
        last_send = time.time()
_, status = os.waitpid(pid, 0)
sys.stdout.write(out.decode(errors="replace").replace("\r\n", "\n"))
sys.stdout.write("\n[exit %d]\n" % (os.waitstatus_to_exitcode(status)))
