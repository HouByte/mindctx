<!-- mindctx:begin -->
Code navigation: prefer the mindctx MCP tools (search / glob / read / outline) over built-in grep/glob/read — one budgeted call replaces repeated round-trips.

- Cross-file questions → `search` (regex); file discovery → `glob`. Results are mtime-ordered, not ranked — judging relevance is your job.
- `outline` before `read` on code files (rs / go / ts / py / java / c-family — other extensions skip it); either way, `read` with offset/limit ranges.
- A `Partial` page is a cursor: resume only with the exact arguments its status line (or `next_call`) names.
- Paths may be project-relative, absolute, `~`-prefixed, or contain `..` — no need to `cd` first.
<!-- mindctx:end -->
