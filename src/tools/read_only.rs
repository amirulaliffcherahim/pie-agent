//! The read-only tier: `read`, `ls`, `grep`, `find` (`DESIGN.md` §20).
//!
//! Safe by construction in the sense that they only observe. The one thing there is to
//! get right is that they cannot escape the configured workspace root — and that check
//! has to be on the **resolved** path, because `..` and a symlink are the two ways a
//! textual prefix check is wrong.
//!
//! # Ownership
//!
//! **Stage A of M11 owns this file** (`ACCEPTANCE.md` §19, stage A; gate
//! `cargo test --test tools`), together with `tests/tools.rs`. The signature below is
//! frozen by M11's contract; the body is not.
//!
//! # The argument shape, chosen here because the contract did not fix one
//!
//! `ToolCall.input` arrives opaque, so the shape has to be the one a coding model
//! actually sends rather than an invention. Claude's own tools are the reference and
//! they use `{"file_path": …}` to read one file, `{"path": …}` to list a directory, and
//! `{"pattern": …, "path": …}` to search. So:
//!
//! | Tool | Required | Optional | Aliases accepted |
//! |---|---|---|---|
//! | `read` | `file_path` | — | `path` |
//! | `ls` | — | `path` | `dir_path` |
//! | `grep` | `pattern` | `path` | `query` for the pattern; `dir_path` for the path |
//! | `find` | `pattern` | `path` | `glob`, `query` for the pattern |
//!
//! The optional `path` defaults to the workspace root. An argument that is missing or
//! the wrong type is a **stated refusal naming the argument**, because a model that is
//! told which field was wrong can fix it, while a generic error makes it guess.
//!
//! # Two bounds, and they are the reason nothing here accumulates
//!
//! `tools.max_output_bytes` caps every call's output. A 10 MB file is read through a
//! `take`, not into memory and then trimmed; a directory with 100k entries stops
//! collecting once the budget is spent. `B22`'s lesson applied to a new input.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Tools;
use crate::tool_loop::{ToolCall, ToolResult};

/// Run one call from the read-only tier.
///
/// The `name` is one of `read`, `ls`, `grep`, `find` — [`crate::tools::ToolSet`] is what
/// routes on it, and this function is never called with anything else.
pub fn run(cfg: &Tools, call: &ToolCall) -> ToolResult {
    // A workspace is the single enabling decision: without one there is nothing to scope
    // a read to. The message names the key rather than saying "not configured", because
    // the only actionable form of that sentence says what to set.
    let Some(configured) = cfg.workspace.as_deref() else {
        return crate::tools::refused(
            call,
            format!(
                "the `{}` tool is not enabled. pie offers no tools until a workspace root is \
                 configured: set `tools.workspace` in config.toml to the directory the tools \
                 may observe. Until then, continue without tools.",
                call.name
            ),
        );
    };

    if !cfg.read {
        return crate::tools::refused(
            call,
            format!(
                "the `{}` tool is not enabled: `tools.read` is false in this configuration. The \
                 read-only tier (`read`, `ls`, `grep`, `find`) is off, so continue without tools.",
                call.name
            ),
        );
    }

    // Resolved once, here, so every tool below compares against one canonical ceiling
    // rather than four copies of the same idea. Canonicalising the root also turns the
    // Windows verbatim prefix into a form the component comparison below can trust.
    let root = match fs::canonicalize(configured) {
        Ok(root) => root,
        Err(e) => {
            return crate::tools::refused(
                call,
                format!(
                    "the configured `tools.workspace` root `{}` cannot be resolved ({e}). Fix the \
                     path in config.toml and run the call again.",
                    configured.display()
                ),
            )
        }
    };

    let outcome = match call.name.as_str() {
        "read" => read(cfg, &root, call),
        "ls" => ls(cfg, &root, call),
        "grep" => grep(cfg, &root, call),
        "find" => find(cfg, &root, call),
        other => Err(format!(
            "the read-only tier does not answer for `{other}`. This is a routing bug in pie rather \
             than something you can fix in the call; continue without it."
        )),
    };

    match outcome {
        Ok(output) => ToolResult {
            id: call.id.clone(),
            output,
            is_error: false,
        },
        Err(why) => crate::tools::refused(call, why),
    }
}

/// `read` — one file's bytes, bounded by the output cap.
fn read(cfg: &Tools, root: &Path, call: &ToolCall) -> Result<String, String> {
    let requested = required_str(&call.input, "file_path", &["path"])?;
    let path = resolve_within(root, &requested)?;

    let meta = fs::metadata(&path).map_err(|e| {
        format!(
            "`{requested}` resolves to `{}` but cannot be inspected ({e}). Check that it exists and \
             is readable, or ask for a different path.",
            path.display()
        )
    })?;

    if meta.is_dir() {
        return Err(format!(
            "`{}` is a directory, and `read` only reads one file. Use the `ls` tool to list it, or \
             name a file inside it.",
            relative(root, &path)
        ));
    }

    let budget = cfg.max_output_bytes;
    let mut raw: Vec<u8> = Vec::new();
    File::open(&path)
        .map_err(|e| format!("`{}` could not be opened ({e}).", path.display()))?
        .take(budget as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("`{}` could not be read ({e}).", path.display()))?;

    if raw.len() > budget {
        return Ok(capped(
            &raw[..budget],
            budget,
            &format!(
                "[output capped at {budget} bytes by `tools.max_output_bytes`; read a smaller file \
                 or raise the cap]"
            ),
        ));
    }

    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// `ls` — one directory's entries, sorted so the listing is stable across calls.
fn ls(cfg: &Tools, root: &Path, call: &ToolCall) -> Result<String, String> {
    let requested = optional_str(&call.input, "path", &["dir_path"])?.unwrap_or_else(|| ".".into());
    let path = resolve_within(root, &requested)?;

    let meta = fs::metadata(&path).map_err(|e| {
        format!(
            "`{requested}` cannot be inspected ({e}).",
            requested = requested
        )
    })?;
    if !meta.is_dir() {
        return Err(format!(
            "`{}` is a file, not a directory, so there is nothing to list. Use the `read` tool on it \
             instead.",
            relative(root, &path)
        ));
    }

    let budget = cfg.max_output_bytes;
    let mut names: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    let entries = fs::read_dir(&path)
        .map_err(|e| format!("`{}` could not be listed ({e}).", path.display()))?;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => return Err(format!("a directory entry could not be read ({e}).")),
        };
        let mut line = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            line.push('/');
        }
        used += line.len() + 1;
        names.push(line);
        if used > budget {
            truncated = true;
            break;
        }
    }

    // Sorted here rather than trusted from the filesystem: `read_dir` order is arbitrary,
    // and "a directory listing is stable" is an acceptance criterion, not a hope.
    names.sort();

    let out = joined(
        &names,
        budget,
        truncated,
        "listing capped at ",
        " bytes by `tools.max_output_bytes`; list a subdirectory to see the rest",
    );
    if out.is_empty() {
        return Ok(format!("`{}` contains no entries.", relative(root, &path)));
    }
    Ok(out)
}

/// `grep` — every line containing `pattern`, reported as `path:line:text`.
///
/// The path is relative to the workspace root, which is what makes the report usable:
/// the model can feed it straight back to `read`.
fn grep(cfg: &Tools, root: &Path, call: &ToolCall) -> Result<String, String> {
    let pattern = required_str(&call.input, "pattern", &["query"])?;
    let requested = optional_str(&call.input, "path", &["dir_path"])?.unwrap_or_else(|| ".".into());
    let path = resolve_within(root, &requested)?;

    if path.is_file() {
        return Ok(grep_finish(
            grep_file(root, &path, &pattern, cfg.max_output_bytes)?,
            &pattern,
            &requested,
        ));
    }
    if !path.is_dir() {
        return Err(format!(
            "`{requested}` is neither a file nor a directory, so there is nothing to search."
        ));
    }

    // The cross-file cap reserves room for its own note before it starts, so the total the
    // call returns is still inside `tools.max_output_bytes`. A note appended after the fact
    // would be the one thing that broke the bound it describes.
    let note = format!(
        "[matches capped at {} bytes by `tools.max_output_bytes`]\n",
        cfg.max_output_bytes
    );
    let room = cfg.max_output_bytes.saturating_sub(note.len());
    let mut out = String::new();
    let mut truncated = false;
    for file in walk(root, &path) {
        let remaining = room.saturating_sub(out.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        if let Some(chunk) = grep_file(root, &file, &pattern, remaining)? {
            out.push_str(&chunk);
            if out.len() >= room {
                truncated = true;
                break;
            }
        }
    }

    if truncated {
        out = fit(out, room);
        out.push_str(&note);
    }
    Ok(grep_finish(
        if out.is_empty() { None } else { Some(out) },
        &pattern,
        &requested,
    ))
}

/// `find` — paths whose name or relative path matches a glob, sorted.
fn find(cfg: &Tools, root: &Path, call: &ToolCall) -> Result<String, String> {
    let pattern = required_str(&call.input, "pattern", &["glob", "query"])?;
    let requested = optional_str(&call.input, "path", &["dir_path"])?.unwrap_or_else(|| ".".into());
    let path = resolve_within(root, &requested)?;

    let budget = cfg.max_output_bytes;
    let single: Vec<PathBuf> = if path.is_file() {
        vec![path]
    } else if path.is_dir() {
        walk(root, &path).into_iter().collect()
    } else {
        return Err(format!("`{requested}` is neither a file nor a directory."));
    };

    let mut hits: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for file in single {
        let rel = relative(root, &file);
        if !matches_pattern(&pattern, &rel) {
            continue;
        }
        used += rel.len() + 1;
        hits.push(rel);
        if used > budget {
            truncated = true;
            break;
        }
    }
    hits.sort();

    if hits.is_empty() && !truncated {
        return Ok(format!(
            "no path under `{requested}` matches `{pattern}`. Try a wider pattern such as `*{pattern}*` \
             or search from the workspace root."
        ));
    }

    let out = joined(
        &hits,
        budget,
        truncated,
        "listing capped at ",
        " bytes by `tools.max_output_bytes`; narrow the pattern",
    );
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Path resolution: the ceiling, checked on the resolved path.
// ---------------------------------------------------------------------------------------

/// Resolve `requested` against the ceiling and refuse anything that lands outside it.
///
/// The check is on `std::fs::canonicalize`'s output, never on the text: a textual prefix
/// check passes `../secret` and passes a symlink pointing out of the tree, which are
/// exactly the two cases `ACCEPTANCE.md` §19 names.
fn resolve_within(root: &Path, requested: &str) -> Result<PathBuf, String> {
    if requested.trim().is_empty() {
        return Err(
            "the path argument was empty. Give a path inside the workspace root; an empty string \
             resolves to no file, so retrying it reaches the same refusal."
                .to_string(),
        );
    }

    let as_path = Path::new(requested);
    // An absolute request is not joined — `Path::join` would silently discard the root for
    // one — it is resolved and then checked against the ceiling like any other.
    let candidate = if as_path.is_absolute() {
        as_path.to_path_buf()
    } else {
        root.join(as_path)
    };

    let resolved = fs::canonicalize(&candidate).map_err(|e| {
        format!(
            "`{requested}` does not resolve to an existing path under the workspace root `{}` ({e}). \
             Check the path, or use the `ls` tool to see what is there.",
            root.display()
        )
    })?;

    if !within(root, &resolved) {
        return Err(format!(
            "`{requested}` resolves to `{}`, which is outside the workspace root `{}`. The \
             `tools.workspace` root is a ceiling for every tool, not a default, so this call was not \
             run. Ask for a path inside the root instead; do not retry this one.",
            resolved.display(),
            root.display()
        ));
    }

    Ok(resolved)
}

/// Whether `path` is `root` or underneath it, compared by **components**.
///
/// String-prefix comparison is the bug this avoids: `C:\ws2` starts with `C:\ws`, and so
/// does a verbatim-prefixed spelling of the same path. Components are what the filesystem
/// means. Case-insensitive on Windows because the filesystem is, and because
/// `canonicalize` may return either case for the same directory.
fn within(root: &Path, path: &Path) -> bool {
    let mut ceiling = root.components();
    let mut candidate = path.components();
    loop {
        match (ceiling.next(), candidate.next()) {
            (None, _) => return true,
            (Some(a), Some(b)) if same_component(a.as_os_str(), b.as_os_str()) => {}
            _ => return false,
        }
    }
}

#[cfg(windows)]
fn same_component(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

#[cfg(not(windows))]
fn same_component(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    a == b
}

/// The path as the model should see it: relative to the ceiling, `/`-separated.
fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let text = rel.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".to_string()
    } else {
        text
    }
}

// ---------------------------------------------------------------------------------------
// Walking, reading, and the output budget.
// ---------------------------------------------------------------------------------------

/// Every file at or under `start`, skipping anything that resolves outside the ceiling.
///
/// Canonicalising each entry is what makes an in-tree symlink to an out-of-tree directory
/// a **skip** rather than an escape, and the visited set is what stops a symlink back to
/// an ancestor from recursing forever.
fn walk(root: &Path, start: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<PathBuf> = vec![start.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(canonical) = fs::canonicalize(&dir) else {
            continue;
        };
        if !within(root, &canonical) || !visited.insert(canonical.clone()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&canonical) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(resolved) = fs::canonicalize(entry.path()) else {
                continue;
            };
            // The ceiling again, on the resolved path: a symlink inside the tree pointing
            // outside it is not part of the tree.
            if !within(root, &resolved) {
                continue;
            }
            if resolved.is_dir() {
                stack.push(resolved);
            } else if resolved.is_file() {
                files.push(resolved);
            }
        }
    }
    files
}

/// One file's matching lines, or `None` when it holds none.
fn grep_file(
    root: &Path,
    path: &Path,
    pattern: &str,
    budget: usize,
) -> Result<Option<String>, String> {
    let file =
        File::open(path).map_err(|e| format!("`{}` could not be read ({e}).", path.display()))?;
    let name = relative(root, path);
    let mut out = String::new();

    for (number, line) in BufReader::new(file).lines().enumerate() {
        // A line that is not UTF-8 means the file is not source text. Skipping the rest of
        // the file is the honest answer; reporting mojibake would be worse than nothing.
        let Ok(line) = line else {
            break;
        };
        if !line.contains(pattern) {
            continue;
        }
        let rendered = format!("{name}:{}:{line}\n", number + 1);
        if out.len() + rendered.len() > budget {
            // The cap is named in the output, because a silently truncated match list reads
            // as the complete one.
            let note = format!("[matches capped at {budget} bytes by `tools.max_output_bytes`]\n");
            let room = budget.saturating_sub(note.len().min(budget));
            out.truncate(floor_boundary(&out, room));
            out.push_str(&note);
            return Ok(Some(fit(out, budget)));
        }
        out.push_str(&rendered);
    }

    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// The sentence a grep returns when it found nothing, so the model is not left guessing
/// whether the tool ran.
fn grep_finish(found: Option<String>, pattern: &str, requested: &str) -> String {
    found.unwrap_or_else(|| {
        format!(
            "no line under `{requested}` contains `{pattern}`. Widen the pattern, or list the \
             directory with `ls` to confirm the path."
        )
    })
}

/// Read `budget` bytes of `raw` and append `note` inside the same budget.
fn capped(raw: &[u8], budget: usize, note: &str) -> String {
    let keep = budget.saturating_sub(note.len());
    let mut text = String::from_utf8_lossy(&raw[..raw.len().min(keep)]).into_owned();
    text.truncate(floor_boundary(&text, keep));
    text.push_str(note);
    fit(text, budget)
}

/// The budget-safe rendering of a list, with its truncation note inside the same budget.
///
/// The note is reserved for *before* the list is laid out, because a note that gets cut
/// off by the trim is a listing that looks complete.
fn joined(items: &[String], budget: usize, truncated: bool, prefix: &str, suffix: &str) -> String {
    if !truncated {
        return fit(items.join("\n"), budget);
    }
    let note = format!("[{prefix}{budget}{suffix}]");
    let room = budget.saturating_sub(note.len() + 1);
    let mut out = fit(items.join("\n"), room);
    out.push('\n');
    out.push_str(&note);
    fit(out, budget)
}

/// Trim `text` to at most `budget` bytes without splitting a character.
fn fit(mut text: String, budget: usize) -> String {
    let end = floor_boundary(&text, budget);
    if end < text.len() {
        text.truncate(end);
    }
    text
}

/// The largest index `<= budget` that is a character boundary of `text`.
fn floor_boundary(text: &str, budget: usize) -> usize {
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

// ---------------------------------------------------------------------------------------
// Arguments.
// ---------------------------------------------------------------------------------------

/// The first present key from `wanted` and its `aliases`, as a non-empty string.
fn required_str(input: &Value, wanted: &str, aliases: &[&str]) -> Result<String, String> {
    if let Some(found) = lookup_str(input, wanted, aliases)? {
        return Ok(found);
    }
    Err(format!(
        "this call is missing its `{wanted}` argument. Send it as a JSON object, for example \
         {{\"{wanted}\": \"…\"}}{}. A retry with the same arguments reaches the same answer.",
        if aliases.is_empty() {
            String::new()
        } else {
            format!(" (the alias `{}` is accepted too)", aliases.join("`/`"))
        }
    ))
}

/// The first present key from `wanted` and its `aliases`, or `None` when none is present.
fn optional_str(input: &Value, wanted: &str, aliases: &[&str]) -> Result<Option<String>, String> {
    lookup_str(input, wanted, aliases)
}

/// The shared half of the two above: type-check what is present, name what is wrong.
fn lookup_str(input: &Value, wanted: &str, aliases: &[&str]) -> Result<Option<String>, String> {
    if !input.is_object() {
        return Err(format!(
            "the arguments for this tool must be a JSON object, but they were {}. Send an object \
             with a `{wanted}` string field.",
            json_kind(input)
        ));
    }
    for key in std::iter::once(wanted).chain(aliases.iter().copied()) {
        let Some(value) = input.get(key) else {
            continue;
        };
        return match value {
            Value::String(text) if !text.trim().is_empty() => Ok(Some(text.clone())),
            Value::String(_) => Err(format!(
                "the `{key}` argument was an empty string. Give it a non-empty value; retrying the \
                 empty string reaches the same refusal."
            )),
            other => Err(format!(
                "the `{key}` argument must be a string, but it was {}. Re-send it as a string.",
                json_kind(other)
            )),
        };
    }
    Ok(None)
}

/// What to call a JSON value in a refusal, so the model learns the type it sent.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

// ---------------------------------------------------------------------------------------
// Pattern matching, with no dependency.
// ---------------------------------------------------------------------------------------

/// Whether `path` (relative, `/`-separated) matches `pattern`.
///
/// A pattern with no wildcard and no separator is a substring test on the file name,
/// which is what a model means by `find("budget")`. Everything else is a glob: `*` stays
/// inside one segment, `**` crosses separators, `?` is one character.
fn matches_pattern(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let has_wildcard = pattern.contains(['*', '?']);
    let name = path.rsplit('/').next().unwrap_or(path);

    if !has_wildcard && !pattern.contains('/') {
        return name.contains(&pattern);
    }
    if !pattern.contains('/') {
        return wildcard(&pattern, name);
    }
    segments_match(
        &pattern.split('/').collect::<Vec<_>>(),
        &path.split('/').collect::<Vec<_>>(),
    )
}

/// Glob segments, with `**` allowed to swallow any number of them.
fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            segments_match(&pattern[1..], path)
                || (!path.is_empty() && segments_match(pattern, &path[1..]))
        }
        (Some(segment), Some(name)) => {
            wildcard(segment, name) && segments_match(&pattern[1..], &path[1..])
        }
        (Some(_), None) => false,
        (None, Some(_)) => false,
    }
}

/// One glob segment against one path segment, `*` and `?` only.
///
/// Iterative with a single backtracking mark, the standard shape: recursive glob matching
/// on a pattern like `************x` is exponential, and a model chooses the pattern.
fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;

    while ti < t.len() {
        let matched = match p.get(pi) {
            Some('?') => true,
            Some(c) => *c == t[ti],
            None => false,
        };
        if matched {
            pi += 1;
            ti += 1;
        } else if p.get(pi) == Some(&'*') {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}
