//! `tenaz watch`: the run, session by session, as it happens.

use std::path::PathBuf;

use super::initialized;
use crate::watch;

/// `tenaz watch`: follows the run in `project`.
pub(crate) fn execute(project: PathBuf, options: &watch::WatchOptions) -> anyhow::Result<()> {
    let project = initialized(project)?;
    watch::watch(&project, options)?;
    Ok(())
}
