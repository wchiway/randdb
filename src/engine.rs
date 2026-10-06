use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use rmcp::schemars;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{
    api::Api,
    config::{Config, IndexReport, repository_root},
    scan::{self, ScanEvent},
    store::{Candidate, Store},
};

#[derive(Clone, Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// Natural-language description of the code or behavior to locate.
    pub information_request: String,
    /// Optional known identifiers to strengthen lexical matching (not hard filters).
    #[serde(default)]
    pub technical_terms: Vec<String>,
    /// Maximum output characters, including formatting; defaults to 24000 (1000–80000).
    pub max_total_chars: Option<usize>,
}

impl SearchRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            Path::new(&self.repo_path).is_absolute(),
            "repo_path must be absolute"
        );
        ensure!(
            !self.information_request.trim().is_empty() && self.information_request.len() <= 8000,
            "information_request must contain 1–8000 UTF-8 bytes"
        );
        ensure!(
            self.technical_terms.len() <= 20
                && self
                    .technical_terms
                    .iter()
                    .all(|t| !t.trim().is_empty() && t.len() <= 200),
            "technical_terms accepts at most 20 nonempty identifiers of up to 200 bytes"
        );
        ensure!(
            (1000..=80000).contains(&self.max_total_chars.unwrap_or(24000)),
            "max_total_chars must be between 1000 and 80000"
        );
        Ok(())
    }
}

pub struct Engine {
    root: PathBuf,
    config: Config,
    store: Store,
    api: Api,
    cancel: CancellationToken,
    _lock: ProjectLock,
}

impl Engine {
    pub async fn open(mut config: Config, root: &Path, cancel: CancellationToken) -> Result<Self> {
        let root = repository_root(root)?;
        std::fs::create_dir_all(&config.home)?;
        config.home = config.home.canonicalize()?;
        let dir = config.project_dir(&root);
        let lock = ProjectLock::acquire(&dir, &cancel).await?;
        let store = Store::open(&dir, config.dimensions).await?;
        let api = Api::new(config.clone(), cancel.clone())?;
        Ok(Self {
            root,
            config,
            store,
            api,
            cancel,
            _lock: lock,
        })
    }

    pub async fn index(&mut self, force: bool) -> Result<IndexReport> {
        let started = Instant::now();
        let mut known = self.store.known_files()?;
        let mut report = IndexReport::default();
        for path in known
            .iter()
            .filter(|(_, v)| v.state == "deleted")
            .map(|(p, _)| p.clone())
            .collect::<Vec<_>>()
        {
            self.store.remove(&path).await?;
            known.remove(&path);
        }
        let child = self.cancel.child_token();
        let (mut events, worker) = scan::start(
            self.root.clone(),
            self.config.home.clone(),
            known.clone(),
            force,
            child.clone(),
        );
        let mut seen = HashSet::new();
        let result: Result<()> = async {
            loop {
                let event = tokio::select! {
                    _ = self.cancel.cancelled() => bail!("indexing cancelled"),
                    event = events.recv() => event,
                };
                let Some(event) = event else {
                    break;
                };
                match event? {
                    ScanEvent::Unchanged {
                        path,
                        modified,
                        size,
                    } => {
                        self.store.touch(&path, &modified, size)?;
                        seen.insert(path);
                        report.unchanged += 1;
                    }
                    ScanEvent::Unreadable { path, reason } => {
                        // Transient: keep any committed entry instead of forcing a
                        // re-embedding later. The next scan retries this file.
                        tracing::warn!(file=%path, %reason, "file could not be read");
                        seen.insert(path);
                        report.skipped += 1;
                    }
                    ScanEvent::Skipped { path, reason } => {
                        // A previously indexed file that became binary/oversized must disappear.
                        if known.contains_key(&path) {
                            self.store.remove(&path).await?;
                            report.removed += 1;
                        }
                        tracing::warn!(file=%path, %reason, "file skipped");
                        seen.insert(path);
                        report.skipped += 1;
                    }
                    ScanEvent::Document(document) => {
                        let inputs = document
                            .chunks
                            .iter()
                            .map(|c| {
                                format!(
                                    "{}\n{}\n{}",
                                    document.path,
                                    c.breadcrumb,
                                    &document.content[c.start..c.end]
                                )
                            })
                            .collect::<Vec<_>>();
                        // Network failures leave the previous committed generation untouched.
                        let embeddings = self.api.embed(&inputs).await?;
                        ensure!(!self.cancel.is_cancelled(), "indexing cancelled");
                        let ids = self.store.stage(&document)?;
                        self.store.materialize(&document, &ids, &embeddings).await?;
                        report.indexed += 1;
                        report.chunks += document.chunks.len();
                        seen.insert(document.path);
                    }
                }
            }
            Ok(())
        }
        .await;
        child.cancel();
        drop(events); // Unblock producers before joining, including on API errors/cancellation.
        worker.await.context("index worker failed")?;
        result?;
        ensure!(!self.cancel.is_cancelled(), "indexing cancelled");
        // Prune only after a complete successful walk. Traversal errors never erase a subtree.
        for path in known.keys().filter(|path| !seen.contains(*path)) {
            self.store.remove(path).await?;
            report.removed += 1;
        }
        tracing::info!(
            indexed = report.indexed,
            unchanged = report.unchanged,
            removed = report.removed,
            skipped = report.skipped,
            elapsed_ms = started.elapsed().as_millis(),
            "index complete"
        );
        Ok(report)
    }

    pub async fn search(&mut self, request: &SearchRequest) -> Result<String> {
        request.validate()?;
        self.index(false).await?;
        let query = request.information_request.trim();
        let vectors = self.api.embed(&[query.to_owned()]).await?;
        let vector_ids = self.store.vector_ids(&vectors[0]).await?;
        let lexical_ids = self
            .store
            .lexical_ids(&format!("{query} {}", request.technical_terms.join(" ")))?;
        let mut scores = HashMap::<String, f64>::new();
        for (ids, weight) in [(vector_ids, 0.6), (lexical_ids, 0.4)] {
            for (rank, id) in ids.into_iter().enumerate() {
                *scores.entry(id).or_default() += weight / (20.0 + rank as f64 + 1.0);
            }
        }
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut candidates = Vec::new();
        for (id, score) in ranked.into_iter().take(40) {
            if let Some(candidate) = self.store.candidate(&id, score)? {
                candidates.push(candidate);
            }
        }
        if candidates.is_empty() {
            return Ok("No matching indexed code found.".into());
        }
        let inputs: Vec<_> = candidates
            .iter()
            .map(|c| format!("{}\n{}\n{}", c.path, c.breadcrumb, c.text))
            .collect();
        let mut warning = None;
        let seeds = match self.api.rerank(query, &inputs).await {
            Ok(ranks) => {
                let low_confidence = ranks.first().is_some_and(|(_, score)| *score < 0.2);
                if low_confidence {
                    warning = Some(
                        "Low relevance confidence; verify the returned code before relying on it.",
                    );
                }
                ranks
                    .into_iter()
                    .take(if low_confidence { 1 } else { 8 })
                    .map(|(i, score)| {
                        let mut candidate = candidates[i].clone();
                        candidate.score = score;
                        candidate
                    })
                    .collect::<Vec<_>>()
            }
            Err(error) => {
                ensure!(!self.cancel.is_cancelled(), "search cancelled");
                tracing::warn!(%error,"reranker unavailable");
                warning = Some("Reranker unavailable; results use hybrid retrieval order.");
                candidates.into_iter().take(8).collect()
            }
        };
        pack(
            &self.store,
            &seeds,
            request.max_total_chars.unwrap_or(24000),
            warning,
        )
    }
}

struct ProjectLock(File);

impl ProjectLock {
    async fn acquire(dir: &Path, cancel: &CancellationToken) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("index.lock"))?;
        let start = Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self(file)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(
                        start.elapsed() < Duration::from_secs(120),
                        "another RandDB process is still using this index"
                    );
                    tokio::select! {
                        _ = cancel.cancelled() => bail!("lock wait cancelled"),
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }
                }
                Err(error) => return Err(error).context("lock index"),
            }
        }
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn pack(
    store: &Store,
    seeds: &[Candidate],
    budget: usize,
    warning: Option<&str>,
) -> Result<String> {
    let mut output = warning.map(|w| format!("{w}\n\n")).unwrap_or_default();
    let mut ranges = HashMap::<String, Vec<(usize, usize)>>::new();
    let mut snippets = 0;
    for seed in seeds {
        let content = store.content(&seed.path)?;
        let (mut start, mut end) = (seed.start, seed.end);
        // At most three neighboring lines / 400 bytes on either side, within the same file.
        for _ in 0..3 {
            let previous = content[..start]
                .trim_end_matches('\n')
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            if seed.start - previous > 400 {
                break;
            }
            start = previous;
        }
        for _ in 0..3 {
            let next = content[end..]
                .find('\n')
                .map(|i| end + i + 1)
                .unwrap_or(content.len());
            if next - seed.end > 400 {
                break;
            }
            end = next;
        }
        let existing = ranges.entry(seed.path.clone()).or_default();
        if existing.len() >= 3
            || existing
                .iter()
                .any(|(a, b)| *a <= seed.start && seed.end <= *b)
        {
            continue;
        }
        // Remove already returned overlap without discarding a distinct match.
        for &(a, b) in existing.iter() {
            if a <= start && start < b {
                start = b;
            }
            if a < end && end <= b {
                end = a;
            }
        }
        if start >= end {
            continue;
        }
        let remaining = budget.saturating_sub(output.chars().count());
        if remaining < 250 {
            break;
        }
        let capacity = remaining.saturating_sub(seed.path.chars().count() + 120);
        let text = &content[start..end];
        let (snippet, truncated) = truncate_chars(text, capacity);
        end = start + snippet.len();
        let start_line = content[..start].bytes().filter(|b| *b == b'\n').count() + 1;
        let end_line = start_line
            + snippet
                .trim_end_matches('\n')
                .bytes()
                .filter(|b| *b == b'\n')
                .count();
        let fence = "`".repeat(longest_backticks(snippet).saturating_add(1).max(3));
        let block = format!(
            "## {} (L{start_line}-L{end_line})\n{fence}{}\n{snippet}\n{fence}\n{}\n",
            seed.path.replace(['\n', '\r'], "?"),
            seed.language,
            if truncated {
                "[Output budget reached]"
            } else {
                ""
            }
        );
        if output.chars().count() + block.chars().count() > budget {
            break;
        }
        output.push_str(&block);
        existing.push((start, end));
        snippets += 1;
        if truncated {
            break;
        }
    }
    if snippets == 0 {
        output.push_str("No code fits the output budget.");
    }
    Ok(output)
}

fn truncate_chars(text: &str, count: usize) -> (&str, bool) {
    match text.char_indices().nth(count) {
        Some((i, _)) => (&text[..i], true),
        None => (text, false),
    }
}

fn longest_backticks(text: &str) -> usize {
    let (mut longest, mut run) = (0, 0);
    for byte in text.bytes() {
        if byte == b'`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lock_wait_is_cancellable_and_lock_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let first = ProjectLock::acquire(dir.path(), &cancel).await.unwrap();
        cancel.cancel();
        assert!(ProjectLock::acquire(dir.path(), &cancel).await.is_err());
        drop(first);
        assert!(
            ProjectLock::acquire(dir.path(), &CancellationToken::new())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn packed_unicode_is_bounded_and_duplicate_seeds_are_not_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path(), 2).await.unwrap();
        let content = format!(
            "// 中文🦀\nfn authenticate() {{}}\n{}",
            "// context\n".repeat(600)
        );
        let document = crate::scan::Document {
            path: "auth.rs".into(),
            hash: crate::config::digest(content.as_bytes()),
            modified: "1".into(),
            size: content.len() as u64,
            language: "rust",
            chunks: crate::chunk::Chunker::default().split(&content, "rust"),
            content,
        };
        let ids = store.stage(&document).unwrap();
        store
            .materialize(&document, &ids, &vec![vec![1.0, 0.0]; ids.len()])
            .await
            .unwrap();
        let seed = store.candidate(&ids[0], 0.9).unwrap().unwrap();
        let output = pack(&store, &[seed.clone(), seed], 1000, None).unwrap();
        assert!(output.chars().count() <= 1000);
        assert!(output.contains("中文🦀"));
        assert_eq!(output.matches("fn authenticate").count(), 1);
        assert!(output.contains("[Output budget reached]"));
    }
}
