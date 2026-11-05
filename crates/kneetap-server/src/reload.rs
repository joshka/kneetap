use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

use crate::Error;
use crate::status::Status;
use crate::tls::{Material, TlsConfig};

/// Asks a server to reread its certificates.
///
/// A library shouldn't claim process signals, so a caller that wants `SIGHUP` forwards it here.
#[derive(Clone, Debug, Default)]
pub struct ReloadHandle(Arc<Notify>);

impl ReloadHandle {
    /// Requests a reload. Requests made while one is running are merged.
    pub fn reload(&self) {
        self.0.notify_one();
    }
}

/// The certificates new connections use, which can change while the server runs.
#[derive(Debug)]
pub struct Certificates {
    current: ArcSwap<Material>,
    config: TlsConfig,
    status: Status,
}

impl Certificates {
    pub fn new(material: Material, config: TlsConfig, status: Status) -> Self {
        Self {
            current: ArcSwap::from_pointee(material),
            config,
            status,
        }
    }

    pub fn current(&self) -> Arc<Material> {
        self.current.load_full()
    }

    /// Replaces the certificates if the new ones are valid.
    ///
    /// A failure keeps the working certificates, because an operator's mistake during rotation
    /// shouldn't take a running server down.
    fn reload(&self) {
        match self.current().reload(&self.config) {
            Ok(material) => {
                self.current.store(Arc::new(material));
                self.status.set_reload_error(None);
                tracing::info!("tls.reload.applied");
            }
            Err(error) => {
                tracing::warn!(%error, "tls.reload.rejected");
                self.status.set_reload_error(Some(describe(&error)));
            }
        }
    }
}

/// Includes the cause, because the error alone only says which kind of file was rejected.
fn describe(error: &Error) -> String {
    match std::error::Error::source(error) {
        Some(source) => format!("{error}: {source}"),
        None => error.to_string(),
    }
}

/// Starts the reload tasks, which stop when the returned set is dropped.
pub fn spawn(
    certificates: Arc<Certificates>,
    handle: ReloadHandle,
    interval: Duration,
) -> JoinSet<()> {
    let mut tasks = JoinSet::new();
    if !interval.is_zero() {
        tasks.spawn(watch_files(certificates.clone(), handle.clone(), interval));
    }
    tasks.spawn(apply_reloads(certificates, handle));
    tasks
}

/// Polls rather than using file events, which miss changes on many network and container mounts.
async fn watch_files(certificates: Arc<Certificates>, handle: ReloadHandle, interval: Duration) {
    let mut seen = file_versions(certificates.clone()).await;
    if seen.is_empty() {
        return;
    }
    // The files may have changed since they were loaded, before this first look at them.
    handle.reload();
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let current = file_versions(certificates.clone()).await;
        // A failure may have been transient, and its files won't change again to prompt a retry.
        if current != seen || certificates.status.read().reload_error.is_some() {
            seen = current;
            handle.reload();
        }
    }
}

/// Reads the files on a blocking thread, because a stalled mount would hold up a runtime thread.
async fn file_versions(certificates: Arc<Certificates>) -> Vec<Option<u64>> {
    let versions = tokio::task::spawn_blocking(move || certificates.config.file_versions());
    versions.await.unwrap_or_default()
}

/// Runs one reload at a time, so a slow reload can't be overtaken by an older one.
async fn apply_reloads(certificates: Arc<Certificates>, handle: ReloadHandle) {
    loop {
        handle.0.notified().await;
        let certificates = certificates.clone();
        // Reading files blocks, which would otherwise hold up a runtime thread.
        let _ = tokio::task::spawn_blocking(move || certificates.reload()).await;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    async fn wait_until_cleared(status: &Status) {
        for _ in 0..200 {
            if status.read().reload_error.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the reload error was never cleared");
    }

    #[tokio::test]
    async fn a_failed_reload_is_retried_while_the_files_stay_the_same() {
        let directory = tempfile::tempdir().unwrap();
        let (certificate, key) = (directory.path().join("c"), directory.path().join("k"));
        let identity = rcgen::generate_simple_self_signed(["localhost".to_owned()]).unwrap();
        fs::write(&certificate, identity.cert.pem()).unwrap();
        fs::write(&key, identity.signing_key.serialize_pem()).unwrap();
        let config = TlsConfig::files(certificate, key);
        let material = Material::load(&config).unwrap();
        let status = Status::default();
        let certificates = Arc::new(Certificates::new(material, config, status.clone()));
        let interval = Duration::from_millis(10);
        let _tasks = spawn(certificates, ReloadHandle::default(), interval);
        // Let the reload that starting performs finish, so it can't be what clears the error.
        tokio::time::sleep(Duration::from_millis(100)).await;

        status.set_reload_error(Some("a transient failure".to_owned()));

        wait_until_cleared(&status).await;
    }
}
