//! Unix domain socket I/O that carries file descriptors alongside bytes,
//! via `recvmsg`/`sendmsg` with `SCM_RIGHTS` ancillary data.
//!
//! `tokio::net::UnixStream`'s plain `read`/`write` do NOT carry ancillary
//! data at all -- that's the actual bug that motivated moving off a raw
//! byte pipe in the first place (Wayland routinely sends fds: shm pool
//! memory, keymaps, DnD/clipboard pipes). Going back to hand-rolled wire
//! parsing without fixing this would silently reintroduce it. sommelier-rs
//! and waypipe (see reference/) both use this same approach.

use nix::cmsg_space;
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// Enough for any Wayland message in practice; libwayland itself caps
/// fds-per-message well below this. Oversized is harmless -- it's just a
/// buffer size, not a hard protocol limit we're enforcing.
const MAX_FDS_PER_RECV: usize = 28;

/// Reads once from `fd` into `buf`, returning the number of bytes read and
/// any file descriptors that rode alongside them via `SCM_RIGHTS`.
///
/// Like a plain `read()`, this can return fewer bytes than a full message,
/// or bytes spanning multiple messages -- framing is the caller's job (see
/// `wire::take_message`). Returns `Ok((0, []))` on EOF, matching `read()`'s
/// convention.
pub fn recv_with_fds(fd: RawFd, buf: &mut [u8]) -> nix::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = [IoSliceMut::new(buf)];
    let mut cmsg_buffer = cmsg_space!([RawFd; MAX_FDS_PER_RECV]);
    let msg = recvmsg::<UnixAddr>(fd, &mut iov, Some(&mut cmsg_buffer), MsgFlags::empty())?;

    let mut fds = Vec::new();
    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(raw_fds) = cmsg {
            // SAFETY: these fds were just handed to us by the kernel via
            // this recvmsg call; we're the sole owner and responsible for
            // eventually closing them (OwnedFd does that on drop).
            fds.extend(raw_fds.into_iter().map(|raw| unsafe { OwnedFd::from_raw_fd(raw) }));
        }
    }
    Ok((msg.bytes, fds))
}

/// Writes `buf` to `fd`, attaching `fds` as `SCM_RIGHTS` ancillary data if
/// non-empty. Like a plain `write()`, may write fewer bytes than `buf.len()`
/// -- callers doing a partial framed write need to handle that themselves,
/// though in practice one message per `sendmsg` call keeps this simple.
pub fn send_with_fds(fd: RawFd, buf: &[u8], fds: &[RawFd]) -> nix::Result<usize> {
    let iov = [IoSlice::new(buf)];
    if fds.is_empty() {
        sendmsg::<UnixAddr>(fd, &iov, &[], MsgFlags::empty(), None)
    } else {
        let cmsg = [ControlMessage::ScmRights(fds)];
        sendmsg::<UnixAddr>(fd, &iov, &cmsg, MsgFlags::empty(), None)
    }
}
