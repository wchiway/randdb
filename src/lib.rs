//! RandDB's local indexing and retrieval engine.

pub mod api;
pub mod chunk;
pub mod config;
pub mod engine;
pub mod mcp;
pub mod scan;
pub mod store;

use anyhow::Result;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

pub async fn index(
    home: PathBuf,
    root: PathBuf,
    force: bool,
    cancel: CancellationToken,
) -> Result<config::IndexReport> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(async {
            let config = config::Config::load(home)?;
            let mut engine = engine::Engine::open(config, &root, cancel).await?;
            engine.index(force).await
        })
    })
    .await?
}

pub async fn search(
    home: PathBuf,
    request: engine::SearchRequest,
    cancel: CancellationToken,
) -> Result<String> {
    request.validate()?;
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(async {
            let config = config::Config::load(home)?;
            let mut engine =
                engine::Engine::open(config, std::path::Path::new(&request.repo_path), cancel)
                    .await?;
            engine.search(&request).await
        })
    })
    .await?
}
