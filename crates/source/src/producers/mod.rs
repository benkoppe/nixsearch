use anyhow::Result;
use async_trait::async_trait;

use nixsearch_store::ArtifactStore;

use crate::artifact::{ProduceRequest, ProducedArtifact};

mod channel;
mod download;
mod eval_modules;
mod existing_file;
mod flake_file;
mod nix;
mod nix_build_options;

pub use channel::{ChannelOptionsJsonProducer, ChannelPackagesJsonProducer};
pub use download::{DownloadCompression, DownloadProducer};
pub use eval_modules::{EvalModule, EvalModuleRef, EvalModulesProducer};
pub use existing_file::ExistingFileProducer;
pub use flake_file::FlakeFileProducer;
pub use nix_build_options::NixBuildOptionsJsonProducer;

#[async_trait]
pub trait Producer: Send + Sync {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact>;
}
