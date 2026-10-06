# RandDB

使用 Rust 编写的本地代码索引与语义上下文检索工具。

[English](README.md)

RandDB 只提供一个 MCP 工具 `codebase-retrieval`：描述需要理解的代码，获取带路径和行号的相关片段。内部使用向量与词法混合召回、重排、有限的同文件补全和明确的输出预算，每次检索前自动更新索引。

## 安装

发布工作流为 Linux x86_64、macOS arm64 和 Windows x86_64 构建程序。预编译程序的使用者无需安装 Node.js 或 Rust，但仍需满足目标操作系统的运行库要求。

```sh
# macOS、Linux 和 WSL
curl -fsSL https://raw.githubusercontent.com/wchiway/randdb/main/install.sh | sh

# Windows PowerShell
irm https://raw.githubusercontent.com/wchiway/randdb/main/install.ps1 | iex
```

安装脚本会选择当前平台可用的最新版本，用发布资产中的 `SHA256SUMS` 校验下载内容，把可执行文件安装到 `~/.local/bin`（Windows 为 `%USERPROFILE%\.local\bin`），并创建配置模板。用 `--version v0.1.0-alpha.1` 指定版本，`--dir DIR` 指定安装目录，`--no-init` 跳过配置文件；PowerShell 脚本对应 `-Version`、`-InstallDir`、`-NoInit`。需要从镜像安装时设置 `RANDDB_RELEASE_BASE_URL` 和 `RANDDB_API_BASE_URL`。

Linux 版本在 Ubuntu 24.04 上构建，较旧的发行版可能需要从源码构建。

### 源码构建

源码构建需要稳定版 Rust（至少 1.91）、C/C++ 编译工具链和 `protoc`。Debian/Ubuntu 可使用 `sudo apt-get install build-essential protobuf-compiler` 安装构建依赖。本项目开发环境中的 CLI 工具（例如 `protoc`）由 mise 管理。

```sh
cargo build --release --locked
./target/release/randdb --help
# 或安装当前工作副本：
cargo install --path . --locked
```

## 配置

```sh
randdb init
```

编辑 `~/.randdb/.env`，填写两个 API 的密钥、完整请求地址、模型和实际向量维度：

```dotenv
EMBEDDINGS_API_KEY=your-key
EMBEDDINGS_BASE_URL=https://api.siliconflow.cn/v1/embeddings
EMBEDDINGS_MODEL=BAAI/bge-m3
EMBEDDINGS_DIMENSIONS=1024
RERANK_API_KEY=your-key
RERANK_BASE_URL=https://api.siliconflow.cn/v1/rerank
RERANK_MODEL=BAAI/bge-reranker-v2-m3
```

以上是服务配置示例。Embedding 接口应接受 OpenAI 风格请求；重排接口应接受 `query` 和 `documents`。URL 必须包含完整的 `/embeddings` 或 `/rerank` 请求路径。向量维度必须匹配模型实际输出。

进程环境变量优先于配置文件。通过 `RANDDB_HOME` 或 `--data-dir /path/to/data` 更换配置和索引目录。程序不会自动读取项目中的 `.env`。`init` 不覆盖已有文件，在 Unix 上新建配置文件的权限为 `0600`。

**被索引的代码片段会发送到配置的 Embedding 和 Reranker 服务。** 请使用适合仓库内容的服务。索引遵循嵌套 `.gitignore`，并排除 `.env` 文件、符号链接、构建产物和依赖目录；索引敏感仓库前应检查忽略规则。

## 使用

```sh
randdb index /absolute/path/to/repository
randdb index /absolute/path/to/repository --force
randdb mcp
```

`index` 用于预热索引并输出报告，`--force` 会重新计算当前配置对应的向量。检索也会增量索引，因此可以直接启动 MCP。协议消息使用 stdout，诊断日志使用 stderr。

MCP 客户端配置示例，必要时将 `command` 换成可执行文件绝对路径：

```json
{
  "mcpServers": {
    "randdb": { "command": "randdb", "args": ["mcp"] }
  }
}
```

| 参数 | 说明 |
| --- | --- |
| `repo_path` | 必填，仓库绝对路径。 |
| `information_request` | 必填，自然语言检索意图，最多 8000 个 UTF-8 字节。 |
| `technical_terms` | 可选，最多 20 个已知标识符；增强词法匹配，不是硬过滤器。 |
| `max_total_chars` | 可选，完整响应预算，包含格式文本；1000–80000 个 Unicode 字符，默认 24000。 |

```json
{
  "repo_path": "/home/me/project",
  "information_request": "索引写入失败后如何恢复？",
  "technical_terms": ["pending", "materialize"],
  "max_total_chars": 12000
}
```

## 索引行为

- 最多四个解析线程、四个排队文件结果；输入文件限制为 1 MiB，Embedding 请求每批最多 16 个分块。文件元数据仍随仓库规模增长。
- TypeScript/TSX、JavaScript/JSX、Python、Rust、Go、Java、C、C++、C#、Shell 使用 Tree-sitter；Ruby、PHP、Kotlin、Swift、Lua、Markdown、JSON 和常见配置/文本格式使用按行分块。
- 接受 UTF-8 和带 BOM 的 UTF-16。二进制、无效/不支持的编码和过大文件会跳过并记录原因。内部偏移统一针对保存的 UTF-8 解码快照。
- 修改时间和大小不变时跳过读取；元数据变化后计算哈希。对于同时保留时间戳与大小的外部修改，使用 `--force`。
- SQLite 保存正文快照、分块位置、词法索引和就绪状态；LanceDB 保存派生向量。未就绪数据不会返回，写入或删除中断后在下一轮索引重试。
- 上下文扩展限于同文件。不包含自动导入扩展、调用图、监听守护进程、符号导航套件或包管理器升级功能。
- 重排失败时返回混合召回排序并附提示；Embedding 失败会终止操作。

架构与恢复协议见 [architecture.md](docs/architecture.md)，旧版迁移说明见 [migration.md](docs/migration.md)。

## 开发

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

内存有限的构建环境可设置 `CARGO_BUILD_JOBS=2`。测试使用临时数据库和本地模拟 HTTP API，无需真实密钥，验证功能正确性，不代表真实模型效果或性能提升。

`scan.rs` / `chunk.rs` 负责扫描和分块，`store.rs` 负责持久化，`api.rs` 调用服务，`engine.rs` 负责索引与检索，`mcp.rs` / `main.rs` 提供协议和命令行入口。

维护者请参阅 [release / pre-release 工作流使用指南](docs/release.md)，了解标签约定、DeepSeek 英文摘要、必需 Secret 和失败恢复方法。

## 许可证

MIT。RandDB 是 ContextWeaver 的 Rust 后继版本，保留原 Git 历史与许可证署名。
