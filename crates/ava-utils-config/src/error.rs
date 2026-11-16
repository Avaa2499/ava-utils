use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    Unavailable,
    Timeout,
    Remote { code: String, message: String },
    Protocol(String),
    Usage(String),
    Io(io::Error),
}

pub mod codes {
    pub const UNAVAILABLE: &str = "unavailable";
    pub const UNKNOWN_CALL: &str = "unknown-call";
    pub const BAD_REQUEST: &str = "bad-request";
    pub const NOT_A_CALL: &str = "not-a-call";
    pub const FAILED: &str = "failed";
}

impl Error {
    pub fn code(&self) -> &str {
        match self {
            Error::Remote { code, .. } => code,
            Error::Usage(_) | Error::Protocol(_) => codes::BAD_REQUEST,
            Error::Unavailable | Error::Timeout => codes::UNAVAILABLE,
            Error::Io(_) => codes::FAILED,
        }
    }

    pub fn wire_message(&self) -> String {
        match self {
            Error::Remote { message, .. } => message.clone(),
            other => other.to_string(),
        }
    }

    pub fn is_code(&self, code: &str) -> bool {
        self.code() == code
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unavailable => write!(f, "unavailable"),
            Error::Timeout => write!(f, "no reply within the deadline"),
            Error::Remote { code, message } => write!(f, "{code}: {message}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Usage(m) => write!(f, "{m}"),
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
    fn every_error_maps_to_one_standard_code() {
        use super::codes::*;
        let io = Error::Io(io::Error::new(io::ErrorKind::PermissionDenied, "no"));
        for (e, code) in [
            (Error::Usage("x".into()), BAD_REQUEST),
            (Error::Protocol("x".into()), BAD_REQUEST),
            (Error::Unavailable, UNAVAILABLE),
            (Error::Timeout, UNAVAILABLE),
            (io, FAILED),
            (
                Error::Remote {
                    code: "custom-code".into(),
                    message: "m".into(),
                },
                "custom-code",
            ),
        ] {
            assert_eq!(e.code(), code, "{e}");
            assert!(e.is_code(code));
        }
    }

    #[test]
    fn a_remote_error_forwards_its_own_message_not_a_wrapped_one() {
        let e = Error::Remote {
            code: "bad-request".into(),
            message: "usage: scan <folder>".into(),
        };
        assert_eq!(e.wire_message(), "usage: scan <folder>");
        assert_eq!(Error::Usage("oops".into()).wire_message(), "oops");
        assert_eq!(Error::Unavailable.wire_message(), "unavailable");
    }

    #[test]
    fn an_error_reply_roundtrips_into_the_same_code_and_message() {
        use crate::envelope::Envelope;
        let call = Envelope::call("anything", &()).unwrap();
        for e in [
            Error::Usage("bad flag".into()),
            Error::Timeout,
            Error::Protocol("p".into()),
        ] {
            let reply = Envelope::from_error(&call, &e).unwrap();
            match reply.into_result() {
                Err(Error::Remote { code, message }) => {
                    assert_eq!(code, e.code());
                    assert_eq!(message, e.wire_message());
                }
                other => panic!("expected a remote error, got {other:?}"),
            }
        }
    }

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
