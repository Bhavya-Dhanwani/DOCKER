//! `docker run -t` / `docker exec -t`: give the container a pseudo-terminal and relay it to ours.
use crate::error::{Error, Result};
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::atomic::{AtomicBool, Ordering},
};

/// A unix socket pair between the supervisor and the process that will own the terminal.
pub fn link() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } < 0 {
        return Err(Error::sys("socketpair"));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn send_fd(sock: RawFd, fd: RawFd) -> bool {
    unsafe {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut buf = vec![0u8; space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = buf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping((&fd as *const RawFd).cast::<u8>(), libc::CMSG_DATA(c), std::mem::size_of::<RawFd>());
        libc::sendmsg(sock, &msg, 0) >= 0
    }
}

/// Supervisor side: wait for the container to send the terminal's master end (None if it died first).
pub fn receive_master(link: OwnedFd) -> Option<OwnedFd> {
    unsafe {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut buf = vec![0u8; space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = buf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        if libc::recvmsg(link.as_raw_fd(), &mut msg, 0) <= 0 {
            return None;
        }
        let c = libc::CMSG_FIRSTHDR(&msg);
        if c.is_null() || (*c).cmsg_level != libc::SOL_SOCKET || (*c).cmsg_type != libc::SCM_RIGHTS {
            return None;
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(c), (&mut fd as *mut RawFd).cast::<u8>(), std::mem::size_of::<RawFd>());
        (fd >= 0).then(|| OwnedFd::from_raw_fd(fd))
    }
}

/// Container side (after its /dev is mounted): create the terminal in the container's own devpts, send
/// the master end to the supervisor, and become the terminal's session leader with it as stdio.
pub fn setup_in_container(link: OwnedFd) -> Result<()> {
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if m < 0 {
            return Err(Error::sys("open /dev/ptmx"));
        }
        let master = OwnedFd::from_raw_fd(m);
        let mut name = [0 as libc::c_char; 64];
        if libc::grantpt(m) < 0 || libc::unlockpt(m) < 0 || libc::ptsname_r(m, name.as_mut_ptr(), name.len()) != 0 {
            return Err(Error::sys("prepare terminal"));
        }
        let s = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if s < 0 {
            return Err(Error::sys("open terminal"));
        }
        if !send_fd(link.as_raw_fd(), master.as_raw_fd()) {
            return Err(Error::sys("send terminal to the supervisor"));
        }
        drop(master);
        attach(OwnedFd::from_raw_fd(s))
    }
}

/// In the child: become a session leader with the pty as controlling terminal and as stdin/stdout/stderr.
pub fn attach(slave: OwnedFd) -> Result<()> {
    let fd = slave.as_raw_fd();
    unsafe {
        if libc::setsid() < 0 {
            return Err(Error::sys("setsid"));
        }
        if libc::ioctl(fd, libc::TIOCSCTTY as _, 0) < 0 {
            return Err(Error::sys("controlling terminal"));
        }
        for target in 0..3 {
            if libc::dup2(fd, target) < 0 {
                return Err(Error::sys("dup2 pty"));
            }
        }
    }
    Ok(()) // `slave` closes here; 0/1/2 keep the terminal open
}

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}

fn sync_winsize(master: RawFd) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(0, libc::TIOCGWINSZ as _, &mut ws) } == 0 {
        unsafe { libc::ioctl(master, libc::TIOCSWINSZ as _, &ws) };
    }
}

/// Our terminal in raw mode (every key goes to the container); restored when dropped.
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable() -> RawMode {
        unsafe {
            if libc::isatty(0) == 0 {
                return RawMode(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return RawMode(None);
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            RawMode(Some(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
    }
}

fn write_all(fd: RawFd, mut buf: &[u8]) {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n <= 0 {
            return;
        }
        buf = &buf[n as usize..];
    }
}

fn read(fd: RawFd, buf: &mut [u8]) -> isize {
    unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) }
}

/// Exit code if `pid` has ended (non-blocking).
fn try_wait(pid: i32) -> Option<i32> {
    let mut st = 0;
    (unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) } == pid)
        .then(|| if libc::WIFSIGNALED(st) { 128 + libc::WTERMSIG(st) } else { libc::WEXITSTATUS(st) })
}

/// Where a detached terminal container accepts `attach` clients (the Engine API connects here).
pub fn attach_sock(id: &str) -> std::path::PathBuf {
    std::path::Path::new("/run/rust-docker").join(format!("{id}.tty"))
}

/// The attach socket of a detached terminal container: input from any client goes to the terminal, and
/// everything the terminal prints goes to all clients (a new one first gets the last few KB, like a redraw).
struct AttachSock {
    listener: std::os::unix::net::UnixListener,
    clients: Vec<std::os::unix::net::UnixStream>,
    scrollback: Vec<u8>,
    path: std::path::PathBuf,
}

impl AttachSock {
    fn open(id: &str) -> Option<Self> {
        let path = attach_sock(id);
        let _ = std::fs::create_dir_all(path.parent()?);
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).ok()?;
        listener.set_nonblocking(true).ok()?;
        Some(Self { listener, clients: vec![], scrollback: vec![], path })
    }

    fn fan_out(&mut self, data: &[u8]) {
        self.scrollback.extend_from_slice(data);
        let excess = self.scrollback.len().saturating_sub(8192);
        self.scrollback.drain(..excess);
        self.clients.retain_mut(|c| std::io::Write::write_all(c, data).is_ok());
    }

    fn accept(&mut self) {
        while let Ok((mut c, _)) = self.listener.accept() {
            let _ = c.set_nonblocking(true);
            if std::io::Write::write_all(&mut c, &self.scrollback).is_ok() {
                self.clients.push(c);
            }
        }
    }
}

impl Drop for AttachSock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Shuttle bytes between our terminal and the container's until the container ends. Returns its exit code.
/// With `use_stdin == false` (a detached container) output goes to our stdout, which is its log file, and
/// the terminal stays open: clients that `attach` (see `AttachSock`) can type into it.
pub fn relay(master: OwnedFd, pid: i32, use_stdin: bool, id: &str) -> Result<i32> {
    let m = master.as_raw_fd();
    let _raw = if use_stdin { RawMode::enable() } else { RawMode(None) };
    if use_stdin {
        sync_winsize(m);
        unsafe { libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t) };
    }
    let mut sock = if use_stdin { None } else { AttachSock::open(id) };
    let (mut stdin_open, mut buf) = (use_stdin, [0u8; 8192]);
    loop {
        if WINCH.swap(false, Ordering::Relaxed) {
            sync_winsize(m);
        }
        // poll set: [stdin] master [listener] [clients...]
        let mut fds = Vec::with_capacity(4);
        if stdin_open {
            fds.push(libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 });
        }
        let master_at = fds.len();
        fds.push(libc::pollfd { fd: m, events: libc::POLLIN, revents: 0 });
        let listener_at = fds.len();
        if let Some(sk) = &sock {
            fds.push(libc::pollfd { fd: sk.listener.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            fds.extend(sk.clients.iter().map(|c| libc::pollfd { fd: c.as_raw_fd(), events: libc::POLLIN, revents: 0 }));
        }
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 100) };
        if n > 0 {
            if stdin_open && fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let r = read(0, &mut buf);
                if r > 0 {
                    write_all(m, &buf[..r as usize]);
                } else if r == 0 {
                    stdin_open = false;
                    write_all(m, &[4]); // Ctrl-D: end of input for the container
                }
            }
            if let Some(sk) = sock.as_mut() {
                // clients first (by position, before the list changes), then new connections
                let mut gone = vec![];
                for (i, c) in sk.clients.iter().enumerate() {
                    if fds[listener_at + 1 + i].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                        let r = read(c.as_raw_fd(), &mut buf);
                        if r > 0 {
                            write_all(m, &buf[..r as usize]);
                        } else if r == 0 {
                            gone.push(i);
                        }
                    }
                }
                for i in gone.into_iter().rev() {
                    sk.clients.remove(i);
                }
                if fds[listener_at].revents & libc::POLLIN != 0 {
                    sk.accept();
                }
            }
            if fds[master_at].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let r = read(m, &mut buf);
                if r > 0 {
                    write_all(1, &buf[..r as usize]);
                    if let Some(sk) = sock.as_mut() {
                        sk.fan_out(&buf[..r as usize]);
                    }
                } else {
                    break; // the container closed its end
                }
            }
        }
        if let Some(code) = try_wait(pid) {
            // drain what it printed last
            unsafe { libc::fcntl(m, libc::F_SETFL, libc::O_NONBLOCK) };
            loop {
                let r = read(m, &mut buf);
                if r <= 0 {
                    break;
                }
                write_all(1, &buf[..r as usize]);
                if let Some(sk) = sock.as_mut() {
                    sk.fan_out(&buf[..r as usize]);
                }
            }
            return Ok(code);
        }
    }
    super::sys::wait(pid)
}
