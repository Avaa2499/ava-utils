use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    Unavailable,
    Timeout,
    Remote { code: String, message: String },
    Protocol(String),
    Io(io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unavailable => write!(f, "unavailable"),
            Error::Timeout => write!(f, "no reply within the deadline"),
            Error::Remote { code, message } => write!(f, "{code}: {message}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof => Error::Unavailable,
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout,
            _ => Error::Io(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_socket_is_unavailable() {
        let e: Error = io::Error::from(io::ErrorKind::NotFound).into();
        assert!(matches!(e, Error::Unavailable));
    }

    #[test]
    fn read_timeout_is_timeout() {
        let e: Error = io::Error::from(io::ErrorKind::WouldBlock).into();
        assert!(matches!(e, Error::Timeout));
    }
}
