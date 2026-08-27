//! Unix control transport with an agent-only explicit-credential prelude.
//!
//! Ordinary clients speak HTTP/2 immediately and are authenticated from
//! `SO_PEERCRED`. A privileged snapshot agent first sends one zero byte with
//! explicit `SCM_CREDENTIALS` containing its real PID and root UID/GID. The
//! server requires those credentials to match `SO_PEERCRED`, consumes the byte
//! before handing the stream to Tonic, and records an unforgeable
//! per-connection marker.

use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::{Stream, StreamExt as _};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::server::Connected;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

const PRIVILEGED_PRELUDE: u8 = 0;
const PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);

/// Kernel-authenticated metadata attached to one local control connection.
#[derive(Clone, Debug)]
pub(crate) struct LocalConnectInfo {
    pub(crate) uid: u32,
    pub(crate) privileged_snapshot_agent: bool,
}

/// A classified local connection passed to Tonic.
pub(crate) struct LocalControlStream {
    stream: UnixStream,
    info: LocalConnectInfo,
}

impl Connected for LocalControlStream {
    type ConnectInfo = LocalConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.info.clone()
    }
}

impl AsyncRead for LocalControlStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for LocalControlStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// Classify accepted streams concurrently so one silent peer cannot stall the
/// Unix listener's accept loop.
pub(crate) fn incoming(
    listener: UnixListener,
) -> impl Stream<Item = io::Result<LocalControlStream>> {
    UnixListenerStream::new(listener)
        .map(|accepted| async move {
            match accepted {
                Ok(stream) => classify(stream).await,
                Err(error) => Err(error),
            }
        })
        .buffer_unordered(64)
}

async fn classify(stream: UnixStream) -> io::Result<LocalControlStream> {
    enable_passcred(stream.as_raw_fd())?;
    let peer = stream.peer_cred()?;
    let first = tokio::time::timeout(PRELUDE_TIMEOUT, async {
        loop {
            stream.readable().await?;
            match peek_first_now(stream.as_raw_fd()) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Ok(byte) => return Ok(byte),
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "local control prelude timed out"))??;

    let privileged_snapshot_agent = if first == PRIVILEGED_PRELUDE {
        let credentials = receive_credential_prelude(&stream).await?;
        if !is_privileged_credentials(credentials, peer.pid(), peer.uid(), peer.gid()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "snapshot-agent credentials do not match the root Unix peer",
            ));
        }
        true
    } else {
        false
    };
    Ok(LocalControlStream {
        stream,
        info: LocalConnectInfo {
            uid: peer.uid(),
            privileged_snapshot_agent,
        },
    })
}

fn peek_first_now(fd: RawFd) -> io::Result<u8> {
    let mut byte = 0u8;
    // SAFETY: `byte` is a live one-byte output buffer and `fd` is a live Unix
    // socket. `MSG_PEEK` leaves the byte queued for the selected parser.
    let received = unsafe {
        libc::recv(
            fd,
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    match received {
        -1 => Err(io::Error::last_os_error()),
        0 => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        _ => Ok(byte),
    }
}

fn is_privileged_credentials(
    credentials: libc::ucred,
    peer_pid: Option<libc::pid_t>,
    peer_uid: libc::uid_t,
    peer_gid: libc::gid_t,
) -> bool {
    credentials.pid > 0
        && Some(credentials.pid) == peer_pid
        && credentials.uid == 0
        && credentials.gid == 0
        && credentials.uid == peer_uid
        && credentials.gid == peer_gid
}

fn enable_passcred(fd: RawFd) -> io::Result<()> {
    let enabled: libc::c_int = 1;
    // SAFETY: `fd` is a live Unix socket, and the option pointer names a
    // correctly aligned `c_int` for exactly its initialized size.
    let status = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&enabled as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if status == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn receive_credential_prelude(stream: &UnixStream) -> io::Result<libc::ucred> {
    loop {
        stream.readable().await?;
        match receive_credential_prelude_now(stream.as_raw_fd()) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            result => return result,
        }
    }
}

fn receive_credential_prelude_now(fd: RawFd) -> io::Result<libc::ucred> {
    let mut byte = [0u8; 1];
    // `cmsghdr` requires native alignment; an integer array provides it.
    let mut control = [0usize; 16];
    // SAFETY: Every pointer in the message names a live, writable buffer for
    // the declared length. `recvmsg` initializes at most those lengths. CMSG
    // traversal stays within the kernel-returned `msg_controllen`.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: byte.len(),
        };
        let mut message: libc::msghdr = zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control);
        let received = libc::recvmsg(fd, &mut message, libc::MSG_DONTWAIT);
        if received == -1 {
            return Err(io::Error::last_os_error());
        }
        if received != 1 || byte[0] != PRIVILEGED_PRELUDE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid snapshot-agent credential prelude",
            ));
        }
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET
                && (*header).cmsg_type == libc::SCM_CREDENTIALS
                && (*header).cmsg_len >= libc::CMSG_LEN(size_of::<libc::ucred>() as u32) as usize
            {
                return Ok(std::ptr::read_unaligned(
                    libc::CMSG_DATA(header).cast::<libc::ucred>(),
                ));
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "snapshot-agent prelude has no SCM_CREDENTIALS",
    ))
}

fn send_credential_prelude_now(fd: RawFd) -> io::Result<()> {
    let pid = libc::pid_t::try_from(std::process::id())
        .map_err(|_| io::Error::other("snapshot-agent PID does not fit pid_t"))?;
    let credentials = libc::ucred {
        pid,
        uid: 0,
        gid: 0,
    };
    send_credential_prelude_with_now(fd, credentials)
}

fn send_credential_prelude_with_now(fd: RawFd, credentials: libc::ucred) -> io::Result<()> {
    let mut byte = [PRIVILEGED_PRELUDE];
    // `cmsghdr` requires native alignment; an integer array provides it.
    let mut control = [0usize; 16];
    // SAFETY: The message points to initialized buffers for their exact
    // lengths. The control buffer is large and aligned enough for one
    // `cmsghdr + ucred`; CMSG helpers calculate the in-buffer offsets.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: byte.len(),
        };
        let mut message: libc::msghdr = zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(size_of::<libc::ucred>() as u32) as usize;
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(io::Error::other("cannot construct SCM_CREDENTIALS"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_CREDENTIALS;
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::ucred>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<libc::ucred>(), credentials);
        let sent = libc::sendmsg(fd, &message, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL);
        if sent == -1 {
            return Err(io::Error::last_os_error());
        }
        if sent != 1 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short snapshot-agent credential prelude",
            ));
        }
    }
    Ok(())
}

async fn send_credential_prelude(stream: &UnixStream) -> io::Result<()> {
    loop {
        stream.writable().await?;
        match send_credential_prelude_now(stream.as_raw_fd()) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            result => return result,
        }
    }
}

/// Connect an agent to the ordinary Unix control socket and attach the
/// explicit root credentials before Tonic writes its HTTP/2 preface.
pub async fn connect_privileged_control(path: &Path) -> Result<Channel, io::Error> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("control socket path must be absolute: '{}'", path.display()),
        ));
    }
    let connector_path = PathBuf::from(path);
    Endpoint::from_static("http://localhost")
        .connect_with_connector(service_fn(move |_| {
            let path = connector_path.clone();
            async move {
                let stream = UnixStream::connect(path).await?;
                send_credential_prelude(&stream).await?;
                Ok::<_, io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn only_matching_explicit_root_credentials_are_privileged(
            pid in prop_oneof![Just(0), any::<libc::pid_t>()],
            uid in prop_oneof![Just(0), any::<libc::uid_t>()],
            gid in prop_oneof![Just(0), any::<libc::gid_t>()],
            peer_pid in prop_oneof![Just(0), any::<libc::pid_t>()],
            peer_uid in prop_oneof![Just(0), any::<libc::uid_t>()],
            peer_gid in prop_oneof![Just(0), any::<libc::gid_t>()],
        ) {
            let credentials = libc::ucred { pid, uid, gid };
            prop_assert_eq!(
                is_privileged_credentials(
                    credentials,
                    Some(peer_pid),
                    peer_uid,
                    peer_gid,
                ),
                pid > 0
                    && pid == peer_pid
                    && uid == 0
                    && gid == 0
                    && peer_uid == 0
                    && peer_gid == 0,
            );
        }
    }

    #[tokio::test]
    async fn ancillary_buffers_round_trip_kernel_credentials() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        enable_passcred(receiver.as_raw_fd()).unwrap();
        let peer = receiver.peer_cred().unwrap();
        let credentials = libc::ucred {
            pid: peer.pid().unwrap(),
            uid: peer.uid(),
            gid: peer.gid(),
        };

        send_credential_prelude_with_now(sender.as_raw_fd(), credentials).unwrap();
        let received = receive_credential_prelude(&receiver).await.unwrap();
        assert_eq!(received.pid, credentials.pid);
        assert_eq!(received.uid, credentials.uid);
        assert_eq!(received.gid, credentials.gid);
    }
}
