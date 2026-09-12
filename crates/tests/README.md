# Integration tests (crates/tests/)

All cross-crate integration tests and shared fixtures live in this workspace member.
Per-crate `crates/*/tests/` directories are intentionally absent — discipline is
"test folders in one place only", scoped to `crates/tests/` and its sub-paths.

Layout:

- `contract/` — MCP-side integration tests.
  - `cross_tool_invariants.rs` — cross-tool invariants (schema portability, terminal
    note grammar, skip-report cap, budget reservation interlock, encoding matrix).
  - `mcp_published_surface.rs` — published tool/input schemas vs the fixture file
    `contract/expected_schemas.json`.
  - `cli_mcp_roundtrip.rs` — CLI-spawned MCP server round-trips and text-wire
    renderings vs fixture files under `contract/wire_text/`.
- `core/` — unit-level cross-crate integration tests.
  - `encoding_matrix.rs` — encoding-decision ladder over the fixtures in
    `fixtures/encoding/`.
  - `envelope_smoke.rs` — envelope JSON shape and serde round-trip.
  - `search_read_loop.rs` — search → read cross-module close-the-loop.
  - `outline.rs` — tree-sitter outline structural assertions across the polyglot
    fixture set.
- `fixtures/` — version-controlled input data referenced by the tests above.
  - `polyglot/` — 6-language micro-repository (rust/go/ts/python/java/cpp).
    Small, deterministic, no third-party deps, never compiled.
  - `encoding/` — encoding-decision ladder fixtures (utf-8, fallback, ambiguous).
  - `search/` — additional fixture families.

Notes:

- Layering: unit tests stay with their crate (`#[cfg(test)] mod tests` inside
  `crates/*/src/`); all integration tests + fixtures live here.
