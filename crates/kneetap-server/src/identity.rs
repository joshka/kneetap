use std::time::SystemTime;

use crate::host::Host;

/// What identifies a running server.
#[derive(Debug)]
pub struct Identity {
    pub name: Option<String>,
    pub host: Host,
    started_at: SystemTime,
    commit: Option<String>,
}

impl Identity {
    /// Starts identifying a server that begins serving now.
    pub fn new(name: Option<String>, host: Host, commit: Option<String>) -> Self {
        Self {
            name,
            host,
            started_at: SystemTime::now(),
            commit,
        }
    }

    /// When the server started serving, in RFC 3339.
    pub fn started_at(&self) -> String {
        timestamp(self.started_at)
    }

    /// The commit the embedding program was built from, when it said.
    pub fn commit(&self) -> Option<String> {
        self.commit.clone()
    }
}

/// Returns 8 random letters and digits: short to read, yet unlikely to repeat across servers.
///
/// # Errors
///
/// Returns an error if the OS can't provide random bytes.
pub fn random_id() -> Result<String, getrandom::Error> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut bits = getrandom::u64()?;
    let mut id = String::with_capacity(8);
    for _ in 0..8 {
        // 62^8 is far below 2^64, so the remainders stay close to uniform.
        id.push(char::from(ALPHABET[(bits % 62) as usize]));
        bits /= 62;
    }
    Ok(id)
}

/// When a request arrived and the ID its report carries, recorded before it waits for anything.
#[derive(Clone, Debug)]
pub struct Arrival {
    pub at: SystemTime,
    pub request_id: String,
}

impl Arrival {
    /// Records a request arriving now.
    ///
    /// The ID is random rather than counted, so reports don't reveal how busy the server is.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS can't provide random bytes.
    pub fn now() -> Result<Self, getrandom::Error> {
        Ok(Self {
            at: SystemTime::now(),
            request_id: random_id()?,
        })
    }
}

/// Formats a time as RFC 3339 in UTC, to the millisecond.
pub fn timestamp(time: SystemTime) -> String {
    humantime::format_rfc3339_millis(time).to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn random_ids_are_eight_letters_and_digits() {
        let id = random_id().unwrap();

        assert_eq!(id.len(), 8, "{id}");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()), "{id}");
    }

    #[test]
    fn timestamps_are_rfc3339_utc_millis() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(1_800_000_000_123);

        assert_eq!(timestamp(time), "2027-01-15T08:00:00.123Z");
    }
}
