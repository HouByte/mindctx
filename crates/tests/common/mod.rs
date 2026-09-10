// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared helpers for `crates/tests/` integration binaries.
//!
//! Included from each test binary via `#[path = "../common/mod.rs"] mod common;`
//! so a single source defines `fixture_root`, `encoding_root`, and the in-process
//! rmcp client helpers used by both `contract/` and `core/` test sets.

use std::path::PathBuf;

use mindctx_core::wire::WireMode;
use mindctx_mcp::MindctxServer;
use rmcp::RoleClient;
use rmcp::service::{RunningService, ServiceExt};

/// Resolve the polyglot fixture corpus used by every cross-language test.
/// Canonical absolute path, resolved via `canonicalize` to collapse any symlinks.
pub fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/polyglot")
        .canonicalize()
        .expect("polyglot fixture must exist")
}

/// Resolve the encoding fixture corpus (UTF-16/GBK/Big5/binary test inputs).
#[allow(dead_code)]
pub fn encoding_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/encoding")
        .canonicalize()
        .expect("encoding fixture must exist")
}

/// Connect an in-process rmcp client to a server instance over a duplex transport
/// (no subprocess, no stdio): both peers run inside this test's runtime.
///
/// Default wire mode is [`WireMode::Envelope`] — preserves the historical contract
/// assertions (envelope JSON parsing); tests targeting the LLM injection surface opt in
/// to [`WireMode::Text`] via [`connected_client_with_wire`].
#[allow(dead_code)]
pub async fn connected_client(root: PathBuf) -> RunningService<RoleClient, ()> {
    connected_client_with_wire(root, WireMode::Envelope).await
}

/// Same as [`connected_client`] but lets the caller pick the wire presentation mode.
#[allow(dead_code)]
pub async fn connected_client_with_wire(
    root: PathBuf,
    wire: WireMode,
) -> RunningService<RoleClient, ()> {
    let (client_transport, server_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let service = MindctxServer::with_wire(root, wire)
            .serve(server_transport)
            .await
            .expect("server initialization must succeed");
        let _ = service.waiting().await;
    });
    match ().serve(client_transport).await {
        Ok(client) => client,
        Err(e) => panic!("client handshake failed: {e}"),
    }
}
