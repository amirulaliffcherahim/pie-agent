//! `bash`, the executing tier (`DESIGN.md` §20).
//!
//! Off by default, and scoped when on. Four properties, each of which is a check in
//! `ACCEPTANCE.md` §19:
//!
//! 1. `argv[0]` is checked against `tools.bash_allow` **before the spawn**, so a refused
//!    command is a stated error rather than a `PermissionDenied` the model has to
//!    interpret.
//! 2. A command that outlives its deadline is **killed**, not waited on (`RISKS.md` §6).
//! 3. A command that prints forever is bounded rather than accumulated — the reason
//!    `read_body` is capped, and `B22`'s lesson applied to a new input.
//! 4. The workspace root is a **ceiling**, checked on the resolved path, and the command's
//!    working directory is inside it. It is not a sandbox and does not claim to be: an
//!    allow-listed `git` can still write anywhere the user can. What is enforced is which
//!    **executables** may run, not what a shell line means.
//!
//! # Ownership
//!
//! **Stage B of M11 owns this file** (`ACCEPTANCE.md` §19, stage B; gate
//! `cargo test --test tools_bash`), together with `tests/tools_bash.rs`. Stage B depends
//! on stage C, because `bash` must use the **one** spawn path rather than become a third
//! copy of the policy — see `process.rs`. The body shipped here is the refusal the suite
//! gives when `bash` is not enabled, which is the default.

use crate::config::Tools;
use crate::tool_loop::{ToolCall, ToolResult};

/// Run one `bash` call.
pub fn run(cfg: &Tools, call: &ToolCall) -> ToolResult {
    let why = if !cfg.bash {
        "the `bash` tool is not enabled. `bash` runs arbitrary commands, so it is opt-in \
         and scoped: set `tools.bash = true` and list the executables it may run in \
         `tools.bash_allow`. Until then, continue without it."
            .to_string()
    } else if cfg.bash_allow.is_empty() {
        // Enabling `bash` with no allow-list is not "all commands allowed" — an empty list
        // is the honest reading of "no command is allowed", and saying so is the
        // difference between a refusal and a hole.
        "the `bash` tool is enabled but `tools.bash_allow` is empty, so no command may \
         run. Add the executables it may run (`bash_allow = [\"cargo\", \"git\"]`) — the \
         allow-list names executables, not command strings. Until then, continue without it."
            .to_string()
    } else {
        "the `bash` tool is not implemented in this build of pie. Continue without it.".to_string()
    };

    crate::tools::refused(call, why)
}
