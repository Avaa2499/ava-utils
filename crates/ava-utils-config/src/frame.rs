use std::io::{Read, Write};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use crate::envelope::Envelope;
use crate::error::{Error, Result};

pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

pub fn write_frame(w: &mut impl Write, env: &Envelope) -> Result<()> {
    let bytes = env.encode()?;
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|l| *l <= MAX_FRAME_BYTES)
        .ok_or_else(|| Error::Protocol(format!("frame too large: {} bytes", bytes.len())))?;
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes);
    w.write_all(&out)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame(r: &mut impl Read) -> Result<Envelope> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(Error::Protocol(format!("frame too large: {len} bytes")));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    let env = Envelope::decode(&buf)?;
    if env.fds > 0 {
        return Err(Error::Protocol(
            "this message carries files, which this reader cannot receive".into(),
        ));
    }
    Ok(env)
}

pub fn write_frame_with_files(
    stream: &mut UnixStream,
    env: &Envelope,
    files: &[BorrowedFd<'_>],
) -> Result<()> {
    if files.is_empty() {
        return write_frame(stream, env);
    }
    let mut env = env.clone();
    env.fds = u8::try_from(files.len()).map_err(|_| Error::Protocol("too many files".into()))?;
    write_frame(stream, &env)?;
    crate::fdpass::send_fds(stream, files)
}

pub fn read_frame_with_files(stream: &mut UnixStream) -> Result<(Envelope, Vec<OwnedFd>)> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(Error::Protocol(format!("frame too large: {len} bytes")));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf)?;
    let env = Envelope::decode(&buf)?;
    let files = match env.fds {
        0 => Vec::new(),
        n => crate::fdpass::recv_fds(stream, usize::from(n))?,
    };
    Ok((env, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_and_stay_in_order() {
        let a = Envelope::call("one", &1u8).unwrap();
        let b = Envelope::call("two", &2u8).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &a).unwrap();
        write_frame(&mut wire, &b).unwrap();
        let mut r = wire.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), a);
        assert_eq!(read_frame(&mut r).unwrap(), b);
    }

    #[test]
    fn closed_stream_is_unavailable() {
        let mut r: &[u8] = &[];
        assert!(matches!(read_frame(&mut r), Err(Error::Unavailable)));
    }

    #[test]
    fn truncated_frame_is_unavailable() {
        let a = Envelope::call("one", &1u8).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &a).unwrap();
        wire.truncate(wire.len() - 1);
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(Error::Unavailable)
        ));
    }

    #[test]
    fn oversized_length_is_refused_without_allocating() {
        let wire = (MAX_FRAME_BYTES + 1).to_be_bytes();
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(Error::Protocol(_))
        ));
    }
}
