use std::fs;
use std::io::Write as _;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use time::OffsetDateTime;

pub(crate) fn create_temp_file(dir: &Utf8Path, prefix: &str, bytes: &[u8]) -> Result<Utf8PathBuf> {
    let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();

    for attempt in 0..100 {
        let temp_path = dir.join(format!(
            "{prefix}.{}.{}.{}",
            std::process::id(),
            timestamp,
            attempt
        ));

        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", temp_path.as_str()));
            }
        };

        if let Err(error) = file.write_all(bytes) {
            let _ = fs::remove_file(&temp_path);

            return Err(error).with_context(|| format!("failed to write {}", temp_path.as_str()));
        }

        if let Err(error) = file.sync_all() {
            let _ = fs::remove_file(&temp_path);

            return Err(error).with_context(|| format!("failed to sync {}", temp_path.as_str()));
        }

        return Ok(temp_path);
    }

    anyhow::bail!(
        "failed to create unique temporary {prefix} file in {}",
        dir.as_str()
    )
}

pub(crate) fn sync_dir(path: &Utf8Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("failed to open directory {}", path.as_str()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", path.as_str()))
}

pub(crate) fn sync_file(path: &Utf8Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("failed to open file {}", path.as_str()))?
        .sync_all()
        .with_context(|| format!("failed to sync file {}", path.as_str()))
}
