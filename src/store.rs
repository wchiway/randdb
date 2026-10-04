use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use arrow_array::{Array, FixedSizeListArray, RecordBatch, StringArray, types::Float32Type};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::{
    DistanceType, Table,
    query::{ExecutableQuery, QueryBase, Select},
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    config::{INDEX_VERSION, digest},
    scan::{Document, KnownFile},
};

pub struct Store {
    db: Connection,
    vectors: Table,
    dimensions: usize,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub path: String,
    pub language: String,
    pub start: usize,
    pub end: usize,
    pub breadcrumb: String,
    pub text: String,
    pub score: f64,
}

impl Store {
    pub async fn open(dir: &Path, dimensions: usize) -> Result<Self> {
        let db = Connection::open(dir.join("index.sqlite"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY, hash TEXT NOT NULL, modified TEXT NOT NULL, size INTEGER NOT NULL,
                language TEXT NOT NULL, content TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('pending','ready','deleted')));
            CREATE TABLE IF NOT EXISTS chunks (
                id TEXT PRIMARY KEY, path TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL,
                breadcrumb TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS chunks_path ON chunks(path);
            CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(id UNINDEXED, body, tokenize='unicode61');")?;
        let version: Option<String> = db
            .query_row("SELECT value FROM metadata WHERE key='version'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(version) = version {
            ensure!(
                version == INDEX_VERSION,
                "unsupported RandDB index version; use a new data directory"
            );
        }
        db.execute(
            "INSERT OR IGNORE INTO metadata VALUES ('version',?1)",
            [INDEX_VERSION],
        )?;
        let uri = dir.join("vectors");
        let connection = lancedb::connect(uri.to_str().context("index path must be UTF-8")?)
            .execute()
            .await?;
        let vectors = match connection.open_table("chunks").execute().await {
            Ok(table) => table,
            Err(lancedb::Error::TableNotFound { .. }) => {
                // SQLite content is authoritative. A lost vector table invalidates ready rows.
                db.execute("UPDATE files SET state='pending' WHERE state='ready'", [])?;
                connection
                    .create_empty_table("chunks", vector_schema(dimensions))
                    .execute()
                    .await?
            }
            Err(error) => return Err(error).context("open vector table"),
        };
        Ok(Self {
            db,
            vectors,
            dimensions,
        })
    }

    pub fn known_files(&self) -> Result<HashMap<String, KnownFile>> {
        let mut stmt = self
            .db
            .prepare("SELECT path,hash,modified,size,state FROM files")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                KnownFile {
                    hash: row.get(1)?,
                    modified: row.get(2)?,
                    size: u64::from(row.get::<_, u32>(3)?),
                    state: row.get(4)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn touch(&self, path: &str, modified: &str, size: u64) -> Result<()> {
        self.db.execute(
            "UPDATE files SET modified=?2,size=?3 WHERE path=?1 AND state='ready'",
            params![
                path,
                modified,
                u32::try_from(size).context("file size exceeds SQLite bounds")?
            ],
        )?;
        Ok(())
    }

    // Visible only after all vector writes succeed. Retrying pending rows is idempotent.
    pub fn stage(&mut self, document: &Document) -> Result<Vec<String>> {
        let tx = self.db.transaction()?;
        delete_chunks(&tx, &document.path)?;
        tx.execute("INSERT INTO files(path,hash,modified,size,language,content,state) VALUES (?1,?2,?3,?4,?5,?6,'pending')
            ON CONFLICT(path) DO UPDATE SET hash=excluded.hash,modified=excluded.modified,size=excluded.size,
            language=excluded.language,content=excluded.content,state='pending'",
            params![document.path,document.hash,document.modified,u32::try_from(document.size).context("file size exceeds SQLite bounds")?,document.language,document.content])?;
        let mut ids = Vec::with_capacity(document.chunks.len());
        for chunk in &document.chunks {
            let id = digest(
                format!(
                    "{}\0{}\0{}\0{}",
                    document.path, document.hash, chunk.start, chunk.end
                )
                .as_bytes(),
            );
            let text = document
                .content
                .get(chunk.start..chunk.end)
                .context("invalid UTF-8 chunk boundaries")?;
            tx.execute(
                "INSERT INTO chunks(id,path,start,end,breadcrumb) VALUES (?1,?2,?3,?4,?5)",
                params![
                    id,
                    document.path,
                    u32::try_from(chunk.start)?,
                    u32::try_from(chunk.end)?,
                    chunk.breadcrumb
                ],
            )?;
            let body = format!(
                "{} {} {} {}",
                document.path,
                chunk.breadcrumb,
                text,
                split_identifiers(text)
            );
            tx.execute(
                "INSERT INTO chunk_fts(id,body) VALUES (?1,?2)",
                params![id, body],
            )?;
            ids.push(id);
        }
        tx.commit()?;
        Ok(ids)
    }

    pub async fn materialize(
        &mut self,
        document: &Document,
        ids: &[String],
        embeddings: &[Vec<f32>],
    ) -> Result<()> {
        ensure!(ids.len() == embeddings.len(), "vector count mismatch");
        ensure!(
            embeddings
                .iter()
                .all(|v| v.len() == self.dimensions && v.iter().all(|x| x.is_finite())),
            "invalid vectors"
        );
        let predicate = path_predicate(&document.path);
        self.vectors.delete(predicate.as_str()).await?;
        if !ids.is_empty() {
            let batch = RecordBatch::try_new(
                vector_schema(self.dimensions),
                vec![
                    Arc::new(StringArray::from_iter_values(
                        ids.iter().map(String::as_str),
                    )),
                    Arc::new(StringArray::from_iter_values(std::iter::repeat_n(
                        document.path.as_str(),
                        ids.len(),
                    ))),
                    Arc::new(
                        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                            embeddings
                                .iter()
                                .map(|v| Some(v.iter().copied().map(Some).collect::<Vec<_>>())),
                            self.dimensions as i32,
                        ),
                    ),
                ],
            )?;
            self.vectors.add(batch).execute().await?;
        }
        let count = self
            .vectors
            .count_rows(Some(predicate.as_str().into()))
            .await?;
        ensure!(
            count == ids.len(),
            "vector write verification failed; file remains pending"
        );
        self.db.execute(
            "UPDATE files SET state='ready' WHERE path=?1 AND hash=?2",
            params![document.path, document.hash],
        )?;
        Ok(())
    }

    pub async fn remove(&mut self, path: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("UPDATE files SET state='deleted' WHERE path=?1", [path])?;
        delete_chunks(&tx, path)?;
        tx.commit()?;
        // Keep the tombstone until LanceDB has completed deletion (including across crashes).
        self.vectors.delete(path_predicate(path).as_str()).await?;
        self.db.execute(
            "DELETE FROM files WHERE path=?1 AND state='deleted'",
            [path],
        )?;
        Ok(())
    }

    pub async fn vector_ids(&self, query: &[f32]) -> Result<Vec<String>> {
        let batches: Vec<RecordBatch> = self
            .vectors
            .query()
            .nearest_to(query)?
            .distance_type(DistanceType::Cosine)
            .limit(80)
            .select(Select::columns(&["id"]))
            .execute()
            .await?
            .try_collect()
            .await?;
        let mut ids = Vec::new();
        for batch in batches {
            let column = batch
                .column_by_name("id")
                .context("missing vector id column")?
                .as_any()
                .downcast_ref::<StringArray>()
                .context("invalid vector id type")?;
            for row in 0..column.len() {
                if !column.is_null(row) {
                    ids.push(column.value(row).to_owned());
                }
            }
        }
        Ok(ids)
    }

    pub fn lexical_ids(&self, query: &str) -> Result<Vec<String>> {
        let words = lexical_query(query);
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.db.prepare("SELECT chunk_fts.id FROM chunk_fts
            JOIN chunks c ON c.id=chunk_fts.id JOIN files f ON f.path=c.path
            WHERE chunk_fts MATCH ?1 AND f.state='ready' ORDER BY bm25(chunk_fts),chunk_fts.id LIMIT 80")?;
        Ok(stmt
            .query_map([words], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn candidate(&self, id: &str, score: f64) -> Result<Option<Candidate>> {
        let record = self
            .db
            .query_row(
                "SELECT c.path,f.language,c.start,c.end,c.breadcrumb,f.content
            FROM chunks c JOIN files f ON c.path=f.path WHERE c.id=?1 AND f.state='ready'",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)? as usize,
                        row.get::<_, u32>(3)? as usize,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        record
            .map(|(path, language, start, end, breadcrumb, content)| {
                let text = content
                    .get(start..end)
                    .context("stored chunk is outside its source snapshot")?
                    .to_owned();
                Ok(Candidate {
                    id: id.to_owned(),
                    path,
                    language,
                    start,
                    end,
                    breadcrumb,
                    text,
                    score,
                })
            })
            .transpose()
    }

    pub fn content(&self, path: &str) -> Result<String> {
        self.db
            .query_row(
                "SELECT content FROM files WHERE path=?1 AND state='ready'",
                [path],
                |r| r.get(0),
            )
            .context("read ready source snapshot")
    }
}

fn delete_chunks(db: &Connection, path: &str) -> Result<()> {
    db.execute(
        "DELETE FROM chunk_fts WHERE id IN (SELECT id FROM chunks WHERE path=?1)",
        [path],
    )?;
    db.execute("DELETE FROM chunks WHERE path=?1", [path])?;
    Ok(())
}

fn path_predicate(path: &str) -> String {
    format!("path = '{}'", path.replace('\'', "''"))
}

fn vector_schema(dimensions: usize) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dimensions as i32,
            ),
            false,
        ),
    ]))
}

fn split_identifiers(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous = ' ';
    for c in text.chars() {
        if c.is_uppercase() && previous.is_lowercase() {
            output.push(' ');
        }
        if c == '_' {
            output.push(' ');
        } else {
            output.push(c);
        }
        previous = c;
    }
    output
}

fn lexical_query(query: &str) -> String {
    let expanded = format!("{query} {}", split_identifiers(query));
    let mut words: Vec<String> = expanded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(48)
        .map(|w| w.to_lowercase())
        .collect();
    words.sort();
    words.dedup();
    words
        .into_iter()
        .map(|w| format!("\"{w}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::Chunker;

    fn document(content: &str) -> Document {
        Document {
            path: "src/it's.rs".into(),
            hash: digest(content.as_bytes()),
            modified: "1".into(),
            size: content.len() as u64,
            language: "rust",
            content: content.into(),
            chunks: Chunker::default().split(content, "rust"),
        }
    }

    #[tokio::test]
    async fn interrupted_updates_are_hidden_and_retries_do_not_duplicate_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path(), 2).await.unwrap();
        let old = document("fn oldVersion() { println!(\"旧🦀\"); }");
        let ids = store.stage(&old).unwrap();
        assert!(store.lexical_ids("oldVersion").unwrap().is_empty());
        store
            .materialize(&old, &ids, &[vec![1.0, 0.0]])
            .await
            .unwrap();
        assert_eq!(store.lexical_ids("oldVersion").unwrap(), ids);
        let new = document("fn newVersion() { println!(\"新🦀\"); }");
        store.stage(&new).unwrap();
        assert!(store.candidate(&ids[0], 1.0).unwrap().is_none());
        drop(store);
        let mut store = Store::open(dir.path(), 2).await.unwrap();
        assert_eq!(store.known_files().unwrap()[&new.path].state, "pending");
        for _ in 0..2 {
            let ids = store.stage(&new).unwrap();
            store
                .materialize(&new, &ids, &[vec![0.0, 1.0]])
                .await
                .unwrap();
        }
        assert_eq!(store.vectors.count_rows(None).await.unwrap(), 1);
        let hits = store.lexical_ids("new version").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            store
                .candidate(&hits[0], 1.0)
                .unwrap()
                .unwrap()
                .text
                .contains("新🦀")
        );
        store.remove(&new.path).await.unwrap();
        assert!(store.lexical_ids("newVersion").unwrap().is_empty());
        assert_eq!(store.vectors.count_rows(None).await.unwrap(), 0);
    }

    #[test]
    fn fts_operators_are_treated_as_literal_terms() {
        assert_eq!(lexical_query("\" OR * ("), "\"or\"");
        assert!(lexical_query("**").is_empty());
    }

    #[tokio::test]
    async fn deleted_tombstones_hide_rows_until_vector_cleanup_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path(), 2).await.unwrap();
        let doc = document("fn authenticate() {}");
        let ids = store.stage(&doc).unwrap();
        store
            .materialize(&doc, &ids, &[vec![1.0, 0.0]])
            .await
            .unwrap();
        // Simulate process loss after committing the deletion marker, before vector cleanup.
        store
            .db
            .execute(
                "UPDATE files SET state='deleted' WHERE path=?1",
                [&doc.path],
            )
            .unwrap();
        drop(store);
        let mut store = Store::open(dir.path(), 2).await.unwrap();
        assert_eq!(store.vectors.count_rows(None).await.unwrap(), 1);
        assert!(store.lexical_ids("authenticate").unwrap().is_empty());
        assert!(store.candidate(&ids[0], 1.0).unwrap().is_none());
        assert_eq!(store.known_files().unwrap()[&doc.path].state, "deleted");
        store.remove(&doc.path).await.unwrap();
        assert!(store.known_files().unwrap().is_empty());
        assert_eq!(store.vectors.count_rows(None).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn missing_vector_table_invalidates_previously_ready_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path(), 2).await.unwrap();
        let doc = document("fn authenticate() {}");
        let ids = store.stage(&doc).unwrap();
        store
            .materialize(&doc, &ids, &[vec![1.0, 0.0]])
            .await
            .unwrap();
        drop(store);
        let db = lancedb::connect(dir.path().join("vectors").to_str().unwrap())
            .execute()
            .await
            .unwrap();
        db.drop_table("chunks", &[]).await.unwrap();
        let store = Store::open(dir.path(), 2).await.unwrap();
        assert_eq!(store.known_files().unwrap()[&doc.path].state, "pending");
        assert!(store.lexical_ids("authenticate").unwrap().is_empty());
    }
}
