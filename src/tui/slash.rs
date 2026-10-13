//! The `/` command palette's registry (`DESIGN.md` §21.3).
//!
//! **One const table, and everything reads it.** The opener, the prefix filter, the
//! generated menu, the answer a typo gets, and the tests that walk the rows all go
//! through [`SLASH_COMMANDS`]. That is the rule §21.3 inherits from §12's keymap, and
//! the reason is recorded: **B26 was a frozen table and its implementation
//! disagreeing**, and the only thing that ever caught it was reading every row against
//! the behaviour. A second list of command names — in the help text, in the typo
//! answer, in a `match` that re-implements a row — would be the same defect waiting.
//!
//! **Each row carries a handler, `fn(args: &str) -> Action`.** A row therefore
//! produces an [`Action`] the loop already knows how to execute, rather than a second
//! effect channel: `/quit` means [`Action::Quit`], `/model` with no argument means
//! [`Action::OpenPicker`] (M6's picker, unchanged), and a row that has pie answer it
//! in-process means [`Action::PaletteRun`], which the loop resolves through this same
//! table by name. That last step is what makes "the palette and a typed command reach
//! one implementation" a value two calls can be compared on rather than a claim.
//!
//! Nothing here reads a file, spawns anything or is `async`: the registry is data, and
//! `commands::tui::answer` is where a row's work happens.

use crate::tui::app::Action;

/// What a row's handler returns for one invocation's arguments.
///
/// A function pointer rather than a closure trait object, because every row's handler
/// is a plain function and a `const` table of them is then constructible: no
/// allocation per frame, and no `Box<dyn Fn>` in a type that is `Copy`.
pub type Handler = fn(args: &str) -> Action;

/// One row of the palette: what a user types, what it takes, what it is, and what it
/// does.
///
/// `argument` is a **hint and never a pattern**: §21.3's filter matches a prefix of
/// `name` alone, so `/[provider/model]` typed at the prompt matches no row rather than
/// matching `/model` through its own documentation. `tests/tui_palette.rs` asserts
/// that for every row, which is the assertion that fails if a later change teaches the
/// filter to look at this field.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    /// The word after the slash, without the slash.
    pub name: &'static str,
    /// The argument hint, printed beside the name. Displayed, never matched.
    pub argument: &'static str,
    /// One line, for the menu and for `/help`.
    pub description: &'static str,
    /// What this row means, as the [`Action`] the loop executes.
    pub handler: Handler,
}

/// The row M7's `/memory` renderer answers.
///
/// Named here rather than spelled inline at the one place that needs it
/// (`crate::tui::local_command`, which keeps [`crate::tui::LocalCommand::Memory`] as
/// the value `commands::memory::local_lines` takes), so the guard names a registry row
/// instead of a word nothing checks. `a_memory_row_exists_and_is_the_one_the_submit_path_keeps`
/// asserts the name is in the table.
pub const MEMORY: &str = "memory";

/// The row M12 stage D answers: `/tools`, in [`crate::tui::tools`].
///
/// Named here for the same reason [`MEMORY`] is: [`crate::tui::answer_local`] routes on
/// the row's own name, and a name matched against a word spelled in that function would
/// be a second copy of this table free to disagree with it.
pub const TOOLS: &str = "tools";

/// A row pie answers in-process: the loop resolves the settled invocation through this
/// table and runs it. See [`Action::PaletteRun`].
fn run(_args: &str) -> Action {
    Action::PaletteRun
}

/// `/quit` — the loop's own exit, not a sentence about exiting.
fn quit(_args: &str) -> Action {
    Action::Quit
}

/// `/tools` — the row `crate::tui::tools` answers, with no model turn and no vendor CLI.
///
/// The handler is [`Action::PaletteRun`] like the other in-process rows; what actually
/// answers it is [`crate::tui::tools::lines`], reached through
/// [`crate::tui::answer_local`], which routes on this row's name ([`TOOLS`]) rather than
/// on a word spelled in the dispatcher.
fn tools(_args: &str) -> Action {
    Action::PaletteRun
}

/// `/model` bare opens M6's picker; `/model provider/model` pins a target instead.
///
/// The one handler whose answer depends on its argument, and the reason the handler is
/// a function of `args` rather than a constant per row: a bare `/model` must not send a
/// turn to look at a model list, and M6's picker is the surface that already fills it.
fn picker_or_run(args: &str) -> Action {
    if args.trim().is_empty() {
        Action::OpenPicker
    } else {
        Action::PaletteRun
    }
}

/// Every command the palette offers, in menu order.
///
/// The order is the order a user reads: the things that change what the next turn
/// costs, then the things that show state, then the ways out.
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "model",
        argument: "[provider/model]",
        description: "choose the model",
        handler: picker_or_run,
    },
    SlashCommand {
        name: "effort",
        argument: "<level>",
        description: "set the reasoning effort",
        handler: run,
    },
    SlashCommand {
        name: "backend",
        argument: "<name>",
        description: "switch the backend",
        handler: run,
    },
    SlashCommand {
        name: "new",
        argument: "",
        description: "start a new session",
        handler: run,
    },
    SlashCommand {
        name: "resume",
        argument: "[id]",
        description: "resume an earlier session",
        handler: run,
    },
    SlashCommand {
        name: "compact",
        argument: "[instructions]",
        description: "compact the transcript",
        handler: run,
    },
    SlashCommand {
        name: "usage",
        argument: "[backend]",
        description: "show quota and token totals",
        handler: run,
    },
    SlashCommand {
        name: "doctor",
        argument: "",
        description: "check every backend CLI",
        handler: run,
    },
    SlashCommand {
        name: "profile",
        argument: "",
        description: "show the projected persona",
        handler: run,
    },
    SlashCommand {
        name: "status",
        argument: "",
        description: "what this session is doing",
        handler: run,
    },
    SlashCommand {
        name: TOOLS,
        argument: "[list|run]",
        description: "list or run one tool",
        handler: tools,
    },
    SlashCommand {
        name: "ocr",
        argument: "<file>",
        description: "read text out of an image",
        handler: run,
    },
    SlashCommand {
        name: "learn",
        argument: "",
        description: "apply what this session learned",
        handler: run,
    },
    SlashCommand {
        name: MEMORY,
        argument: "",
        description: "what pie learned, with provenance",
        handler: run,
    },
    SlashCommand {
        name: "schedule",
        argument: "[status|run]",
        description: "what is scheduled, or run it",
        handler: run,
    },
    SlashCommand {
        name: "watch",
        argument: "[all|drift|news]",
        description: "run the monthly watchtower",
        handler: run,
    },
    SlashCommand {
        name: "help",
        argument: "",
        description: "every command, from this table",
        handler: run,
    },
    SlashCommand {
        name: "hotkeys",
        argument: "",
        description: "every key, from the keymap",
        handler: run,
    },
    SlashCommand {
        name: "quit",
        argument: "",
        description: "leave the TUI",
        handler: quit,
    },
];

impl SlashCommand {
    /// The row as one line: what it is called, what it takes, and what it does.
    ///
    /// Built from the row's own fields by the row itself, so the menu, `/help` and the
    /// sentence a typo gets cannot describe a command differently from the way the
    /// table declares it.
    pub fn menu_line(&self) -> String {
        if self.argument.is_empty() {
            format!("/{} — {}", self.name, self.description)
        } else {
            format!("/{} {} — {}", self.name, self.argument, self.description)
        }
    }
}

/// The row with this exact name.
///
/// Exact, not prefixed: [`matching`] is the filter a user's keystrokes drive, and a
/// lookup that accepted a prefix would run `/w` as `/watch`.
pub fn find(name: &str) -> Option<&'static SlashCommand> {
    SLASH_COMMANDS.iter().find(|row| row.name == name)
}

/// The filter a prompt's text asks for, or `None` when the text is not a slash word.
///
/// `Some("")` is the menu itself (`/` on its own), which is the state §21.3 opens the
/// palette in. Only the **first word** is taken: `/model gpt` filters on `model`, so a
/// row does not disappear the moment its argument is typed.
pub fn filter_of(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('/')?;
    Some(rest.split_whitespace().next().unwrap_or_default())
}

/// The rows whose **name** starts with `filter`, in table order.
///
/// The argument hint is not consulted — see [`SlashCommand::argument`].
pub fn matching(filter: &str) -> Vec<&'static SlashCommand> {
    SLASH_COMMANDS
        .iter()
        .filter(|row| row.name.starts_with(filter))
        .collect()
}

/// The text after the command word, when it belongs to `name`.
///
/// `""` when the word the user typed is not this row's name: an abbreviation the
/// palette completed is a row the user meant, and the characters they typed for it are
/// not its argument.
pub fn arguments_of<'a>(text: &'a str, name: &str) -> &'a str {
    let rest = text.trim_start().strip_prefix('/').unwrap_or(text);
    match rest.split_once(char::is_whitespace) {
        Some((word, args)) if word == name => args.trim(),
        _ => "",
    }
}

/// The row and its arguments, from a typed invocation. `None` when no row has that name.
pub fn invoke(text: &str) -> Option<(&'static SlashCommand, &str)> {
    let filter = filter_of(text)?;
    let row = find(filter)?;
    Some((row, arguments_of(text, row.name)))
}

/// The invocation a row runs with, as it settled into the composer.
///
/// The one function that spells an invocation, used both by the palette when it settles
/// the highlighted row into the prompt and by the tests that drive every row.
pub fn settled(row: &SlashCommand, args: &str) -> String {
    if args.trim().is_empty() {
        format!("/{}", row.name)
    } else {
        format!("/{} {}", row.name, args.trim())
    }
}

/// The whole table as the lines `/help`, the menu and a typo's answer all print.
pub fn menu_lines() -> Vec<String> {
    SLASH_COMMANDS.iter().map(SlashCommand::menu_line).collect()
}
