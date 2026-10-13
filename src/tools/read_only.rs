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
//! frozen by M11's contract; the body is not. The body shipped here is the refusal the
//! suite gives when it is not configured, which is `ACCEPTANCE.md` §19's first check:
//! *"with an empty config, no tool runs at all"*. Stage A replaces it with the real tier
//! and keeps that behaviour for the unconfigured case.

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
    let why = if cfg.workspace.is_none() {
        format!(
            "the `{}` tool is not enabled. pie offers no tools until a workspace root is \
             configured: set `tools.workspace` in config.toml to the directory the tools \
             may observe. Until then, continue without tools.",
            call.name
        )
    } else {
        format!(
            "the `{}` tool is not enabled: `tools.read` is false in this configuration. The \
             read-only tier (`read`, `ls`, `grep`, `find`) is off, so continue without tools.",
            call.name
        )
    };

    crate::tools::refused(call, why)
}
