//! Records the commit a binary was built from, for the startup log.

use std::error::Error;

use vergen_gitcl::{Emitter, Gitcl};

/// Emits `VERGEN_GIT_SHA` from git, from a published package's `.cargo_vcs_info.json`, or from the
/// environment when neither is available, as in a container build. It's left unset otherwise.
fn main() -> Result<(), Box<dyn Error>> {
    let git = Gitcl::builder().sha(true).vcs_info_fallback(true).build();
    Emitter::default().add_instructions(&git)?.emit()?;
    Ok(())
}
