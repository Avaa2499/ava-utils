use rmpv::Value;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::id::new_request_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Call,
    Reply,
    Error,
    Event,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    #[serde(default = "nil")]
    pub body: Value,
}

fn nil() -> Value {
    Value::Nil
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

fn to_body<T: Serialize>(body: &T) -> Result<Value> {
    let bytes = rmp_serde::to_vec_named(body)
        .map_err(|e| Error::Protocol(format!("cannot encode body: {e}")))?;
    rmpv::decode::read_value(&mut bytes.as_slice())
        .map_err(|e| Error::Protocol(format!("cannot encode body: {e}")))
}

impl Envelope {
    pub fn call<T: Serialize>(name: &str, body: &T) -> Result<Self> {
        Ok(Self {
            id: new_request_id()?,
            kind: Kind::Call,
            name: name.to_string(),
            body: to_body(body)?,
        })
    }

    pub fn call_with_id<T: Serialize>(id: &str, name: &str, body: &T) -> Result<Self> {
        Ok(Self {
            id: id.to_string(),
            kind: Kind::Call,
            name: name.to_string(),
            body: to_body(body)?,
        })
    }

    pub fn event<T: Serialize>(name: &str, body: &T) -> Result<Self> {
        Ok(Self {
            id: new_request_id()?,
            kind: Kind::Event,
            name: name.to_string(),
            body: to_body(body)?,
        })
    }

    pub fn reply<T: Serialize>(to: &Envelope, body: &T) -> Result<Self> {
        Ok(Self {
            id: to.id.clone(),
            kind: Kind::Reply,
            name: to.name.clone(),
            body: to_body(body)?,
        })
    }

    pub fn error(to: &Envelope, code: &str, message: &str) -> Result<Self> {
        Ok(Self {
            id: to.id.clone(),
            kind: Kind::Error,
            name: to.name.clone(),
            body: to_body(&ErrorBody {
                code: code.to_string(),
                message: message.to_string(),
            })?,
        })
    }

    pub fn from_error(to: &Envelope, e: &Error) -> Result<Self> {
        Self::error(to, e.code(), &e.wire_message())
    }

    pub fn body_as<T: DeserializeOwned>(&self) -> Result<T> {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &self.body)
            .map_err(|e| Error::Protocol(format!("bad body for {}: {e}", self.name)))?;
        rmp_serde::from_slice(&bytes)
            .map_err(|e| Error::Protocol(format!("bad body for {}: {e}", self.name)))
    }

    pub fn into_result(self) -> Result<Envelope> {
        if self.kind == Kind::Unknown {
            return Err(Error::Protocol(format!(
                "{}: message kind not understood by this module",
                self.name
            )));
        }
        if self.kind == Kind::Error {
            let b: ErrorBody = self.body_as()?;
            return Err(Error::Remote {
                code: b.code,
                message: b.message,
            });
        }
        Ok(self)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(self).map_err(|e| Error::Protocol(format!("cannot encode: {e}")))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        rmp_serde::from_slice(bytes).map_err(|e| Error::Protocol(format!("cannot decode: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Lookup {
        paths: Vec<String>,
        #[serde(default)]
        total_size: Option<u64>,
    }

    #[test]
    fn a_kind_from_the_future_decodes_as_unknown_and_is_not_a_success() {
        #[derive(Serialize)]
        struct Future {
            id: String,
            kind: String,
            name: String,
        }
        let bytes = rmp_serde::to_vec_named(&Future {
            id: "abc123".into(),
            kind: "stream".into(),
            name: "x".into(),
        })
        .unwrap();
        let env = Envelope::decode(&bytes).unwrap();
        assert_eq!(env.kind, Kind::Unknown);
        assert!(matches!(env.into_result(), Err(Error::Protocol(_))));
    }

    #[test]
    fn roundtrip_keeps_named_boxes() {
        let msg = Lookup {
            paths: vec!["/a".into()],
            total_size: Some(7),
        };
        let env = Envelope::call("file-lookup", &msg).unwrap();
        let back = Envelope::decode(&env.encode().unwrap()).unwrap();
        assert_eq!(back, env);
        assert_eq!(back.body_as::<Lookup>().unwrap(), msg);
    }

    #[test]
    fn boxes_travel_with_their_names() {
        let env = Envelope::call(
            "file-lookup",
            &Lookup {
                paths: vec![],
                total_size: None,
            },
        )
        .unwrap();
        let bytes = env.encode().unwrap();
        assert!(bytes.windows(5).any(|w| w == b"paths"));
        assert!(bytes.windows(10).any(|w| w == b"total_size"));
    }

    #[test]
    fn unknown_boxes_are_ignored_and_missing_optional_boxes_default() {
        #[derive(Serialize)]
        struct Newer {
            paths: Vec<String>,
            added_later: u8,
        }
        let env = Envelope::call(
            "file-lookup",
            &Newer {
                paths: vec!["/b".into()],
                added_later: 1,
            },
        )
        .unwrap();
        let got: Lookup = env.body_as().unwrap();
        assert_eq!(got.paths, vec!["/b".to_string()]);
        assert_eq!(got.total_size, None);
    }

    #[test]
    fn reply_keeps_the_request_id() {
        let call = Envelope::call("run-command", &"x").unwrap();
        let reply = Envelope::reply(&call, &"y").unwrap();
        assert_eq!(reply.id, call.id);
        assert_eq!(reply.kind, Kind::Reply);
    }

    #[test]
    fn error_becomes_a_typed_remote_error() {
        let call = Envelope::call("run-command", &"x").unwrap();
        let err = Envelope::error(&call, "bad-input", "nope").unwrap();
        match err.into_result() {
            Err(Error::Remote { code, message }) => {
                assert_eq!(code, "bad-input");
                assert_eq!(message, "nope");
            }
            other => panic!("expected Remote error, got {other:?}"),
        }
    }

    #[test]
    fn garbage_is_a_protocol_error() {
        assert!(matches!(
            Envelope::decode(&[0xc1, 0x00, 0x01]),
            Err(Error::Protocol(_))
        ));
    }
}
