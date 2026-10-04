# RandDB

Local code indexing and semantic context retrieval for AI agents, written in Rust.

[简体中文](README.zh-CN.md)

RandDB exposes one MCP tool, `codebase-retrieval`: describe the code you need and receive relevant snippets with paths and line numbers. It combines vector and lexical retrieval, reranking, limited same-file context, and an explicit output budget. Indexes update before each search.

## Install

Build prerequisites: stable Rust (1.91 or newer), a C/C++ toolchain, and `protoc`. On Debian/Ubuntu, install native prerequisites with `sudo apt-get install build-essential protobuf-compiler`. This project's development environment manages CLI tools such as `protoc` with mise.

```sh
cargo build --release --locked
./target/release/randdb --help
# Or install this checkout:
cargo install --path . --locked
```

Tagged releases build executables for Linux x86_64, macOS arm64, and Windows x86_64. Prebuilt executables do not require Node.js or Rust on the destination machine; operating-system runtime requirements still apply.

## Configure

```sh
randdb init
```

Edit `~/.randdb/.env` and supply both API keys, complete endpoint URLs, models, and actual embedding dimensions:

```dotenv
EMBEDDINGS_API_KEY=your-key
EMBEDDINGS_BASE_URL=https://api.siliconflow.cn/v1/embeddings
EMBEDDINGS_MODEL=BAAI/bge-m3
EMBEDDINGS_DIMENSIONS=1024
RERANK_API_KEY=your-key
RERANK_BASE_URL=https://api.siliconflow.cn/v1/rerank
RERANK_MODEL=BAAI/bge-reranker-v2-m3
```

These are example settings. Embedding services must accept OpenAI-style embedding requests; rerank services must accept `query` and `documents`. URLs include the full `/embeddings` or `/rerank` request path. Environment variables override the file. `init` preserves existing files and creates new configuration files with mode `0600` on Unix.

Change the data location with `RANDDB_HOME` or `--data-dir /path/to/data`. RandDB does not automatically read a repository's `.env`.

**Indexed snippets are sent to the configured embedding and reranking services.** Use providers suitable for your repositories. Nested `.gitignore` rules apply; `.env` files, symlinks, build outputs, and dependency directories are excluded. Review ignore rules before indexing sensitive repositories.

## Use

```sh
randdb index /absolute/path/to/repository
randdb index /absolute/path/to/repository --force
randdb mcp
```

`index` prewarms the cache and prints an index report. `--force` recomputes embeddings for the current configuration. Running `index` separately is optional: each search performs an incremental update. MCP uses stdio; diagnostic logs go to stderr.

Configure your MCP client, using the executable's absolute path if needed:

```json
{
  "mcpServers": {
    "randdb": { "command": "randdb", "args": ["mcp"] }
  }
}
```

| Tool input | Meaning |
| --- | --- |
| `repo_path` | Required absolute repository path. |
| `information_request` | Required natural-language question, at most 8000 UTF-8 bytes. |
| `technical_terms` | Optional known identifiers, up to 20; lexical hints, not hard filters. |
| `max_total_chars` | Optional complete response budget, including formatting; 1000–80000 Unicode characters, default 24000. |

```json
{
  "repo_path": "/home/me/project",
  "information_request": "How are failed index writes recovered?",
  "technical_terms": ["pending", "materialize"],
  "max_total_chars": 12000
}
```

## Index behavior

- At most four parsing workers and four queued file results; input files are limited to 1 MiB. Embedding requests contain at most 16 chunks. File metadata still scales with repository size.
- Tree-sitter handles TypeScript/TSX, JavaScript/JSX, Python, Rust, Go, Java, C, C++, C#, and shell. Ruby, PHP, Kotlin, Swift, Lua, Markdown, JSON, and common configuration/text formats use line-based chunks.
- UTF-8 and BOM-marked UTF-16 are accepted. Binary, invalid/unsupported-encoding, and oversized files are skipped with diagnostics. Stored offsets address the decoded UTF-8 snapshot.
- Unchanged modification time and size avoid file reads; changed metadata triggers hashing. Use `--force` for external tools that preserve both timestamp and size while modifying content.
- SQLite owns source snapshots, chunk locations, lexical indexes, and readiness. LanceDB stores derived vectors. Pending files are hidden; interrupted writes and deletions retry on the next scan.
- Same-file context is bounded. There is no import expansion, call graph, watcher daemon, symbol-navigation suite, or package-manager updater.
- A failed reranker request falls back to hybrid ordering with a warning. Embedding failures stop the operation.

See [architecture and recovery](docs/architecture.md) and [migration from ContextWeaver](docs/migration.md).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Use `CARGO_BUILD_JOBS=2` on memory-constrained build hosts. Tests use temporary databases and a deterministic local HTTP provider; real credentials are unnecessary. They verify correctness, not production-model relevance or speedups.

`scan.rs` and `chunk.rs` handle ingestion; `store.rs` coordinates persistence; `api.rs` calls providers; `engine.rs` indexes and retrieves; `mcp.rs` and `main.rs` expose the tool and CLI.

## License

MIT. RandDB succeeds ContextWeaver; Git history and existing license attribution are retained.
