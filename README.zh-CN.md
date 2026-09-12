# MindCtx

> **给 coding agent 的本地上下文工程层。**
> 内建工具让你"能拿到"，mindctx 让你"拿得准、拿得省、记得住"。
>
> [English](README.md)

mindctx 是一个 Rust 单二进制，以 MCP server 形式运行，给你的 coding agent 一个快、省预算的仓库浏览方式。无守护进程、无远程服务——就在你本机索引与检索，自动尊重 `.gitignore`。

## 安装

```bash
npm i -g mindctx                                         # npm
cargo install mindctx                                    # crates.io
```

验证：

```bash
mindctx --version
```

然后接入你的 agent（见 [MCP 接入](#mcp-接入)）。

## 为什么用 mindctx

三类浪费，三种解法：

| 浪费 | 解法 |
|---|---|
| **导航浪费** — grep 往返、反复遍历目录树 | 持久索引；`search`/`glob` 一次调用拿到结果 |
| **容量浪费** — 改十行却读整个文件 | 每个工具都有 token 预算；精确 o200k 记账 |
| **重复浪费** — 每个会话重新摸索、重复踩坑 | 分层知识库（markdown、可 git、跨宿主） |

## 工具

四个 MCP 工具，默认开启：

| 工具 | 作用 |
|---|---|
| `search` | 内容检索（Rust regex）。4 种 output_mode、glob/type 过滤、`head_limit`+`offset` 分页、skip 报告。mtime 倒序，**不做相关性排序**——判断交给 agent。 |
| `glob` | 文件发现，支持 `!` 排除、filter 模式、path/modified 排序、分页。 |
| `read` | 文本读取，1-based 行、token 预算为唯一上限、1–32 文件批量共享预算、编码自动探测。 |
| `outline` | 文件符号骨架（tree-sitter，6 语言）——不读全文先看结构。 |

所有结果都是 **envelope**：精确 token 记账（`token_usage`）+ `terminal`/`next_call` 续读契约——"被裁"是可继续的游标而非死胡同。

## MCP 接入

在 agent 的 MCP 配置里加上 mindctx：

```json
{
  "command": "/path/to/mindctx",
  "args": ["serve"]
}
```

`serve` 通过 stdio JSON-RPC 暴露四个工具。用 `--root /path/to/project` 指向项目根，或让它继承当前目录。项目根即检索语料（自动尊重 `gitignore`）。

常用参数：

- `--wire text|envelope` — wire 呈现模式。`text`（默认）是 LLM 注入面：每次调用返回一个被 token 预算约束的文本页。`envelope` 返回完整 envelope JSON（HTTP / IDE 插件 / 契约测试等机器消费方）。
- `MINDCTX_WIRE` 环境变量在没有 `--wire` 时生效。

## 安装脚本做了什么

一行安装（macOS / Linux / WSL）：

```bash
curl -fsSL https://mindctx.com/install.sh | bash
```

除把二进制放进 `~/.local/bin`，脚本还会给 `PATH` 上探测到的每个宿主接好：

| 步骤 | 效果 |
|---|---|
| MCP 注册 | 用 `claude mcp add --scope user` / `codex mcp add` 把 mindctx 注册为用户级 MCP server（`mindctx serve`，stdio） |
| Agent prompt | 把 [`scripts/agent-prompt.md`](scripts/agent-prompt.md) 的块写进 `~/.claude/CLAUDE.md` 与 `~/.codex/AGENTS.md`，用 `<!-- mindctx:begin -->` / `<!-- mindctx:end -->` 标记包裹 |
| 运行配置 | 往 `~/.mindctx/config.toml` 写 `[core]` 段，用 `# mindctx:begin:core` / `# mindctx:end:core` 标记包裹 |

prompt 块与二进制取自同一个 release tag，因此始终与所装版本一致。`PATH` 上没有的宿主直接跳过；只写 home 下的文件，绝不改动项目文件。

每一步都幂等：重复安装只会原地替换 mindctx 自己的块，文件其余内容不动。MCP 注册是 best-effort（失败只打 warning，安装继续）；prompt 模板下载失败则明确报错退出，不会留下半配置的宿主。

### 手动接入

安装脚本只是便利，不是必须。手工接入：

1. 注册 server：`claude mcp add --scope user --transport stdio mindctx -- mindctx serve`、`codex mcp add mindctx -- mindctx serve`，或直接改宿主配置（见 [MCP 接入](#mcp-接入)）
2. 把 [`scripts/agent-prompt.md`](scripts/agent-prompt.md) 的块追加到 agent 的指令文件（`~/.claude/CLAUDE.md` 或 `~/.codex/AGENTS.md`）
3. 可选：建 `~/.mindctx/config.toml`，写入 `[core]` 段

撤销时删掉自己追加的块，用 `claude mcp remove mindctx --scope user` / `codex mcp remove mindctx` 移除条目，并删掉因此变空的文件。

## 卸载

```bash
curl -fsSL https://mindctx.com/uninstall.sh | bash
```

加 `--purge` 会连同 `~/.mindctx/record/`（已移除的 `mindctx apply` 留下的 receipt 与备份）一起删除，默认保留：
`curl -fsSL https://mindctx.com/uninstall.sh | bash -s -- --purge`

卸载精确反转安装动作：只剥自己的标记块（剥完为空的宿主文件会删除）、通过宿主 CLI 移除 mindctx 条目、只删自己的二进制；文件里的其它内容一律保留。

## 状态与索引

只读辅助命令：

- `status` — 版本、项目根、run 目录是否存在、索引状态、检索语料大小
- `index` — 语料遍历报告（文件数、Top 扩展名、总字节数）。检索直接走 rg 层，无需预建索引

## 贡献

构建、调试、发布说明见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## License

MIT OR Apache-2.0
