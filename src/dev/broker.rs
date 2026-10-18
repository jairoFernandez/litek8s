//! Host listeners for a cluster in a sandbox.
//!
//! Inside the dev sandbox the cluster has its own network namespace, so a
//! NodePort it listens on is not on the host's localhost. Instead of copying
//! every connection through a forwarder, the cluster asks the supervisor
//! (still in the host's namespaces) over a Unix socket to bind the port on
//! the host's loopback, and gets the listening socket back as a file
//! descriptor (SCM_RIGHTS). A socket keeps the namespace it was created in,
//! so the cluster accepts host connections on it directly.
//!
//! Requests are one line, `tcp <port>`; the reply is one byte, `0` with the
//! descriptor attached or `1` followed by an error line.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};

static BROKER: OnceLock<Mutex<UnixStream>> = OnceLock::new();

/// Use `fd` (inherited from the supervisor) for host listeners.
pub fn install(fd: RawFd) {
    // SAFETY: the descriptor was handed to this process for this purpose.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let _ = BROKER.set(Mutex::new(stream));
}

/// Whether this process runs in a dev sandbox with a broker.
pub fn installed() -> bool {
    BROKER.get().is_some()
}

/// A listener on the host's 127.0.0.1:`port`, or `None` outside a sandbox
/// or when the port is taken there.
pub fn bind_host_port(port: u16) -> Option<std::net::TcpListener> {
    let broker = BROKER.get()?;
    let mut stream = broker.lock().unwrap();
    if writeln!(stream, "tcp {port}").is_err() {
        return None;
    }
    match recv_with_fd(&stream) {
        Ok((b'0', Some(fd))) => {
            let listener = std::net::TcpListener::from(fd);
            listener.set_nonblocking(true).ok()?;
            Some(listener)
        }
        Ok(_) => {
            let mut line = String::new();
            let _ = BufReader::new(&*stream).read_line(&mut line);
            eprintln!("dev: host port {port} not exposed: {}", line.trim());
            None
        }
        Err(e) => {
            eprintln!("dev: host port broker failed: {e}");
            None
        }
    }
}

/// Serve host listener requests on `stream` until it closes (supervisor).
pub fn serve(stream: UnixStream, bind_ip: std::net::IpAddr) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone broker socket"));
    let mut writer = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let reply = match line.trim().split_once(' ') {
            Some(("tcp", port)) => port
                .parse::<u16>()
                .map_err(|e| e.to_string())
                .and_then(|p| bind_reusable(std::net::SocketAddr::new(bind_ip, p)).map_err(|e| e.to_string())),
            _ => Err(format!("bad request {:?}", line.trim())),
        };
        let ok = match reply {
            Ok(listener) => send_with_fd(&writer, b'0', listener.as_raw_fd()).is_ok(),
            Err(e) => writer.write_all(format!("1{e}\n").as_bytes()).is_ok(),
        };
        if !ok {
            return;
        }
    }
}

fn bind_reusable(addr: std::net::SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let socket = socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

fn send_with_fd(stream: &UnixStream, byte: u8, fd: RawFd) -> std::io::Result<()> {
    let mut data = [byte];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: 1 };
    // SAFETY: CMSG_SPACE is a pure computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg = vec![0u8; space];
    // SAFETY: msghdr points at buffers that outlive the call; the control
    // message is filled in through the CMSG macros.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_with_fd(stream: &UnixStream) -> std::io::Result<(u8, Option<OwnedFd>)> {
    let mut data = [0u8; 1];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: 1 };
    let mut cmsg = [0u8; 64];
    // SAFETY: msghdr points at buffers that outlive the call; we walk the
    // control messages the kernel filled in.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr().cast();
        msg.msg_controllen = cmsg.len() as _;
        let n = libc::recvmsg(stream.as_raw_fd(), &mut msg, 0);
        if n <= 0 {
            return Err(if n == 0 { std::io::ErrorKind::UnexpectedEof.into() } else { std::io::Error::last_os_error() });
        }
        let c = libc::CMSG_FIRSTHDR(&msg);
        let fd = (!c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS)
            .then(|| OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd)));
        Ok((data[0], fd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_out_host_listeners() {
        let (client, server) = UnixStream::pair().unwrap();
        std::thread::spawn(move || serve(server, [127, 0, 0, 1].into()));
        let mut client = client;
        // Port 0: any free port, to keep the test hermetic.
        writeln!(client, "tcp 0").unwrap();
        let (byte, fd) = recv_with_fd(&client).unwrap();
        assert_eq!(byte, b'0');
        let listener = std::net::TcpListener::from(fd.unwrap());
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback() && addr.port() != 0);
        let _conn = std::net::TcpStream::connect(addr).unwrap();
        assert!(listener.accept().is_ok());
        writeln!(client, "tcp nope").unwrap();
        let (byte, fd) = recv_with_fd(&client).unwrap();
        assert!(byte == b'1' && fd.is_none());
    }
}
