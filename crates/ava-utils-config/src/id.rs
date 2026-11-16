use crate::error::{Error, Result};

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| Error::Protocol(format!("no randomness: {e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn new_request_id() -> Result<String> {
    random_hex(8)
}

pub fn new_epoch() -> Result<u64> {
    loop {
        let mut buf = [0u8; 8];
        getrandom::fill(&mut buf).map_err(|e| Error::Protocol(format!("no randomness: {e}")))?;
        let n = u64::from_le_bytes(buf);
        if n != 0 {
            return Ok(n);
        }
    }
}

pub fn new_job_id() -> Result<String> {
    random_hex(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epochs_are_never_zero_and_differ() {
        let a = new_epoch().unwrap();
        let b = new_epoch().unwrap();
        assert!(a != 0 && b != 0 && a != b);
    }

    #[test]
    fn job_ids_are_six_hex_chars() {
        let id = new_job_id().unwrap();
        assert_eq!(id.len(), 6);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn request_ids_are_sixteen_hex_chars_and_differ() {
        let a = new_request_id().unwrap();
        let b = new_request_id().unwrap();
        assert_eq!(a.len(), 16);
        assert_ne!(a, b);
    }
}
