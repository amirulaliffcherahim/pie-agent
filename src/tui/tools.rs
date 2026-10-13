//! `/tools` — M12 stage D, and the reason the palette is worth building
//! (`DESIGN.md` §21.4).
//!
//! Two subcommands, and the second one is the point:
//!
//! * **`/tools list`** prints every tool name M11 answers for with its **effective**
//!   state, read from the resolved [`Tools`] value — never from a document, never from
//!   a list written here. Three sentences, and they are deliberately different:
//!   *enabled*, *disabled* (with the config key that would enable it), and
//!   *declared-but-unbuilt* — which is what `write` and `edit` are, because
//!   `ACCEPTANCE.md` §19 (`S18`) freezes three stages and gives the mutating tier no
//!   criterion at all. A key that quietly does nothing, reported as merely "disabled",
//!   would send a user to set a key that enables nothing: those are different sentences
//!   because they call for different actions.
//!
//! * **`/tools run <name> <json-args>`** invokes one tool **directly, with no model
//!   turn**. M11's tier is otherwise reachable end-to-end only through a backend with a
//!   working loop, and only the API-key backends have one — so on a machine with no key
//!   the tools are exercisable by the test suite alone, which is what M11's
//!   `offline-verified` verdict records. A human typing
//!   `/tools run read {"file_path":"Cargo.toml"}` is the missing interactive path, and it
//!   spends no quota and spawns no vendor CLI.
//!
//! # Two boundaries, both deliberate
//!
//! **The refusals are M11's, and this module renders them rather than reproducing
//! them.** Whether a workspace is configured, whether a tier is on, whether `write` is
//! built — each of those is already a sentence inside [`crate::tools::ToolSet`]'s tier,
//! written for a reader who has to act on it. So `run` builds a [`ToolCall`] and hands it
//! to the tier; it never inspects `tools.workspace` or `tools.bash` on the way in. A
//! second copy of a safety check is what M11 spent a whole stage deleting, and a copy
//! here would be free to disagree with the one the model reads.
//!
//! **Nothing here writes configuration, and nothing here can.** `/tools` reads the
//! resolved value and runs tools; the decision to offer a tool is made once, in
//! `config.toml`, by a human who is not under time pressure (`DESIGN.md` §20). This
//! module does not even see the path of `config.toml`: it takes `&Tools`, and its only
//! I/O goes through M11's tier. That is why `tests/tui_tools.rs` can assert
//! `config.toml` is byte-identical afterwards without mocking anything.
//!
//! # Truncation is stated, never silent — and there is exactly one cap in this path
//!
//! M11's tier caps a call's output at `tools.max_output_bytes` and puts its own note
//! **inside** the output (`[output capped at 4096 bytes by `tools.max_output_bytes`; …]`
//! and its two siblings). That note therefore arrives here as ordinary text, and this
//! module's job is to pass it through — which the gate asserts against a call that really
//! does exceed the cap.
//!
//! **This module adds no second cap, and that is deliberate.** A cut here would hide the
//! tier's note whenever the note sat beyond it, and `src/tools/bash.rs` records the shape
//! of that defect: *a truncation whose explanation was itself cut off reads as a complete
//! answer*. The tier's cap is bounded by configuration; the transcript is a scrollback;
//! and "keep the default view compact" (`DESIGN.md` §21.5) is the collapsed tool block's
//! job in the turn view rather than this row's, which the user typed deliberately.

use crate::config::Tools;
use crate::tool_loop::{ToolCall, ToolResult, ToolRunner};
use crate::tools::{ToolSet, ANSWERED_TOOLS};

/// The id a `/tools run` call carries.
///
/// The tier echoes it back; nothing matches on it here, because there is no vendor to
/// pair a call with. It is named rather than left empty so that a result cannot be
/// mistaken for one the loop asked for.
const RUN_ID: &str = "tools-run";

/// The lines `/tools` answers with, for the arguments the user typed.
///
/// Always non-empty (M7's rule): a slash command runs in-process, so no turn ever ends
/// the `Running` status a submission sets, and an answer with nothing in it would look
/// exactly like a hang.
///
/// `tools` is the **resolved** value — `Config::tools`, the same value the loop hands to
/// [`ToolSet::new`] — so what `list` prints and what `run` does cannot disagree.
pub fn lines(tools: &Tools, args: &str) -> Vec<String> {
    let (word, rest) = split(args);
    match word {
        // Bare `/tools` is `list`: the state is what a user is asking about, and a menu
        // of two subcommands would be an answer with no information in it.
        "" | "list" => list(tools),
        "run" => run(tools, rest),
        other => vec![
            format!(
                "`/tools {other}` is not a subcommand: `{other}` names nothing this row does. \
                 `/tools` takes `list` (the effective state of every tool) or \
                 `run <name> <json-args>` (one tool, with no model turn)."
            ),
            "nothing ran, and nothing was sent to a backend. Try `/tools` on its own for the \
             list, or `/tools run read {\"file_path\":\"README.md\"}`"
                .to_string(),
        ],
    }
}

// ------------------------------------------------------------------- list ----

/// Every answered tool, with the state the **resolved** `[tools]` table gives it.
///
/// The names come from [`ANSWERED_TOOLS`] — M11's own constant, which is what the tier
/// routes on — so a tool M11 adds appears here without an edit in this file, and a name
/// that is in neither the enabled nor the declared set is reported as *unclassified*
/// rather than dropped.
fn list(tools: &Tools) -> Vec<String> {
    let mut out = vec![
        "tools — this build answers for the names below; each line is the state the resolved \
         `[tools]` table gives it"
            .to_string(),
        match tools.workspace.as_deref() {
            Some(root) => format!("workspace  {}", root.display()),
            None => "workspace  not set — `tools.workspace` is unset, and no tool can run until \
                     it names a directory"
                .to_string(),
        },
    ];

    for name in ANSWERED_TOOLS {
        out.push(format!("{name:<6} {}", state_of(tools, name)));
    }

    out.push(
        "this list is read, never written: enabling a tool is decided once, in config.toml, by \
         a human (`/tools` does not change it)"
            .to_string(),
    );
    out
}

/// What the workspace key being unset means for every tier that needs a root.
///
/// One sentence rather than four, because it is one decision: without a root there is
/// nothing to scope a tool to, which is M11's own reading of it (`ACCEPTANCE.md` §19: *a
/// workspace is the single enabling decision*).
const NO_WORKSPACE: &str = "disabled — no workspace root is configured; set `tools.workspace` to \
                           the directory the tools may observe";

/// One tool's effective state, as the sentence `list` prints beside its name.
///
/// Three readings of the same table, kept in one function so that a name cannot be
/// classified in two places: what the workspace key gates for the whole suite, what the
/// tier's own key gates, and the tier that M11 declares and does not build.
fn state_of(tools: &Tools, name: &str) -> String {
    // The mutating tier first, because it is the case that must never read as
    // "disabled": setting `tools.write` enables nothing, and a user who believes
    // otherwise is being told to do something that will not work.
    if matches!(name, "write" | "edit") {
        return format!(
            "declared, not built — `tools.{name}` exists in config.toml and setting it \
             enables nothing: M11 ships no mutating tier (`ACCEPTANCE.md` §19, S18)"
        );
    }

    // The order of the keys below **mirrors the order its own tier checks them**
    // (`src/tools/read_only.rs` consults the workspace before `tools.read`;
    // `src/tools/bash.rs` consults `tools.bash` first). That is not cosmetic: the key this
    // line names is the key whose change actually moves the tool, so `/tools list` and the
    // refusal a call returns name the same one. `tests/tui_tools.rs` asserts that equality
    // against the tier's own refusal rather than against a substring chosen here.
    match name {
        "read" | "ls" | "grep" | "find" => {
            if tools.workspace.is_none() {
                return NO_WORKSPACE.to_string();
            }
            if tools.read {
                "enabled — `tools.read`, scoped to the workspace above".to_string()
            } else {
                "disabled — `tools.read` is false".to_string()
            }
        }
        "bash" => {
            if !tools.bash {
                "disabled — `tools.bash` is false, so nothing is executable".to_string()
            } else if tools.workspace.is_none() {
                NO_WORKSPACE.to_string()
            } else if tools.bash_allow.is_empty() {
                "disabled — `tools.bash` is true but `tools.bash_allow` is empty, so no \
                 command may run"
                    .to_string()
            } else {
                format!(
                    "enabled — `tools.bash`, scoped to {} allowed executable(s)",
                    tools.bash_allow.len()
                )
            }
        }
        // Unreachable while `ANSWERED_TOOLS` and this function agree. Stated rather than
        // left to a wildcard that would print a tool as if it were fine: a name M11 starts
        // answering for is a name whose state this row does not know, and saying so is
        // how the drift gets noticed.
        other => format!(
            "unclassified — this build answers for `{other}` and `/tools` does not know what \
             gates it; treat the state as unknown rather than enabled"
        ),
    }
}

// -------------------------------------------------------------------- run ----

/// One tool, invoked directly — no model turn, no vendor CLI, no session row.
fn run(tools: &Tools, rest: &str) -> Vec<String> {
    let (name, json) = split(rest);

    // A malformed or missing argument is a stated refusal **naming the argument**, and
    // it stops here: nothing is spawned and no turn starts. M11's tier is not consulted,
    // because a call with no name or no arguments is not a call it can answer.
    if name.is_empty() {
        return vec![
            "`/tools run` needs a tool name: the argument `name` is missing".to_string(),
            format!(
                "usage: `/tools run <name> <json-args>` — the names are the ones `/tools list` \
                 prints ({})",
                ANSWERED_TOOLS.join(", ")
            ),
        ];
    }
    if json.is_empty() {
        return vec![
            format!(
                "`/tools run {name}` needs arguments: the argument `args` is missing. It takes \
                 the tool's JSON input, as one value."
            ),
            "nothing ran. For example: `/tools run read {\"file_path\":\"README.md\"}`".to_string(),
        ];
    }

    let input: serde_json::Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(error) => {
            return vec![
                format!(
                    "`/tools run {name}` was not run: the argument `args` is not JSON — {error}"
                ),
                format!(
                    "the value as read: {json} — a tool's input is one JSON value, so \
                     `{{\"file_path\":\"README.md\"}}` rather than `file_path=README.md`"
                ),
            ]
        }
    };

    let call = ToolCall {
        id: RUN_ID.to_string(),
        name: name.to_string(),
        input,
    };

    // The tier decides everything else: whether the name exists, whether it is enabled,
    // whether a workspace is configured, whether the tier is built at all. This function
    // renders the answer; it is not a second opinion about it.
    render(name, &ToolSet::new(tools.clone()).run(&call))
}

/// What one result becomes on screen: the tool, its state, its output, and its
/// truncation if it has any.
///
/// The header names the tool and whether it **errored**, because "the output is long" and
/// "the output is a refusal" are the same three lines to a reader, and the tier's
/// refusals are prose.
fn render(name: &str, result: &ToolResult) -> Vec<String> {
    let mut out = vec![format!(
        "`{name}` — {}",
        if result.is_error { "error" } else { "ok" }
    )];

    if result.output.is_empty() {
        out.push("the tool returned no output".to_string());
        return out;
    }

    // Verbatim, including the tier's own truncation note when it capped the call: the
    // sentence that says the output is incomplete is inside the text this line renders, so
    // it cannot be lost by rendering it differently.
    out.extend(result.output.lines().map(str::to_string));
    out
}

// ----------------------------------------------------------------- helpers ----

/// The first word of `text` and everything after it, trimmed.
fn split(text: &str) -> (&str, &str) {
    let trimmed = text.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (trimmed, ""),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn every_answered_name_is_classified_and_the_mutating_tier_is_never_disabled() {
        let tools = Tools {
            workspace: Some("/tmp".into()),
            ..Tools::default()
        };
        for name in ANSWERED_TOOLS {
            let state = state_of(&tools, name);
            assert!(!state.starts_with("unclassified"), "`{name}`: {state}");
            if matches!(*name, "write" | "edit") {
                assert!(state.starts_with("declared"), "`{name}`: {state}");
                assert!(
                    !state.contains("disabled"),
                    "a key that enables nothing must not read as merely off: {state}"
                );
            }
        }
    }

    #[test]
    fn a_run_renders_the_tier_answer_rather_than_its_own() {
        // No workspace: the refusal is M11's, and it reaches the screen unchanged. This is
        // the assertion that fails if this module starts writing its own copy of the check.
        let tools = Tools::default();
        let call = ToolCall {
            id: RUN_ID.to_string(),
            name: "read".to_string(),
            input: json!({"file_path": "README.md"}),
        };
        let tier = ToolSet::new(tools.clone()).run(&call);
        let rendered = lines(&tools, "run read {\"file_path\":\"README.md\"}").join("\n");
        assert!(rendered.contains("`read` — error"), "{rendered}");
        assert!(
            rendered.contains(&tier.output),
            "the tier's own sentence must be the one rendered:\n{rendered}\n---\n{}",
            tier.output
        );
    }
}
