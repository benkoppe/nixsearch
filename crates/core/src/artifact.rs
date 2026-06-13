use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    OptionsJson,
    PackagesJson,
    FlakeInfoJson,
}

impl ArtifactKind {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::OptionsJson => "options.json",
            Self::PackagesJson => "packages.json",
            Self::FlakeInfoJson => "flake-info.json",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OptionsJson => "options-json",
            Self::PackagesJson => "packages-json",
            Self::FlakeInfoJson => "flake-info-json",
        }
    }
}
