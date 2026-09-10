// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `glob` wire argument: a single pattern or an ordered list, shared by the search
//! include/exclude field and the glob tool's pattern field.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The `glob` wire parameter: one pattern or an ordered pattern list, published only in
/// the array form. A leading `!` marks an exclusion and always wins; a negative-only list
/// keeps every other file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlobArgs(pub Vec<String>);

impl<'de> Deserialize<'de> for GlobArgs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Untagged wire forms: a bare string widens to a one-element list; JSON null is
        // the absent parameter (same as the empty list).
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Absent,
            Single(String),
            List(Vec<String>),
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Absent => GlobArgs(Vec::new()),
            Wire::Single(pattern) => GlobArgs(vec![pattern]),
            Wire::List(patterns) => GlobArgs(patterns),
        })
    }
}

impl Serialize for GlobArgs {
    /// Published shape: always the array form.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(&self.0)
    }
}

impl GlobArgs {
    /// No patterns at all: the parameter's default.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_forms_string_and_array() {
        // Accepts a bare string and an array; always serializes the array form.
        let one: GlobArgs = serde_json::from_str(r#""*.rs""#).unwrap();
        assert_eq!(one.0, ["*.rs"]);
        let many: GlobArgs = serde_json::from_str(r#"["*.rs","!x.rs"]"#).unwrap();
        assert_eq!(many.0, ["*.rs", "!x.rs"]);
        let none: GlobArgs = serde_json::from_str("null").unwrap();
        assert!(none.is_empty());
        assert_eq!(
            serde_json::to_value(GlobArgs(vec!["*.rs".into()])).unwrap(),
            serde_json::json!(["*.rs"])
        );
        assert_eq!(
            serde_json::to_value(GlobArgs::default()).unwrap(),
            serde_json::json!([])
        );
    }
}
