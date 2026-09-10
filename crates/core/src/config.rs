// SPDX-License-Identifier: MIT OR Apache-2.0

//! Project layout helpers: the per-project `.mindctx/` directory holding the
//! configuration override and the on-disk run state.

use std::path::{Path, PathBuf};

/// Project directory `<root>/.mindctx/`: config.toml overrides + index.db (gitignored).
pub fn project_dir(root: &Path) -> PathBuf {
    root.join(".mindctx")
}
