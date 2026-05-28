use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::validation::validate_non_empty;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DataConfig {
    pub artifact_url: String,
    pub index_dir: Utf8PathBuf,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            artifact_url: "file://./data/artifacts".to_owned(),
            index_dir: Utf8PathBuf::from("./data/indexes"),
        }
    }
}

impl DataConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_non_empty("data.artifact_url", &self.artifact_url)?;

        if self.index_dir.as_str().is_empty() {
            return Err(ConfigError::Validation(
                "data.index_dir must not be empty".to_owned(),
            ));
        }

        Ok(())
    }
}
