# 发布指南

本文面向 RandDB 维护者，介绍正式版和预发布版的配置、发布流程、构建产物及故障恢复方法。

发布流程由版本标签触发，使用 GitHub Actions 完成多平台构建，调用 DeepSeek 生成英文发布说明，并将构建产物上传到 GitHub Release。

## 1. 工作流与触发方式

| 文件 | 职责 | 自动触发 | 手动触发 |
| --- | --- | --- | --- |
| `.github/workflows/release.yml` | 正式版入口 | `v*` 标签，排除包含 `-` 的标签 | 传入已有正式版标签，例如 `v0.1.0` |
| `.github/workflows/prerelease.yml` | 预发布入口 | `v*-*` 标签 | 传入已有预发布标签，例如 `v0.2.0-rc.1` |
| `.github/workflows/publish.yml` | 两个入口共用的校验、构建、摘要和上传流程 | 由入口调用 | 不直接运行 |
| `.github/workflows/install.yml` | 安装脚本测试：三平台构建，再对着本地模拟的发布目录执行 `install.sh` / `install.ps1` | 改动安装脚本或测试脚本时 | 直接运行 |

正式版标签采用 `vMAJOR.MINOR.PATCH`。预发布标签采用 `vMAJOR.MINOR.PATCH-标识符`，例如 `v0.2.0-alpha.1`、`v0.2.0-beta.1`、`v0.2.0-rc.1`。数字标识符不能包含前导零；当前自动化不接受 `+build` 元数据。

入口的标签通配符只负责分流，脚本还会验证合法版本、渠道以及 Cargo 版本是否一致。不能通过正式版入口发布 RC，也不能通过预发布入口发布正式版。

### 执行顺序

1. **Prepare**：运行发布脚本测试，校验标签、`Cargo.toml` 与 `Cargo.lock` 中 RandDB 的版本，以及 API 配置。解析标签对应的 commit SHA。
2. **Build**：各平台检出同一个已校验 SHA，执行格式检查、Clippy、测试、release 构建，并上传归档到当前 Actions run。
3. **Notes**：全部平台构建成功后，调用 DeepSeek Responses API 生成英文 Markdown，保存为 `release-notes` artifact。
4. **Publish**：下载归档和摘要，再次检查远程标签是否仍指向已构建的 SHA，生成校验和。先创建或恢复 draft，上传全部资产成功后再公开发布。

同一标签的自动触发和手动触发共用并发组，不会相互取消。不同标签可以并行运行。构建和摘要任务仅有仓库读取权限；只有最后的 Publish 任务具有 `contents: write`。

## 2. 发布前的一次性配置

### 仓库与 GitHub Actions

发布前确认以下条件：

- 工作流和 `.github/scripts/` 已提交并推送到仓库。要发布的标签必须包含这些文件。
- GitHub Actions 已启用，并允许工作流中的第三方 actions。
- 组织或仓库策略允许发布任务使用 `GITHUB_TOKEN` 的 `contents: write` 权限。
- 维护者具有推送标签、管理 Actions Secrets/Variables 和创建 Release 的权限。
- Linux、macOS 和 Windows 构建所需的 GitHub-hosted runner 可用。

建议保护发布标签，只允许维护者创建；已经发布的标签不要移动或覆盖。

### DeepSeek 配置

在仓库 **Settings → Secrets and variables → Actions** 中配置：

| 类型 | 名称 | 是否必需 | 默认值或用途 |
| --- | --- | --- | --- |
| Secret | `DEEPSEEK_API_KEY` | 必需 | DeepSeek API 密钥。缺失时 Prepare 失败，不开始构建。 |
| Variable | `DEEPSEEK_RESPONSES_URL` | 可选 | 默认为 `https://api.deepseek.com/responses`。必须是完整的 HTTPS `/responses` 请求地址，不允许 URL 凭据、查询参数或 fragment。 |
| Variable | `DEEPSEEK_MODEL` | 可选 | 默认为 `deepseek-flash`。覆盖值必须是所配置 endpoint 支持的模型。 |

也可用 GitHub CLI 配置。密钥命令交互读取输入，不要将密钥直接写进命令行或提交到仓库：

```sh
gh secret set DEEPSEEK_API_KEY --repo wchiway/randdb

# 以下变量有默认值，不设置也可以。
gh variable set DEEPSEEK_RESPONSES_URL --repo wchiway/randdb \
  --body 'https://api.deepseek.com/responses'
gh variable set DEEPSEEK_MODEL --repo wchiway/randdb \
  --body 'deepseek-flash'
```

`GITHUB_TOKEN` 由 Actions 自动提供，不需要额外创建同名 Secret。嵌入检索使用的 `EMBEDDINGS_*`、`RERANK_*` 配置与发布摘要无关，工作流不读取它们。

### 摘要生成与数据范围

摘要使用 `POST /responses`。请求采用 Bearer 认证、非流式响应、`reasoning.effort: none` 和受限输出预算，不传入官方 Responses API 不支持的 `store` 参数。

摘要请求向配置的 DeepSeek endpoint 发送版本、渠道、commit SHA、比较基线、提交标题和变更文件路径。不发送源码 diff、文件正文、提交正文、仓库 `.env`、GitHub token 或其他 Secrets。提交标题和文件路径不应包含凭据等敏感信息。

默认最多使用 100 条提交标题、200 个文件条目，每条最多 500 个字符。发生截断时，发布说明会标记摘要范围受限。

- 正式版：选择当前 commit 的祖先标签中，语义版本低于当前版本的最高正式版标签作为基线，忽略 RC 等预发布标签。
- 预发布版：允许选择更低版本的正式版或预发布祖先标签，同样按语义版本排序。
- 没有符合条件的标签时：只提供最新提交标题和当前跟踪的文件路径，不把继承的整个 ContextWeaver 历史当作 RandDB 新增变更。
- 基线是 Git 标签，不保证对应成功发布的 GitHub Release。未成功发布的旧标签也可能成为基线。

提示词要求英文正文、保留代码标识符原样、不编造功能或验证结果，并将提交文本当作数据而非指令。脚本检查响应完成状态、非空正文和以下英文标题：

```markdown
## Highlights
## Changes
## Breaking Changes
## Upgrade Notes
```

脚本将版本、渠道、commit SHA 和比较基线追加到 `Release Details`。自动校验检查响应结构和英文标题；维护者仍应检查生成说明与实际变更是否一致。

## 3. 发布正式版

以下以 `v0.1.0` 为例。确认该版本尚未发布，再执行相应步骤。

### 准备版本

检查以下值完全一致：

| 位置 | 示例 |
| --- | --- |
| `Cargo.toml` 的 `[package].version` | `0.1.0` |
| `Cargo.lock` 中本地 `randdb` 包的 `version` | `0.1.0` |
| Git 标签 | `v0.1.0` |

更新版本后，用 `cargo check` 更新根包锁文件记录，再检查 `git diff -- Cargo.toml Cargo.lock`。不要为了修改版本号无意升级全部依赖。

本地检查：

```sh
python3 -m unittest discover -s .github/scripts/tests -v
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

内存有限的构建环境可设置 `CARGO_BUILD_JOBS=2`。本地检查不能替代目标平台的 Actions 构建。

### 提交、合并并推送标签

遵循项目的分支/worktree 流程，将版本改动合并到 `main`。只暂存本次发布需要的文件，不要误提交本地配置或其他未完成改动。

在已合并且检查过的 `main` 上运行：

```sh
git push origin main
git tag -a v0.1.0 -m 'Release v0.1.0'
git push origin v0.1.0
```

推送标签会启动 **RandDB release**。不要再手动创建同标签的 GitHub Release，工作流负责生成英文说明和上传资产。

`git push origin main` 本身只触发 CI，不发布版本。如果用另一个 Actions 工作流的默认 `GITHUB_TOKEN` 推送标签，GitHub 通常不会因此再次触发工作流；这里的步骤面向维护者本地推送标签。

## 4. 发布预发布版

例如先将 Cargo 清单和锁文件中的 RandDB 版本都更新为 `0.2.0-rc.1`，完成检查、提交和合并，然后运行：

```sh
git push origin main
git tag -a v0.2.0-rc.1 -m 'Prerelease v0.2.0-rc.1'
git push origin v0.2.0-rc.1
```

这会启动 **RandDB pre-release**。英文摘要将说明这是测试预览版本，GitHub Release 标记为 prerelease，并明确设置为非 Latest。

从 RC 转为正式版时，将两个 Cargo 版本更新为不带后缀的 `0.2.0`，完成相同检查后创建新的 `v0.2.0` 标签。不要将旧 RC 标签移动到新 commit。

## 5. 手动运行与失败恢复

工作流已存在于默认分支后，可在 Actions 页面选择 **Run workflow**，选择包含工作流的分支，并输入一个已经存在的标签。标签所在 commit 也必须包含发布脚本；构建源码来自标签，而不是随意选择的分支 HEAD。

GitHub CLI 示例：

```sh
# 手动发布或恢复已有的正式版标签。
gh workflow run release.yml --repo wchiway/randdb --ref main \
  -f tag=v0.1.0

# 手动发布或恢复已有的预发布标签。
gh workflow run prerelease.yml --repo wchiway/randdb --ref main \
  -f tag=v0.2.0-rc.1

# 查看运行记录。
gh run list --repo wchiway/randdb --workflow release.yml
gh run list --repo wchiway/randdb --workflow prerelease.yml
```

可在 GitHub 界面重跑失败任务，或用以上入口重新运行同一标签。两个入口不会自动创建 Git 标签。

| 失败位置 | 发布结果 | 恢复方法 |
| --- | --- | --- |
| 标签、版本或配置校验 | 不构建、不发布 | 修正配置，或提交正确版本后使用新标签。 |
| 任一平台检查或构建 | 不调用摘要、不发布 | 解决平台问题，创建新版本标签；临时基础设施错误可重跑。 |
| DeepSeek 请求、解析或摘要校验 | 不发布 | 检查 Secret、endpoint、模型、额度和服务状态后重跑。 |
| 归档或校验和验证 | 不发布 | 检查所有平台 artifact，必要时重跑整个流程。 |
| 上传资产失败 | 保留 draft，不公开不完整版本 | 重跑时恢复 draft，使用当前运行的说明及资产重新上传。 |
| 远程标签被移动 | 拒绝发布 | 恢复标签一致性并人工核查，优先使用新版本标签。 |
| 同标签已成功发布 | 不覆盖既有说明和资产 | 仅确认已发布；内容修订应由维护者另行处理。 |

API 连接错误、HTTP 429 和 5xx 最多尝试 3 次。401/403、其他非重试状态、重定向、无效 JSON、`incomplete`、拒绝响应或空摘要会直接失败。日志不会打印 API 密钥或原始响应正文。

Actions artifact 保留 14 天。恢复较旧运行时，如果 artifact 已过期，应重跑整个工作流，而不是只重跑 Publish。已发布的 GitHub Release 资产不受这个 Actions artifact 保留期影响。

当前流程在摘要成功后自动发布，不包含人工批准环节。如果需要人工审批，可另外配置受保护的 GitHub Environment；这不是默认行为。

## 6. 发布资产

以 `v0.1.0` 为例：

| Runner | 目标 | 资产 |
| --- | --- | --- |
| `ubuntu-24.04` | `x86_64-unknown-linux-gnu` | `randdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz` |
| `macos-14` | `aarch64-apple-darwin` | `randdb-v0.1.0-aarch64-apple-darwin.tar.gz` |
| `windows-2022` | `x86_64-pc-windows-msvc` | `randdb-v0.1.0-x86_64-pc-windows-msvc.tar.gz` |
| Publish job | 所有归档 | `SHA256SUMS` |

预发布资产使用完整版本标签，例如 `randdb-v0.2.0-rc.1-x86_64-unknown-linux-gnu.tar.gz`。Windows 也使用 `.tar.gz`，内部为 `randdb.exe`。其余归档内部为 `randdb`。

两个渠道共享相同的构建矩阵、测试要求及校验和流程。构建产物通过 [GitHub Releases](https://github.com/wchiway/randdb/releases) 分发；工作流不发布 npm 包或 crates.io 包。

## 7. 修改工作流后的验证

发布自动化使用 Python 3.13 标准库、Git 和 GitHub CLI，不增加 Node 依赖，也不需要真实 API 密钥运行单元测试。

```sh
python3 -m unittest discover -s .github/scripts/tests -v
actionlint
git diff --check
```

单元测试覆盖版本校验、变更范围、API 响应处理和发布恢复逻辑，不访问外部服务。发布后检查 Actions 运行结果、英文发布说明和资产校验和。

### 官方参考

- [DeepSeek Responses API 指南](https://api-docs.deepseek.com/guides/responses_api)
- [DeepSeek Create Response API](https://api-docs.deepseek.com/api/create-response)
- [DeepSeek 模型列表](https://api-docs.deepseek.com/api/list-models)
- [GitHub 复用工作流](https://docs.github.com/en/actions/sharing-automations/reusing-workflows)
- [GitHub 手动运行工作流](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow)
