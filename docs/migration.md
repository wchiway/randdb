# Migrating from ContextWeaver to RandDB

RandDB is a Rust rewrite with a smaller interface. Its initial preview release uses version `0.1.0-alpha.1` under the new executable/package identity.

1. Install or build `randdb`, then run `randdb init`.
2. Copy your embedding/reranker settings into `~/.randdb/.env`. The seven provider variables retain their names. Old `CW_SEARCH_*` settings are unused. Keep credentials out of Git.
3. Change the MCP client's command from `contextweaver mcp` to `randdb mcp`; an absolute executable path avoids PATH ambiguity.
4. Run `randdb index /absolute/path` or let the first query create the index. Rebuilding sends chunks to the embedding provider and can incur API cost.

RandDB does not open, convert, delete, or modify `~/.contextweaver`. Its UTF-8 byte offsets and index fingerprint are incompatible with the old UTF-16 cache. Retain the old executable and data directory for rollback. Remove old caches manually only after validating the new version.

## Interface changes

- `codebase-retrieval` is the only MCP tool. Required inputs remain `repo_path` and `information_request`; optional inputs are `technical_terms` and `max_total_chars`.
- Technical terms are lexical hints, not required matches. Unsupported parameters produce validation errors rather than being ignored.
- Retrieval modes, glob/language filters, file/segment controls, debug switches, selectable confidence handling, and JSON/both formats are removed. The output is bounded Markdown with locations and snippets.
- CLI `watch`, `search`, `migrate`, `stats`, `config`, `update`, and file/symbol navigation mirrors are removed. Configure the `.env` file, update using release/install tooling, and use the agent's file tools for exact navigation.
- Automatic import/call-site expansion and symbol graph storage are removed. Cross-file results can still be retrieved directly through semantic and lexical search.
- Ruby/PHP/Kotlin/Swift/Lua remain searchable with line-based chunks; their native grammars are not bundled initially. Unsupported encodings must be converted before indexing.

## Recovery

Rerun interrupted operations: pending writes and deletion tombstones are retried. `randdb index /path --force` deliberately recomputes embeddings. Changing embedding endpoint/model/dimensions selects a separate cache automatically. Neither operation modifies old ContextWeaver data.

RandDB uses a single composite hash directory under `indexes/`. Indexes created by development builds with nested repository/configuration hash directories are not reused or deleted. The next indexing operation builds a new cache and can incur embedding costs; remove superseded caches manually only after validation.

Product identity, executable, Cargo package, MCP server, and data directory are renamed. The checkout directory and GitHub repository URL remain unchanged until explicitly renamed by the owner; existing source links continue to work.
