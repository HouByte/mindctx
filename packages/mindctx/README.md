# MindCtx

> **Local context engineering for coding agents.**
> Built-in tools get you *something*. mindctx gets you the *right thing, within budget, and remembers.*

A single Rust binary that runs as an MCP server: four tools — `search` / `glob` / `read` /
`outline` — over your repository, with exact o200k token accounting and an envelope
continuation contract so truncation is a cursor, not a dead end. This npm package is a thin
launcher that spawns the platform binary as a subprocess (no postinstall, zero dependencies).

## Install

```bash
npm i -g mindctx
mindctx --version
```

## Connect

Add it to your agent's MCP config:

```json
{
  "command": "/path/to/mindctx",
  "args": ["serve"]
}
```

Full docs: [github.com/HouByte/mindctx](https://github.com/HouByte/mindctx).

## License

MIT OR Apache-2.0
