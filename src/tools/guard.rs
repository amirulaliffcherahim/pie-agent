//! The workspace ceiling, checked on the **resolved** path — in one place.
//!
//! Every tool that accepts a path from a model asks this module whether the path is inside
//! `tools.workspace`. It lives here rather than inside a tier because it was briefly about to
//! be written twice: the read-only tier resolved paths against the ceiling, and giving `bash`
//! a working-directory argument would have needed the identical check. A second copy of a
//! safety check is the defect `DESIGN.md` §20's "One spawn policy, not three" exists to
//! remove — the copies are free to drift, and the drift is invisible because each copy is
//! tested by its own tier's tests.
//!
//! # The check is `canonicalize`, then components
//!
//! Both halves are load-bearing, and each has its own witness in `tests/tools.rs`:
//!
//! * **`canonicalize` first.** Checking the *requested* text passes a symlink pointing out of
//!   the tree, because the text never mentions where it lands. Verified by
//!   `a_symlink_escaping_the_root_is_refused`, which goes red when the check is moved onto the
//!   unresolved path.
//! * **Then components, never a string prefix.** `C:\ws2` starts with the characters `C:\ws`,
//!   so a `starts_with` on the string admits a sibling directory. Verified by
//!   `a_sibling_whose_name_merely_starts_with_the_root_is_refused`, which goes red when the
//!   comparison becomes textual.
//!
//! `..` needs no special case: it is resolved by `canonicalize` like anything else, which is
//! why `sub/../top.txt` — inside the root, and a path a textual ban on `..` would wrongly
//! refuse — is allowed while `../escaped.txt` is refused.
//!
//! # This is a ceiling, not a sandbox
//!
//! It constrains which paths pie's tools will **name**. It does not constrain what a command
//! does once it runs: an allow-listed `git` started in the root can still write anywhere the
//! user can. `DESIGN.md` §20 states that boundary rather than implying a guarantee pie does
//! not have.

use std::fs;
use std::path::{Path, PathBuf};

/// Resolve `requested` against the ceiling and refuse anything that lands outside it.
///
/// The error is a sentence written for a **model** to read — what was refused, why, and what
/// to do instead — because the model is the reader. A refusal it will simply retry is a
/// refusal that costs a turn.
pub(crate) fn resolve_within(root: &Path, requested: &str) -> Result<PathBuf, String> {
    if requested.trim().is_empty() {
        return Err(
            "the path argument was empty. Give a path inside the workspace root; an empty string \
             resolves to no file, so retrying it reaches the same refusal."
                .to_string(),
        );
    }

    let as_path = Path::new(requested);
    // An absolute request is not joined — `Path::join` would silently discard the root for one
    // — it is resolved and then checked against the ceiling like any other.
    let candidate = if as_path.is_absolute() {
        as_path.to_path_buf()
    } else {
        root.join(as_path)
    };

    let resolved = fs::canonicalize(&candidate).map_err(|e| {
        format!(
            "`{requested}` does not resolve to an existing path under the workspace root `{}` \
             ({e}). Check the path, or use the `ls` tool to see what is there.",
            root.display()
        )
    })?;

    if !within(root, &resolved) {
        return Err(format!(
            "`{requested}` resolves to `{}`, which is outside the workspace root `{}`. The \
             `tools.workspace` root is a ceiling for every tool, not a default, so this call was \
             not run. Ask for a path inside the root instead; do not retry this one.",
            resolved.display(),
            root.display()
        ));
    }

    Ok(resolved)
}

/// Whether `path` is `root` or underneath it, compared by **components**.
///
/// String-prefix comparison is the bug this avoids: `C:\ws2` starts with `C:\ws`, and so does
/// a verbatim-prefixed spelling of the same path. Components are what the filesystem means.
/// Case-insensitive on Windows because the filesystem is, and because `canonicalize` may
/// return either case for the same directory.
pub(crate) fn within(root: &Path, path: &Path) -> bool {
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
///
/// Absolute paths in tool output would leak the user's directory layout into a transcript for
/// no benefit, and a backslash-separated Windows path is unreadable next to the `/`-separated
/// one the model wrote.
pub(crate) fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let text = rel.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".to_string()
    } else {
        text
    }
}
