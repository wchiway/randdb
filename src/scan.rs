use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail, ensure};
use ignore::{WalkBuilder, WalkState};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    chunk::{Chunk, Chunker, MAX_FILE_BYTES, decode, language},
    config::digest,
};

pub const WORKERS: usize = 4;
pub const QUEUE_LENGTH: usize = 4;

#[derive(Clone, Debug)]
pub struct KnownFile {
    pub hash: String,
    pub modified: String,
    pub size: u64,
    pub state: String,
}

#[derive(Debug)]
pub struct Document {
    pub path: String,
    pub hash: String,
    pub modified: String,
    pub size: u64,
    pub language: &'static str,
    pub content: String,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug)]
pub enum ScanEvent {
    Document(Document),
    Unchanged {
        path: String,
        modified: String,
        size: u64,
    },
    Skipped {
        path: String,
        reason: String,
    },
}

pub fn start(
    root: PathBuf,
    data_home: PathBuf,
    known: HashMap<String, KnownFile>,
    force: bool,
    cancel: CancellationToken,
) -> (
    mpsc::Receiver<Result<ScanEvent>>,
    tokio::task::JoinHandle<()>,
) {
    let (send, receive) = mpsc::channel(QUEUE_LENGTH);
    let task = tokio::task::spawn_blocking(move || {
        let known = Arc::new(known);
        let mut walk = WalkBuilder::new(&root);
        // Include useful dotfiles, honor nested .gitignore even outside Git repositories.
        walk.hidden(false)
            .follow_links(false)
            .require_git(false)
            .threads(WORKERS);
        walk.filter_entry(move |entry| {
            if entry.path().starts_with(&data_home) {
                return false;
            }
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                !matches!(
                    name.as_ref(),
                    ".git"
                        | ".hg"
                        | ".svn"
                        | ".randdb"
                        | ".contextweaver"
                        | ".worktrees"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                        | ".next"
                )
            } else {
                // Secrets and dependency lockfiles add little code context.
                !(name == ".env"
                    || name.starts_with(".env.")
                    || matches!(
                        name.as_ref(),
                        "Cargo.lock"
                            | "package-lock.json"
                            | "pnpm-lock.yaml"
                            | "yarn.lock"
                            | "bun.lock"
                    ))
            }
        });
        walk.build_parallel().run(|| {
            let send = send.clone();
            let known = known.clone();
            let root = &root;
            let cancel = &cancel;
            let mut chunker = Chunker::default();
            Box::new(move |entry| {
                if cancel.is_cancelled() || send.is_closed() {
                    return WalkState::Quit;
                }
                let event = match entry {
                    Err(error) => Err(anyhow::anyhow!("incomplete directory scan: {error}")),
                    Ok(entry) => {
                        if !entry.file_type().is_some_and(|t| t.is_file()) {
                            return WalkState::Continue;
                        }
                        let relative =
                            match entry.path().strip_prefix(root).ok().and_then(Path::to_str) {
                                Some(relative) => relative.replace('\\', "/"),
                                None => {
                                    let _ = send.blocking_send(Err(anyhow::anyhow!(
                                        "file path cannot be represented as UTF-8"
                                    )));
                                    return WalkState::Quit;
                                }
                            };
                        if language(&relative) == "unknown"
                            && !matches!(
                                entry.file_name().to_str(),
                                Some("Dockerfile" | "Makefile" | "Justfile")
                            )
                        {
                            return WalkState::Continue;
                        }
                        read_document(
                            entry.path(),
                            &relative,
                            known.get(&relative),
                            force,
                            &mut chunker,
                        )
                    }
                };
                if send.blocking_send(event).is_err() {
                    WalkState::Quit
                } else {
                    WalkState::Continue
                }
            })
        });
    });
    (receive, task)
}

fn modified(meta: &std::fs::Metadata) -> Result<String> {
    Ok(meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .context("file modification time predates Unix epoch")?
        .as_nanos()
        .to_string())
}

fn read_document(
    path: &Path,
    relative: &str,
    known: Option<&KnownFile>,
    force: bool,
    chunker: &mut Chunker,
) -> Result<ScanEvent> {
    let before =
        std::fs::symlink_metadata(path).with_context(|| format!("read metadata for {relative}"))?;
    ensure!(
        before.file_type().is_file(),
        "file changed type during scan: {relative}"
    );
    let stamp = modified(&before)?;
    let unchanged = || ScanEvent::Unchanged {
        path: relative.to_owned(),
        modified: stamp.clone(),
        size: before.len(),
    };
    if !force
        && known
            .is_some_and(|k| k.state == "ready" && k.modified == stamp && k.size == before.len())
    {
        return Ok(unchanged());
    }
    let skip = |reason: String| ScanEvent::Skipped {
        path: relative.to_owned(),
        reason,
    };
    if before.len() > MAX_FILE_BYTES as u64 {
        return Ok(skip("exceeds 1 MiB file limit".into()));
    }
    let file = File::open(path).with_context(|| format!("read {relative}"))?;
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = std::fs::symlink_metadata(path)?;
    if !after.file_type().is_file() || modified(&after)? != stamp || after.len() != before.len() {
        bail!("file changed while being read: {relative}; retry indexing");
    }
    if bytes.len() > MAX_FILE_BYTES {
        return Ok(skip("exceeds 1 MiB file limit".into()));
    }
    let hash = digest(&bytes);
    if !force && known.is_some_and(|k| k.state == "ready" && k.hash == hash) {
        return Ok(unchanged());
    }
    let content = match decode(&bytes) {
        Ok(content) => content,
        Err(error) => return Ok(skip(error.to_string())),
    };
    let language = language(relative);
    let chunks = chunker.split(&content, language);
    Ok(ScanEvent::Document(Document {
        path: relative.to_owned(),
        hash,
        modified: stamp,
        size: before.len(),
        language,
        content,
        chunks,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nested_ignores_secrets_and_symlinks_are_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(dir.path().join("ignored")).unwrap();
        std::fs::write(dir.path().join("ignored/a.rs"), "fn ignored() {}").unwrap();
        std::fs::write(dir.path().join("src/.gitignore"), "hidden.rs\n").unwrap();
        std::fs::write(dir.path().join("src/hidden.rs"), "fn hidden() {}").unwrap();
        std::fs::write(dir.path().join("src/ok.rs"), "fn useful() {}").unwrap();
        std::fs::write(dir.path().join(".env.local"), "SECRET=value").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("src/ok.rs"), dir.path().join("link.rs"))
            .unwrap();
        let (mut events, task) = start(
            dir.path().into(),
            dir.path().join("data"),
            HashMap::new(),
            false,
            CancellationToken::new(),
        );
        let mut paths = Vec::new();
        while let Some(event) = events.recv().await {
            if let ScanEvent::Document(doc) = event.unwrap() {
                paths.push(doc.path);
            }
        }
        task.await.unwrap();
        assert_eq!(paths, ["src/ok.rs"]);
    }
}
