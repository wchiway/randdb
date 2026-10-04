# RandDB architecture

RandDB is one Rust executable and one Cargo package. Its supported interface is `init`, `index`, `mcp`, and the `codebase-retrieval` tool.

## Index pipeline

```mermaid
flowchart LR
    W[Ignore-aware directory walk] --> P[Up to four parsing workers]
    P --> Q[Bounded queue: four file results]
    Q --> E[Embedding batches: up to 16 chunks]
    E --> S[SQLite pending transaction]
    S --> V[LanceDB replacement and verification]
    V --> R[SQLite ready marker]
```

Traversal prunes dependencies, build outputs, internal caches, and ignored directories. Symlinks are not followed. Each parser worker owns its parser cache and drops the AST after extracting chunks. Input and decoded-size limits bound file contents; the queue holds at most four completed file results. File metadata and the set of observed paths still scale with repository size.

Parsing and synchronous SQLite coordination run outside the asynchronous protocol/network tasks. The MCP process admits one retrieval pipeline at a time. An OS advisory lock serializes processes accessing the same repository/configuration index. Locks release on process exit; the lock file itself is not unlinked during use.

## Snapshot and recovery invariants

1. Every chunk range addresses the decoded UTF-8 source snapshot in SQLite. Parsing, storage, and slicing use the same byte coordinate system.
2. Only `ready` files contribute results. Vector IDs are resolved against SQLite before being returned.
3. Embeddings are computed before replacing the previous committed SQLite generation. An embedding failure stops the request.
4. A SQLite transaction replaces the source, chunk metadata, and lexical entries, marking the file `pending`.
5. LanceDB deletes previous rows for that path, inserts deterministic chunk IDs, and verifies the count. SQLite then marks the file `ready`.
6. A crash after step 4 leaves a pending file. The next scan reprocesses it even when timestamps match. Deleting previous rows before insertion prevents retry duplicates.
7. Deletion first hides the file and removes lexical/chunk entries in a transaction. A `deleted` tombstone remains until vector deletion succeeds.
8. Stale paths are pruned only after a successful complete directory walk. Traversal errors stop the request without deleting unvisited records.

A missing vector table invalidates ready markers and is recreated. Arbitrary external modification of database files is not a supported mutation API; use `index --force` or a new data directory when repairing externally damaged indexes.

## Retrieval

Each request validates its arguments, locks the project, and completes an incremental scan. Vector and lexical candidates are merged using weighted reciprocal rank fusion. Up to 40 ready chunks enter reranking; up to eight seeds are returned. A low top score returns the best seed with a warning. Reranker failure falls back to hybrid ordering with a warning; scores are heuristic relevance signals, not correctness probabilities.

Extraction adds only bounded neighboring lines in the same file. The complete Markdown response is limited by Unicode character count, including formatting. Technical terms strengthen lexical matching instead of filtering out other results.

## Configuration and storage

The `.env` parser does not mutate process-global environment. Explicit environment variables override the file. Index directories are keyed by canonical repository path and an embedding/chunker fingerprint: endpoint, model, dimensions, and format version. Changing models with unchanged dimensions still selects a new index.

## Build notes

LanceDB 0.39 requires the `remote` feature to compile unconditional job-error conversions referencing `Error::Http`. This is an upstream build workaround; RandDB connects only to local paths. SQLite/Tree-sitter contain native code and LanceDB requires `protoc` at build time. Native binaries must be built and tested for each supported platform.

## Validation boundaries

Tests use deterministic local HTTP providers and real embedded stores. Production-model relevance, large-repository peak RSS, API costs, and filesystem behavior on every platform need separate measurements. Changing implementation language alone does not establish a speedup.
