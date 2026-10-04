use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Endpoint};

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const EMBEDDING_BATCH: usize = 16;

#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
    config: Config,
    cancel: CancellationToken,
}

impl Api {
    pub fn new(config: Config, cancel: CancellationToken) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("RandDB/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            config,
            cancel,
        })
    }

    async fn request(&self, endpoint: &Endpoint, body: Value, label: &str) -> Result<Value> {
        for attempt in 0..3 {
            let request = self
                .client
                .post(endpoint.url.clone())
                .bearer_auth(&endpoint.key)
                .json(&body)
                .send();
            let result = tokio::select! {
                _ = self.cancel.cancelled() => bail!("request cancelled"),
                result = request => result,
            };
            let mut response = match result {
                Ok(response) => response,
                Err(_) if attempt < 2 => {
                    self.delay(attempt).await?;
                    continue;
                }
                Err(_) => bail!(
                    "{label} request failed (connection or timeout); check endpoint and connectivity"
                ),
            };
            let status = response.status();
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < 2 {
                drop(response);
                self.delay(attempt).await?;
                continue;
            }
            // Do not echo provider bodies or full URLs: either may contain credentials/code.
            ensure!(status.is_success(), "{label} returned HTTP {status}");
            let mut bytes = Vec::new();
            loop {
                let part = tokio::select! {
                    _ = self.cancel.cancelled() => bail!("request cancelled"),
                    part = response.chunk() => part.context("read API response")?,
                };
                let Some(part) = part else {
                    break;
                };
                ensure!(
                    bytes.len() + part.len() <= MAX_RESPONSE_BYTES,
                    "{label} response exceeds size limit"
                );
                bytes.extend_from_slice(&part);
            }
            return serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid {label} JSON response"));
        }
        unreachable!("request loop returns after final attempt")
    }

    async fn delay(&self, attempt: u32) -> Result<()> {
        tokio::select! {
            _ = self.cancel.cancelled() => bail!("request cancelled"),
            _ = tokio::time::sleep(Duration::from_secs(1 << attempt)) => Ok(()),
        }
    }

    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(EMBEDDING_BATCH) {
            let response = self
                .request(
                    &self.config.embedding,
                    json!({
                        "model": self.config.embedding.model,
                        "input": batch,
                        "encoding_format": "float"
                    }),
                    "embedding",
                )
                .await?;
            vectors.extend(parse_embeddings(
                response,
                batch.len(),
                self.config.dimensions,
            )?);
        }
        Ok(vectors)
    }

    pub async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let response = self
            .request(
                &self.config.reranker,
                json!({
                    "model": self.config.reranker.model, "query": query, "documents": documents,
                    "top_n": documents.len().min(8), "return_documents": false
                }),
                "reranker",
            )
            .await?;
        #[derive(Deserialize)]
        struct Entry {
            index: usize,
            relevance_score: f64,
        }
        #[derive(Deserialize)]
        struct Response {
            results: Vec<Entry>,
        }
        let parsed: Response =
            serde_json::from_value(response).context("invalid reranker results")?;
        let mut seen = std::collections::HashSet::new();
        let mut ranks = Vec::new();
        for entry in parsed.results {
            ensure!(
                entry.index < documents.len()
                    && entry.relevance_score.is_finite()
                    && seen.insert(entry.index),
                "invalid or duplicate reranker index/score"
            );
            ranks.push((entry.index, entry.relevance_score));
        }
        ensure!(!ranks.is_empty(), "reranker returned no results");
        ranks.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(ranks)
    }
}

fn parse_embeddings(response: Value, count: usize, dimensions: usize) -> Result<Vec<Vec<f32>>> {
    #[derive(Deserialize)]
    struct Entry {
        index: usize,
        embedding: Vec<f32>,
    }
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Entry>,
    }
    let response: Response =
        serde_json::from_value(response).context("invalid embedding results")?;
    ensure!(
        response.data.len() == count,
        "embedding response count does not match input"
    );
    let mut ordered: Vec<Option<Vec<f32>>> = vec![None; count];
    for entry in response.data {
        ensure!(
            entry.index < count && ordered[entry.index].is_none(),
            "invalid or duplicate embedding index"
        );
        ensure!(
            entry.embedding.len() == dimensions,
            "embedding dimensions do not match EMBEDDINGS_DIMENSIONS"
        );
        ensure!(
            entry.embedding.iter().all(|v| v.is_finite())
                && entry.embedding.iter().any(|v| *v != 0.0),
            "embedding must be finite and nonzero"
        );
        ordered[entry.index] = Some(entry.embedding);
    }
    ordered
        .into_iter()
        .map(|v| v.context("missing embedding index"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeddings_are_reordered_and_validated_before_storage() {
        let response =
            json!({"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]});
        assert_eq!(
            parse_embeddings(response, 2, 2).unwrap(),
            vec![vec![1.0, 0.0], vec![0.0, 1.0]]
        );
        assert!(parse_embeddings(json!({"data":[{"index":0,"embedding":[1.0]}]}), 1, 2).is_err());
        assert!(
            parse_embeddings(json!({"data":[{"index":0,"embedding":[0.0,0.0]}]}), 1, 2).is_err()
        );
        assert!(
            parse_embeddings(
                json!({"data":[{"index":0,"embedding":[1.0]},{"index":0,"embedding":[1.0]}]}),
                2,
                1
            )
            .is_err()
        );
    }
}
