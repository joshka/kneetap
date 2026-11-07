use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::host::Host;

/// What identifies a running server, and the source of its request IDs.
#[derive(Debug)]
pub struct Identity {
    pub name: Option<String>,
    pub host: Host,
    started_at: SystemTime,
    commit: Option<String>,
    prefix: String,
    count: AtomicU64,
}

impl Identity {
    /// Starts identifying a server that begins serving now.
    ///
    /// The ID prefix is random, so IDs don't repeat across restarts or replicas.
    pub fn new(
        name: Option<String>,
        host: Host,
        commit: Option<String>,
    ) -> Result<Self, getrandom::Error> {
        Ok(Self {
            name,
            host,
            started_at: SystemTime::now(),
            commit,
            prefix: random_id()?,
            count: AtomicU64::new(0),
        })
    }

    /// Returns an ID that no other request to this server or any other has.
    pub fn next_request_id(&self) -> String {
        let count = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}-{count}", self.prefix)
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

/// When a request arrived, recorded before it waits for anything else.
#[derive(Clone, Copy, Debug)]
pub struct Arrival(pub SystemTime);

/// Formats a time as RFC 3339 in UTC, to the millisecond.
pub fn timestamp(time: SystemTime) -> String {
    humantime::format_rfc3339_millis(time).to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn request_ids_differ_within_and_across_servers() {
        let first = Identity::new(None, Host::default(), None).unwrap();
        let second = Identity::new(None, Host::default(), None).unwrap();

        let ids = [
            first.next_request_id(),
            first.next_request_id(),
            second.next_request_id(),
        ];

        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[0], ids[2]);
    }

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
