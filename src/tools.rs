//! The tool suite the loop may offer a model (M11, `DESIGN.md` §20).
//!
//! M9 built the loop and shipped no tools, so a `tool_use` was answered with a stated
//! error. This module is where that answer becomes a real one — or, with no
//! configuration, stays exactly the answer it was, which is the point.
//!
//! # The constraint that decides the shape of everything here
//!
//! M9's loop runs between the model and pie with **no interactive channel**. `agy
//! --print` and `codex exec` are one-shot, claude's SDK mode streams, and the TUI is not
//! the only entry point. There is nowhere to ask "may I run this command?" mid-turn, and
//! a tool design that assumed an approval prompt would work in one front-end and silently
//! block in the others.
//!
//! So: **a tool is enabled in config with a declared scope, or it is not offered.** No
//! per-call confirmation, no "ask the user", no timeout-then-proceed. The decision is
//! made once, in `config.toml`, by a human who is not under time pressure — the same
//! shape as the rest of pie's safety model.
//!
//! # Two kinds of "no", which are different sentences
//!
//! A name that this build does not answer for at all is an *unknown tool*. A name it
//! answers for but that this configuration does not enable is a *disabled tool*, and the
//! refusal names the config key that would enable it. Both are **answered**: a loop that
//! ignored a call would leave the model waiting for a result that never comes, which is a
//! conversation that never finishes rather than a missing feature.
//!
//! The refusal is written for a **model** to read, because that is the reader. It says
//! what was not done, why, and what to do instead — so the model can proceed with what it
//! has rather than retry the same call.

pub mod bash;
pub(crate) mod guard;
pub mod read_only;

use crate::config::Tools;
use crate::tool_loop::{ToolCall, ToolResult, ToolRunner};

/// Routes a model's tool call to the tier that owns it.
///
/// One [`ToolRunner`] for the whole suite rather than one per tier, so `tool_loop`'s
/// `run_loop` needed no change to gain tools — the loop routes, it does not interpret
/// (`RISKS.md` §8).
#[derive(Debug, Clone)]
pub struct ToolSet {
    tools: Tools,
}

impl ToolSet {
    /// The suite a resolved configuration enables.
    pub fn new(tools: Tools) -> Self {
        Self { tools }
    }

    /// The configuration this suite was built from.
    pub fn config(&self) -> &Tools {
        &self.tools
    }
}

/// Every tool name this build **answers for**, enabled or not.
///
/// Not `enabled_tools`: membership here means "the model gets a sentence naming the config
/// key", while a name outside it means "the model gets a sentence saying there is no such
/// tool". A build that quietly collapsed the two would make a typo indistinguishable from
/// a policy decision, and a model cannot act on that.
pub const ANSWERED_TOOLS: &[&str] = &["read", "ls", "grep", "find", "bash", "write", "edit"];

impl ToolRunner for ToolSet {
    fn run(&self, call: &ToolCall) -> ToolResult {
        match call.name.as_str() {
            "read" | "ls" | "grep" | "find" => read_only::run(&self.tools, call),
            "bash" => bash::run(&self.tools, call),
            // Declared by `DESIGN.md` §20's tier table and deliberately **not built by
            // M11**: `ACCEPTANCE.md` §19 freezes three stages and gives the mutating tier
            // no criterion. A mutating tool with no frozen criterion is exactly what this
            // project's discipline forbids, so the honest handling is a refusal that names
            // the key rather than a key that merely looks like it does something.
            name @ ("write" | "edit") => refused(
                call,
                format!(
                    "the `{name}` tool is not implemented in this build of pie. The \
                     configuration key `tools.{name}` is declared, and setting it does not \
                     enable anything: the mutating tier needs its own acceptance criteria \
                     (atomic writes, and the same resolved-path ceiling the read tier has) \
                     before it can be offered. Do not retry this call — continue with the \
                     tools you have, or state what you would have written."
                ),
            ),
            other => refused(
                call,
                format!(
                    "there is no `{other}` tool in pie. The tools this build answers for are \
                     {}. Do not retry this call; a retry reaches the same answer.",
                    ANSWERED_TOOLS.join(", ")
                ),
            ),
        }
    }
}

/// The one shape a refusal takes: an error result the model can read and act on.
///
/// Shared rather than written per tier so that "answered, never ignored" is one decision
/// in one place. The text differs; the shape does not.
pub(crate) fn refused(call: &ToolCall, why: String) -> ToolResult {
    ToolResult {
        id: call.id.clone(),
        output: why,
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "toolu_1".to_string(),
            name: name.to_string(),
            input: json!({}),
        }
    }

    #[test]
    fn an_unknown_tool_is_answered_rather_than_ignored() {
        let set = ToolSet::new(Tools::default());
        let result = set.run(&call("teleport"));
        assert!(result.is_error, "an unknown tool must not report success");
        assert_eq!(
            result.id, "toolu_1",
            "a result must answer the call it was given"
        );
        assert!(
            result.output.contains("no `teleport` tool"),
            "the refusal must name the tool that does not exist: {}",
            result.output
        );
        assert!(
            result.output.contains("read") && result.output.contains("bash"),
            "the refusal must say what does exist, so the model can proceed: {}",
            result.output
        );
    }

    #[test]
    fn a_declared_but_unbuilt_tier_names_its_config_key_and_discourages_a_retry() {
        // The property that keeps this from being a key that only *looks* like it works:
        // setting it changes nothing, and the answer says so.
        let tools = Tools {
            write: true,
            ..Tools::default()
        };
        let set = ToolSet::new(tools);
        let result = set.run(&call("write"));
        assert!(result.is_error);
        assert!(
            result.output.contains("tools.write"),
            "the refusal must name the key, even when that key is set: {}",
            result.output
        );
        assert!(
            result.output.contains("Do not retry"),
            "a refusal the model will retry is a refusal that costs a turn: {}",
            result.output
        );
    }

    #[test]
    fn every_answered_name_is_routed_somewhere() {
        let set = ToolSet::new(Tools::default());
        for name in ANSWERED_TOOLS {
            let result = set.run(&call(name));
            assert_eq!(result.id, "toolu_1", "`{name}` must be answered");
            assert!(
                !result.output.contains("there is no"),
                "`{name}` is in ANSWERED_TOOLS, so it must not be answered as unknown: {}",
                result.output
            );
        }
    }
}
