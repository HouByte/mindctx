// SPDX-License-Identifier: MIT OR Apache-2.0

//! Public-API behavior of `mindctx_core::index::walk` — the corpus enumerator
//! behind the index/status CLI surface.

use std::fs;
use std::path::Path;

use mindctx_core::index::walk;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn walk_respects_gitignore_and_hidden_and_mindctx() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "src/main.rs", "fn main() {}");
    write(root, "notes.md", "doc");
    write(root, "generated.bin", &"x".repeat(1024));
    write(root, ".gitignore", "generated.bin\n");
    write(root, ".mindctx/index.db", "junk");
    fs::write(root.join(".hidden"), "nope").unwrap();

    let files = walk(root).unwrap();
    let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
    assert_eq!(rels, ["notes.md", "src/main.rs"]);
}
