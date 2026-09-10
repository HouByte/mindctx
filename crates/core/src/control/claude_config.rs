// SPDX-License-Identifier: MIT OR Apache-2.0

//! Claude Code (`~/.claude.json`) configuration planning and the surgical text edits that
//! upsert/remove the `mcpServers.mindctx` entry without touching the user's other bytes.

use std::path::Path;

use super::apply::{ApplyOpts, ChangeSet, FileChange};
use super::fsatomic::read_file_or_empty;
use crate::error::{Error, Result};

/// Plan Claude Code (~/.claude.json) configuration.
///
/// The `mcpServers.mindctx` entry is upserted surgically on the raw text, leaving every other
/// byte of the user's document (key order, indentation, unrelated content) untouched. A full
/// rewrite is only used as a fallback for awkward-but-valid structures.
pub(crate) fn plan_claude_config(home: &Path, opts: &ApplyOpts, set: &mut ChangeSet) -> Result<()> {
    let config_path = home.join(".claude.json");
    let (original_existed, original_bytes) = read_file_or_empty(&config_path)?;

    // Use the binary path from opts (injected at CLI entry, never env::current_exe() in library code)
    let binary_path_str = opts.binary_path.to_string_lossy().to_string();

    let mut mindctx_value = serde_json::json!({
        "command": binary_path_str,
        "args": ["serve"],
    });
    // Env values are strings in both hosts; omitted entirely without --budget.
    if let Some(budget) = opts.budget {
        mindctx_value["env"] = serde_json::json!({ "MINDCTX_TOKEN_BUDGET": budget.to_string() });
    }

    let new_bytes: Vec<u8> = if !original_existed || original_bytes.trim_ascii().is_empty() {
        // Fresh (or empty) file: write our own minimal document.
        let doc = serde_json::json!({ "mcpServers": { "mindctx": mindctx_value } });
        let mut bytes = serde_json::to_vec_pretty(&doc)
            .map_err(|e| Error::Config(format!("failed to serialize Claude Code config: {e}")))?;
        bytes.push(b'\n');
        bytes
    } else {
        // Validate the existing document first: a malformed config is an error, never clobbered.
        let parsed: serde_json::Value = serde_json::from_slice(&original_bytes)
            .map_err(|e| Error::Config(format!("failed to parse Claude Code config: {e}")))?;
        if parsed.as_object().is_none() {
            return Err(Error::Config(
                "Claude Code config root is not a JSON object".into(),
            ));
        }
        let text = String::from_utf8(original_bytes.clone())
            .map_err(|_| Error::Config("Claude Code config is not valid UTF-8".into()))?;

        match upsert_claude_mindctx(&text, &mindctx_value)? {
            Some(new_text) => new_text.into_bytes(),
            None => {
                // Fallback: full rewrite for awkward-but-valid structures.
                let mut doc = parsed;
                let root = doc.as_object_mut().ok_or_else(|| {
                    Error::Config("Claude Code config root is not a JSON object".into())
                })?;
                let mcp_servers = root
                    .entry("mcpServers")
                    .or_insert_with(|| serde_json::json!({}));
                let mcp_servers = mcp_servers.as_object_mut().ok_or_else(|| {
                    Error::Config("`mcpServers` in Claude Code config is not a JSON object".into())
                })?;
                mcp_servers.insert("mindctx".into(), mindctx_value);
                let mut bytes = serde_json::to_vec_pretty(&doc).map_err(|e| {
                    Error::Config(format!("failed to serialize Claude Code config: {e}"))
                })?;
                if original_bytes.ends_with(b"\n") {
                    bytes.push(b'\n');
                }
                bytes
            }
        }
    };

    // Only add change if bytes differ (idempotence)
    if original_bytes != new_bytes {
        set.add(FileChange::write(
            config_path,
            original_bytes,
            new_bytes,
            original_existed,
        ));
    }

    Ok(())
}

/// Surgical upsert of the `mcpServers.mindctx` entry into a JSON object document.
///
/// - Ok(Some(text)): edited document, everything except the managed entry byte-identical.
/// - Ok(None): structure is valid but too awkward for a surgical edit; caller falls back to a
///   full rewrite.
/// - Err: `mcpServers` / `mcpServers.mindctx` exists but is not a JSON object — a malformed
///   config must surface as an error, never be silently rewritten.
fn upsert_claude_mindctx(text: &str, mindctx_value: &serde_json::Value) -> Result<Option<String>> {
    let malformed = |what: &str| {
        Error::Config(format!(
            "`{what}` in Claude Code config is not a JSON object"
        ))
    };

    // Locate the top-level "mcpServers" member.
    let mut sc = Scanner::new(text);
    sc.skip_ws();
    if sc.eat(b'{').is_none() {
        return Err(Error::Config(
            "Claude Code config root is not a JSON object".into(),
        ));
    }
    let root_after_open = sc.pos;
    let mut root_member_indent: Option<String> = None;
    let mut mcp_span: Option<(usize, usize)> = None;
    let mut mcp_key_indent = String::new();
    loop {
        sc.skip_ws();
        match sc.peek() {
            Some(b'}') => break,
            Some(b',') => {
                sc.bump();
                continue;
            }
            Some(b'"') => {}
            _ => return Ok(None),
        }
        let key_start = sc.pos;
        let (_, key_end) = sc.scan_string().ok_or_else(|| {
            Error::Config("malformed Claude Code config (unterminated string)".into())
        })?;
        if root_member_indent.is_none() {
            root_member_indent = Some(line_indent(text, key_start).to_string());
        }
        let key = &text[key_start + 1..key_end - 1];
        sc.skip_ws();
        if sc.eat(b':').is_none() {
            return Ok(None);
        }
        sc.skip_ws();
        let val_start = sc.pos;
        let val_end = sc
            .scan_value()
            .ok_or_else(|| Error::Config("malformed Claude Code config".into()))?;
        if key == "mcpServers" {
            if mcp_span.is_some() {
                // Duplicate "mcpServers" members: a JSON parser keeps the last one but this
                // scanner would edit the first — refuse to guess; the caller
                // falls back to a full rewrite. Keep scanning so later duplicates are seen.
                return Ok(None);
            }
            mcp_span = Some((val_start, val_end));
            mcp_key_indent = line_indent(text, key_start).to_string();
        }
        sc.pos = val_end;
    }

    let value_text = |value: &serde_json::Value, indent: &str| -> Result<String> {
        let pretty = serde_json::to_string_pretty(value)
            .map_err(|e| Error::Config(format!("failed to serialize Claude Code config: {e}")))?;
        Ok(reindent_value(&pretty, indent))
    };
    let member_text = |key: &str, value: &serde_json::Value, indent: &str| -> Result<String> {
        Ok(format!("\"{key}\": {}", value_text(value, indent)?))
    };

    match mcp_span {
        None => {
            // "mcpServers" missing: insert it as the first member of the root object.
            let Some(root_indent) = root_member_indent else {
                return Ok(None); // empty root object: fall back to a full rewrite
            };
            let value = serde_json::json!({ "mindctx": mindctx_value });
            let entry = member_text("mcpServers", &value, &format!("{root_indent}  "))?;
            Ok(Some(format!(
                "{}\n{root_indent}{entry},{}",
                &text[..root_after_open],
                &text[root_after_open..]
            )))
        }
        Some((vs, ve)) => {
            // Locate the "mindctx" member inside the mcpServers object.
            let mut inner = Scanner::new(text);
            inner.pos = vs;
            inner.skip_ws();
            if inner.eat(b'{').is_none() {
                return Err(malformed("mcpServers"));
            }
            let inner_after_open = inner.pos;
            let mut mindctx: Option<(usize, usize, usize)> = None;
            let mut has_members = false;
            loop {
                inner.skip_ws();
                match inner.peek() {
                    Some(b'}') => break,
                    Some(b',') => {
                        inner.bump();
                        continue;
                    }
                    Some(b'"') => {}
                    _ => return Ok(None),
                }
                has_members = true;
                let key_start = inner.pos;
                let (_, key_end) = inner.scan_string().ok_or_else(|| {
                    Error::Config("malformed Claude Code config (unterminated string)".into())
                })?;
                let key = &text[key_start + 1..key_end - 1];
                inner.skip_ws();
                if inner.eat(b':').is_none() {
                    return Ok(None);
                }
                inner.skip_ws();
                let val_start = inner.pos;
                let val_end = inner
                    .scan_value()
                    .ok_or_else(|| Error::Config("malformed Claude Code config".into()))?;
                if key == "mindctx" {
                    if mindctx.is_some() {
                        // Duplicate "mindctx" members inside "mcpServers": refuse to guess
                        // which one the host's parser keeps.
                        return Ok(None);
                    }
                    mindctx = Some((key_start, val_start, val_end));
                }
                inner.pos = val_end;
            }

            match mindctx {
                Some((key_start, val_start, val_end)) => {
                    if !text[val_start..val_end].trim_start().starts_with('{') {
                        return Err(malformed("mcpServers.mindctx"));
                    }
                    // The span starts after `"mindctx": ` — replace the value only.
                    let indent = line_indent(text, key_start);
                    let replacement = value_text(mindctx_value, indent)?;
                    Ok(Some(format!(
                        "{}{replacement}{}",
                        &text[..val_start],
                        &text[val_end..]
                    )))
                }
                None => {
                    let indent = format!("{mcp_key_indent}  ");
                    let entry = member_text("mindctx", mindctx_value, &indent)?;
                    if has_members {
                        Ok(Some(format!(
                            "{}\n{indent}{entry},{}",
                            &text[..inner_after_open],
                            &text[inner_after_open..]
                        )))
                    } else {
                        // Empty mcpServers object: give it a pretty body.
                        Ok(Some(format!(
                            "{}{{\n{indent}{entry}\n{mcp_key_indent}}}{}",
                            &text[..vs],
                            &text[ve..]
                        )))
                    }
                }
            }
        }
    }
}

/// Whitespace between the start of the line containing `pos` and `pos` itself, or "" when the
/// position is not at a line start (compact/inline JSON).
fn line_indent(text: &str, pos: usize) -> &str {
    let line_start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let indent = &text[line_start..pos];
    if indent.chars().all(|c| c == ' ' || c == '\t') {
        indent
    } else {
        ""
    }
}

/// Re-indent a `to_string_pretty` value so its nested lines sit under `member_indent`
/// (first line stays inline after the member key).
fn reindent_value(pretty: &str, member_indent: &str) -> String {
    let mut out = String::new();
    for (i, line) in pretty.lines().enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(member_indent);
            let trimmed = line.trim_start();
            out.push_str(&"  ".repeat((line.len() - trimmed.len()) / 2));
            out.push_str(trimmed);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Minimal read-only JSON text scanner used for surgical edits. Byte-offset based; slicing only
/// happens at ASCII delimiter boundaries, so multi-byte content is safe.
struct Scanner<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn eat(&mut self, expected: u8) -> Option<()> {
        if self.peek() == Some(expected) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// Scan a string literal with the cursor at the opening quote.
    /// Returns (start_including_quote, end_excluding_closing_quote).
    fn scan_string(&mut self) -> Option<(usize, usize)> {
        let start = self.pos;
        self.eat(b'"')?;
        loop {
            match self.bump()? {
                b'"' => return Some((start, self.pos)),
                b'\\' => {
                    self.bump()?;
                }
                _ => {}
            }
        }
    }

    /// Scan a JSON value from the current position; returns the end offset (exclusive).
    fn scan_value(&mut self) -> Option<usize> {
        match self.peek()? {
            b'"' => {
                let (_, end) = self.scan_string()?;
                Some(end)
            }
            open @ (b'{' | b'[') => {
                let _ = open;
                let mut depth = 0usize;
                loop {
                    match self.peek()? {
                        b'"' => {
                            let (_, end) = self.scan_string()?;
                            self.pos = end;
                        }
                        b'{' | b'[' => {
                            self.bump();
                            depth += 1;
                        }
                        b'}' | b']' => {
                            self.bump();
                            depth -= 1;
                            if depth == 0 {
                                return Some(self.pos);
                            }
                        }
                        _ => {
                            self.bump();
                        }
                    }
                }
            }
            _ => {
                // Number / true / false / null: until a delimiter.
                loop {
                    match self.peek() {
                        Some(b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') | None => {
                            return Some(self.pos);
                        }
                        Some(_) => {
                            self.pos += 1;
                        }
                    }
                }
            }
        }
    }
}

/// Locate the first `key` member inside the JSON object whose `{` sits at `object_start`
/// (pass 0 for the document root). Returns (key_start, value_start, value_end); `None` when
/// the key is absent, the structure is not a plain object, or the key occurs more than once —
/// with a duplicate, a text scanner cannot tell which member the host's parser keeps, so the
/// caller must not splice.
fn find_object_member(text: &str, object_start: usize, key: &str) -> Option<(usize, usize, usize)> {
    let mut sc = Scanner::new(text);
    sc.pos = object_start;
    sc.skip_ws();
    sc.eat(b'{')?;
    let mut found: Option<(usize, usize, usize)> = None;
    loop {
        sc.skip_ws();
        match sc.peek() {
            Some(b'}') => break,
            Some(b',') => {
                sc.bump();
            }
            Some(b'"') => {}
            _ => return None,
        }
        sc.skip_ws();
        let (key_start, key_end) = sc.scan_string()?;
        sc.skip_ws();
        sc.eat(b':')?;
        sc.skip_ws();
        let val_start = sc.pos;
        let val_end = sc.scan_value()?;
        if &text[key_start + 1..key_end - 1] == key {
            if found.is_some() {
                return None;
            }
            found = Some((key_start, val_start, val_end));
        }
        sc.pos = val_end;
    }
    found
}

/// Remove `mcpServers.mindctx` from Claude Code config bytes with a surgical splice of the raw
/// text: every other byte of the document (key order, indentation, unrelated members) is kept
/// exactly (the previous full-rewrite reformatted the whole file, churning the
/// user's formatting and collapsing duplicate top-level keys). Returns None when the structure
/// cannot be spliced safely — the caller then leaves the file untouched and warns.
pub(crate) fn strip_claude_mindctx(current: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(current).ok()?;
    let (_, mcp_start, _) = find_object_member(text, 0, "mcpServers")?;
    let (key_start, _, val_end) = find_object_member(text, mcp_start, "mindctx")?;

    // Splice the member together with one adjacent comma so the remaining members stay valid:
    // trailing comma -> drop the comma and the whitespace up to the next member or the closing
    // brace (which keeps its own indentation); last member -> drop the preceding comma together
    // with the newline+indent before the key; sole member -> drop only the member, leaving an
    // empty object.
    let mut after = Scanner::new(text);
    after.pos = val_end;
    after.skip_ws();
    let (rem_start, rem_end) = if after.peek() == Some(b',') {
        after.bump();
        after.skip_ws();
        (key_start, after.pos)
    } else {
        let bytes = text.as_bytes();
        let line_start = text[..key_start].rfind('\n').map_or(0, |i| i + 1);
        let mut i = line_start;
        while i > 0 && matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r') {
            i -= 1;
        }
        if i > 0 && bytes[i - 1] == b',' {
            (i - 1, val_end)
        } else {
            (key_start, val_end)
        }
    };
    Some(format!("{}{}", &text[..rem_start], &text[rem_end..]).into_bytes())
}
