use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use super::IndexStore;

impl IndexStore {
    pub(super) fn create_temp_file(
        &self,
        dir: &Utf8Path,
        prefix: &str,
        bytes: &[u8],
    ) -> Result<Utf8PathBuf> {
        crate::atomic_file::create_temp_file(dir, prefix, bytes)
    }

    pub(super) fn sync_dir(path: &Utf8Path) -> Result<()> {
        crate::atomic_file::sync_dir(path)
    }
}
