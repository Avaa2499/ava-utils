use std::io::{Error as IoError, ErrorKind};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::error::{Error, Result};

pub const MAX_FDS: usize = 8;

fn space(n: usize) -> usize {
    unsafe { libc::CMSG_SPACE((n * size_of::<RawFd>()) as u32) as usize }
}

pub fn send_fds(stream: &UnixStream, fds: &[BorrowedFd<'_>]) -> Result<()> {
    if fds.is_empty() || fds.len() > MAX_FDS {
        return Err(Error::Protocol(format!(
            "can pass 1 to {MAX_FDS} files, not {}",
            fds.len()
        )));
    }
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut buf = vec![0u8; space(raw.len())];
    unsafe {
        let mut msg: libc::msghdr = zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = buf.as_mut_ptr().cast();
        msg.msg_controllen = buf.len() as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN((raw.len() * size_of::<RawFd>()) as u32) as _;
        std::ptr::copy_nonoverlapping(
            raw.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            raw.len() * size_of::<RawFd>(),
        );
        loop {
            let n = libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL);
            if n == 1 {
                return Ok(());
            }
            let err = IoError::last_os_error();
            if n < 0 && err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(if n < 0 {
                err.into()
            } else {
                Error::Protocol("short write while passing files".into())
            });
        }
    }
}

pub fn recv_fds(stream: &UnixStream, count: usize) -> Result<Vec<OwnedFd>> {
    if count == 0 || count > MAX_FDS {
        return Err(Error::Protocol(format!(
            "a message cannot carry {count} files (limit {MAX_FDS})"
        )));
    }
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut buf = vec![0u8; space(MAX_FDS)];
    let mut got: Vec<OwnedFd> = Vec::new();
    let n;
    unsafe {
        let mut msg: libc::msghdr = zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = buf.as_mut_ptr().cast();
        msg.msg_controllen = buf.len() as _;
        loop {
            let r = libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
            if r < 0 && IoError::last_os_error().kind() == ErrorKind::Interrupted {
                continue;
            }
            if r < 0 {
                return Err(IoError::last_os_error().into());
            }
            n = r;
            break;
        }
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for i in 0..len / size_of::<RawFd>() {
                    got.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if n == 0 {
        return Err(Error::Unavailable);
    }
    if msg_truncated(&got, count) {
        return Err(Error::Protocol(format!(
            "expected {count} files, received {}",
            got.len()
        )));
    }
    Ok(got)
}

fn msg_truncated(got: &[OwnedFd], count: usize) -> bool {
    got.len() != count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsFd;

    fn temp(tag: &str, text: &str) -> (std::path::PathBuf, File) {
        let p = std::env::temp_dir().join(format!("au-fd-{}-{tag}", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let f = File::open(&p).unwrap();
        (p, f)
    }

    #[test]
    fn a_file_arrives_open_and_readable() {
        let (a, b) = UnixStream::pair().unwrap();
        let (p, f) = temp("plain", "hello");
        send_fds(&a, &[f.as_fd()]).unwrap();
        let mut got = File::from(recv_fds(&b, 1).unwrap().remove(0));
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn the_receiver_can_still_read_a_file_deleted_after_the_handover() {
        let (a, b) = UnixStream::pair().unwrap();
        let (p, f) = temp("deleted", "still here");
        send_fds(&a, &[f.as_fd()]).unwrap();
        drop(f);
        std::fs::remove_file(&p).unwrap();
        assert!(!p.exists());
        let mut got = File::from(recv_fds(&b, 1).unwrap().remove(0));
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "still here");
    }

    #[test]
    fn the_receiver_can_still_read_a_file_renamed_after_the_handover() {
        let (a, b) = UnixStream::pair().unwrap();
        let (p, f) = temp("renamed", "moved");
        send_fds(&a, &[f.as_fd()]).unwrap();
        let q = p.with_extension("moved");
        std::fs::rename(&p, &q).unwrap();
        let mut got = File::from(recv_fds(&b, 1).unwrap().remove(0));
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "moved");
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn several_files_arrive_in_order() {
        let (a, b) = UnixStream::pair().unwrap();
        let files: Vec<_> = (0..3)
            .map(|i| temp(&format!("multi{i}"), &format!("f{i}")))
            .collect();
        let borrowed: Vec<_> = files.iter().map(|(_, f)| f.as_fd()).collect();
        send_fds(&a, &borrowed).unwrap();
        for (i, fd) in recv_fds(&b, 3).unwrap().into_iter().enumerate() {
            let mut s = String::new();
            File::from(fd).read_to_string(&mut s).unwrap();
            assert_eq!(s, format!("f{i}"));
        }
        for (p, _) in files {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn the_received_file_shares_the_senders_open_file_description_and_is_close_on_exec() {
        let (a, b) = UnixStream::pair().unwrap();
        let p = std::env::temp_dir().join(format!("au-fd-{}-shared", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&p)
            .unwrap();
        send_fds(&a, &[f.as_fd()]).unwrap();
        let got = recv_fds(&b, 1).unwrap().remove(0);
        let flags = unsafe { libc::fcntl(got.as_raw_fd(), libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0);
        let mut g = File::from(got);
        f.write_all(b"abc").unwrap();
        g.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        g.read_to_string(&mut s).unwrap();
        assert_eq!(s, "abc");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn receiving_the_wrong_number_of_files_is_an_error_and_leaks_nothing() {
        let (a, b) = UnixStream::pair().unwrap();
        let (p, f) = temp("count", "x");
        send_fds(&a, &[f.as_fd()]).unwrap();
        assert!(recv_fds(&b, 2).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_closed_peer_is_unavailable() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        assert!(matches!(recv_fds(&b, 1), Err(Error::Unavailable)));
    }

    #[test]
    fn limits_are_enforced_on_both_sides() {
        let (a, b) = UnixStream::pair().unwrap();
        assert!(send_fds(&a, &[]).is_err());
        let (p, f) = temp("limit", "x");
        let many: Vec<_> = (0..=MAX_FDS).map(|_| f.as_fd()).collect();
        assert!(send_fds(&a, &many).is_err());
        assert!(recv_fds(&b, 0).is_err());
        assert!(recv_fds(&b, MAX_FDS + 1).is_err());
        let _ = std::fs::remove_file(p);
    }
}
