//! Standard input for detached containers without a terminal (`docker run -di`, API `OpenStdin`).
//! The supervisor gives the container a pipe as stdin; a helper process feeds that pipe from a unix socket
//! (the sibling of `pty::attach_sock`): whatever an Engine API client sends to `<id>.in` becomes the
//! container's input. The first byte of a connection says what happens when that client is done:
//! `b'O'` closes the container's stdin (Docker's StdinOnce), `b'K'` keeps it open for the next client.
use std::{
    io::{self, Read},
    os::{fd::OwnedFd, unix::net::UnixListener},
    path::{Path, PathBuf},
};

pub fn stdin_sock(id: &str) -> PathBuf {
    Path::new("/run/rust-docker").join(format!("{id}.in"))
}

/// Helper-process body: serve clients one after another (ponytail: sequential, a second attach waits).
pub fn serve(id: &str, pipe: OwnedFd) {
    let path = stdin_sock(id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else { return };
    let mut pipe = std::fs::File::from(pipe);
    for mut client in listener.incoming().flatten() {
        let mut mode = [0u8; 1];
        if client.read_exact(&mut mode).is_err() {
            continue;
        }
        let _ = io::copy(&mut client, &mut pipe);
        if mode[0] == b'O' {
            return; // dropping the pipe is EOF for the container
        }
    }
}
