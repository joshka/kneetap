use std::sync::{Arc, PoisonError, RwLock};

use kneetap_api::ServerStatus;

/// The server's self-reported problems, shared between background tasks and request handlers.
#[derive(Clone, Debug, Default)]
pub struct Status(Arc<RwLock<ServerStatus>>);

impl Status {
    /// Starts with what the caller had to work around at startup.
    pub fn new(degradations: Vec<String>) -> Self {
        Self(Arc::new(RwLock::new(ServerStatus {
            reload_error: None,
            degradations,
        })))
    }

    /// Returns a guard for reading. A panic can't leave this data half-written, so poison is
    /// ignored.
    pub fn read(&self) -> impl std::ops::Deref<Target = ServerStatus> {
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the outcome of the latest certificate reload.
    pub fn set_reload_error(&self, error: Option<String>) {
        let mut status = self.0.write().unwrap_or_else(PoisonError::into_inner);
        status.reload_error = error;
    }
}
