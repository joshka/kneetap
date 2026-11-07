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
    let (uid, gid) = target_ids(uid, gid);
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

/// Fills in unset IDs. Staying root keeps root's group, since a request for root isn't a request to
/// lose its group.
#[cfg(unix)]
fn target_ids(uid: Option<u32>, gid: Option<u32>) -> (u32, u32) {
    let uid = uid.unwrap_or(DEFAULT_ID);
    let default_gid = if uid == 0 { 0 } else { DEFAULT_ID };
    (uid, gid.unwrap_or(default_gid))
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

#[cfg(all(test, unix))]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(None, None, (DEFAULT_ID, DEFAULT_ID))]
    #[case(Some(1000), None, (1000, DEFAULT_ID))]
    #[case(None, Some(1001), (DEFAULT_ID, 1001))]
    #[case(Some(0), None, (0, 0))]
    #[case(Some(0), Some(1001), (0, 1001))]
    fn unset_ids_get_defaults(
        #[case] uid: Option<u32>,
        #[case] gid: Option<u32>,
        #[case] ids: (u32, u32),
    ) {
        assert_eq!(target_ids(uid, gid), ids);
    }
}
