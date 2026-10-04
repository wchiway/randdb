use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const INDEX_VERSION: &str = "randdb-utf8-v1";
pub const DEFAULT_ENV: &str = "# RandDB configuration. Environment variables override this file.\n\
EMBEDDINGS_API_KEY=\n\
EMBEDDINGS_BASE_URL=https://api.siliconflow.cn/v1/embeddings\n\
EMBEDDINGS_MODEL=BAAI/bge-m3\n\
EMBEDDINGS_DIMENSIONS=1024\n\n\
RERANK_API_KEY=\n\
RERANK_BASE_URL=https://api.siliconflow.cn/v1/rerank\n\
RERANK_MODEL=BAAI/bge-reranker-v2-m3\n";

#[derive(Clone)]
pub struct Endpoint {
    pub url: reqwest::Url,
    pub model: String,
    pub key: String,
}

#[derive(Clone)]
pub struct Config {
    pub home: PathBuf,
    pub embedding: Endpoint,
    pub dimensions: usize,
    pub reranker: Endpoint,
}

pub fn default_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".randdb")
}

pub fn initialize(home: &Path) -> Result<bool> {
    std::fs::create_dir_all(home).context("create RandDB data directory")?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(home.join(".env")) {
        Ok(mut file) => {
            file.write_all(DEFAULT_ENV.as_bytes())?;
            file.sync_all()?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err).context("create configuration file"),
    }
}

impl Config {
    pub fn load(home: PathBuf) -> Result<Self> {
        let path = home.join(".env");
        let values = if path.exists() {
            dotenvy::from_path_iter(&path)
                .context("read RandDB .env")?
                .collect::<std::result::Result<HashMap<_, _>, _>>()
                .context("invalid RandDB .env syntax")?
        } else {
            HashMap::new()
        };
        // Read without modifying process-global environment (MCP requests may overlap).
        Self::from_values(home, |key| {
            std::env::var(key).ok().or_else(|| values.get(key).cloned())
        })
    }

    fn from_values(home: PathBuf, get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let required = |key: &str| -> Result<String> {
            let value = get(key).filter(|v| !v.trim().is_empty()).with_context(|| {
                format!(
                    "missing {key}; run `randdb init` and configure {}",
                    home.join(".env").display()
                )
            })?;
            ensure!(
                !value.starts_with("your-api-key"),
                "replace the placeholder for {key}"
            );
            Ok(value.trim().to_owned())
        };
        let endpoint = |prefix: &str| -> Result<Endpoint> {
            let url = reqwest::Url::parse(&required(&format!("{prefix}_BASE_URL"))?)
                .with_context(|| format!("invalid {prefix}_BASE_URL"))?;
            ensure!(
                matches!(url.scheme(), "http" | "https"),
                "{prefix}_BASE_URL must use HTTP or HTTPS"
            );
            ensure!(
                url.username().is_empty() && url.password().is_none(),
                "use {prefix}_API_KEY instead of URL credentials"
            );
            Ok(Endpoint {
                url,
                model: required(&format!("{prefix}_MODEL"))?,
                key: required(&format!("{prefix}_API_KEY"))?,
            })
        };
        let dimensions = required("EMBEDDINGS_DIMENSIONS")?
            .parse::<usize>()
            .context("EMBEDDINGS_DIMENSIONS must be an integer")?;
        ensure!(
            (1..=16384).contains(&dimensions),
            "EMBEDDINGS_DIMENSIONS must be between 1 and 16384"
        );
        Ok(Self {
            embedding: endpoint("EMBEDDINGS")?,
            reranker: endpoint("RERANK")?,
            dimensions,
            home,
        })
    }

    pub fn fingerprint(&self) -> String {
        digest(
            format!(
                "{INDEX_VERSION}\0{}\0{}\0{}",
                self.embedding.url, self.embedding.model, self.dimensions
            )
            .as_bytes(),
        )
    }

    pub fn project_dir(&self, root: &Path) -> PathBuf {
        self.home
            .join("indexes")
            .join(digest(root.as_os_str().as_encoded_bytes()))
            .join(self.fingerprint())
    }
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn repository_root(path: &Path) -> Result<PathBuf> {
    let root = path
        .canonicalize()
        .context("repository path does not exist or is inaccessible")?;
    if !root.is_dir() {
        bail!("repository path must be a directory");
    }
    Ok(root)
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct IndexReport {
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub skipped: usize,
    pub chunks: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_preserves_existing_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(initialize(tmp.path()).unwrap());
        std::fs::write(tmp.path().join(".env"), "secret").unwrap();
        assert!(!initialize(tmp.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".env")).unwrap(),
            "secret"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(tmp.path().join(".env"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
