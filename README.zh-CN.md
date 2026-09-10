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

## 贡献

构建、调试、发布说明见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## License

MIT OR Apache-2.0
