# polyglot

Six-language micro-repo with fixed fixtures: tree-sitter outline, search relevance, and search performance baseline.

## Conventions

- **>=20 symbols per language** with known nested structures; all around a shared theme (HTTP client retry policy: retry / backoff / jitter / Retry-After) so search relevance can be asserted across languages.
- Kept tiny, deterministic, no third-party deps, **not compiled** (tree-sitter only parses).
- `notes.md` provides non-code corpus (docs material is also indexed).

## Symbol theme (aligned across languages)

`RetryPolicy` (struct + methods), `BackoffKind` (enum/constants), `HttpClient`
(struct/class + get/post/setHeader/send), `Transport` (trait/interface),
`RetryError`, `retry_with_backoff` (core retry loop), `parse_retry_after`,
`full/equal jitter`, `HeaderMap`, `USER_AGENT` constant.
