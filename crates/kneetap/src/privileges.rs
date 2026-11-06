/// The identity to run as when started as root, chosen so that a container needs no user database.
#[cfg(unix)]
const DEFAULT_ID: u32 = 65532;

/// Gives up root once ports are bound and certificates are read, before any request is handled.
///
/// This runs before the runtime starts, because changing identity only affects the calling thread
/// on some platforms. Afterward it verifies the change, since serving as root by mistake is worse
/// than not serving.
#[cfg(unix)]
pub fn drop_privileges(uid: Option<u32>, gid: Option<u32>) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use privdrop::reexports::nix::unistd::{Uid, getegid, geteuid, getgid, getuid, setuid};

    if !geteuid().is_root() {
        if uid.is_some() || gid.is_some() {
            tracing::warn!("privileges.ids.ignored");
        }
        return Ok(());
    }
    let (uid, gid) = (uid.unwrap_or(DEFAULT_ID), gid.unwrap_or(DEFAULT_ID));
    if uid == 0 {
        tracing::warn!("privileges.root.retained");
    }
    // Numeric IDs work in scratch images, which have no passwd database.
    privdrop::PrivDrop::default()
        .user(uid.to_string())
        .group(gid.to_string())
        .fallback_to_ids_if_names_are_numeric()
        .apply()
        .context("couldn't drop privileges")?;
    ensure!(
        getuid().as_raw() == uid
            && geteuid().as_raw() == uid
            && getgid().as_raw() == gid
            && getegid().as_raw() == gid,
        "the process identity didn't change as requested"
    );
    #[cfg(not(target_vendor = "apple"))]
    ensure!(
        privdrop::reexports::nix::unistd::getgroups()?
            .iter()
            .all(|group| group.as_raw() == gid),
        "supplementary groups weren't replaced"
    );
    if uid != 0 {
        ensure!(
            setuid(Uid::from_raw(0)).is_err(),
            "root could be restored after dropping privileges"
        );
    }
    tracing::info!(uid, gid, "privileges.dropped");
    Ok(())
}

/// Refuses an identity that can't be applied here, rather than running with more privilege than
/// the operator asked for.
#[cfg(not(unix))]
pub fn drop_privileges(uid: Option<u32>, gid: Option<u32>) -> anyhow::Result<()> {
    anyhow::ensure!(
        uid.is_none() && gid.is_none(),
        "PUID and PGID aren't supported on this platform"
    );
    Ok(())
}
