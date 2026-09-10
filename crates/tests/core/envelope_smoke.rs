// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration smoke test: the envelope contract holds outside the crate boundary, locked by the inline assertions here and in the envelope unit tests.

use mindctx_core::envelope::Envelope;

#[test]
fn envelope_shape_is_accepted_from_outside_the_crate() {
    let raw = r#"{
        "text": "crates/core/src/retrieve/mod.rs\n1-hybrid ranking\n\n(Complete: 1 result shown.)",
        "token_usage": { "returned": 10, "budget": 4000 },
        "truncated": false,
        "terminal": { "state": "complete", "unit": "results", "shown_from": 1, "shown_to": 1 }
    }"#;

    let e: Envelope =
        serde_json::from_str(raw).expect("envelope must parse across the crate boundary");
    assert!(e.text.as_deref().unwrap().contains("hybrid ranking"));
    assert_eq!(e.token_usage.returned, 10);
    assert!(!e.truncated);
}

/// Payloads carrying fields retired from the contract (`results`/`citations`/
/// `kb_hits`/`truncation_pointer`/`Hit.score`/`Hit.tokens`) must still deserialize:
/// unknown fields are ignored, so removing a field never breaks an old payload.
#[test]
fn envelope_still_parses_payloads_with_retired_fields() {
    let raw = r#"{
        "results": [
            { "path": "a.rs", "lines": [1, 2], "snippet": "x", "score": 0.5, "tokens": 10 }
        ],
        "citations": ["a.rs:1-2"],
        "kb_hits": [],
        "token_usage": { "returned": 10 },
        "truncated": false,
        "truncation_pointer": null
    }"#;

    let e: Envelope =
        serde_json::from_str(raw).expect("payloads carrying retired fields must parse");
    assert_eq!(e.token_usage.returned, 10);
}
