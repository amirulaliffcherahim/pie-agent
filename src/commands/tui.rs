//! `pie tui` — the interactive session (`DESIGN.md` §12, §21).
//!
//! Opt-in, and the only command that takes over the terminal: a bare `pie` keeps
//! printing help, so CI and every existing test are unaffected, and nothing that
//! runs today can enter the TUI by accident.
//!
//! What this file does is small on purpose. It resolves the backend and the model
//! with exactly the two functions `pie -p` uses (`provider::resolve_target`,
//! `provider::by_id`), so a misspelt `--model` or a bad `general.default_backend`
//! fails with the same message and the same exit code as the one-shot path — and
//! fails *before* the terminal is put into raw mode, so the error is readable on a
//! normal screen. It then mints the session, builds [`App`] and hands both to
//! `tui::run_session`.
//!
//! The persona is **not** loaded here. `tui::run_session` reads
//! `Profile::from_home()` per turn and builds the request through
//! `tui::build_request`, which takes the same projection, through the same
//! accessor, that `commands/print.rs` hands to every backend — M3's Done condition
//! is that agy and codex receive byte-identical personas, and a TUI that assembled
//! its own would falsify it silently.
//!
//! The second half of this file is the `/` palette's answers (M12 stage A), and it
//! follows the same rule: a row reaches the function the subcommand reaches, or it
//! says which function that is and why this stage does not call it.

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::paths;
use crate::provider;
use crate::session_log;
use crate::tui::app::KEYBINDINGS;
use crate::tui::{self, slash, App, LocalCommand, Status};

/// `pie tui`
pub async fn run(cfg: &Config) -> Result<()> {
    // Which backend and model, resolved identically to `pie -p` so the TUI opens on
    // the conversation the user's configuration already selects. `resolve_target`
    // does not consult `--continue`/`--session`: a TUI run is its own conversation,
    // deliberately, because auto-resuming would silently answer in the wrong
    // context (`DESIGN.md` §11).
    let (backend, model) = provider::resolve_target(cfg, None)?;

    // The same check `pie -p` makes, for the same reason and before the same
    // irreversible step: an unknown backend must be reported on a normal terminal.
    // The provider returned here is dropped because `tui::run_turn` resolves one per
    // turn from `App::backend` — the picker can change the backend mid-session — so
    // the honest way to read this call is "is this backend one we can drive?".
    if provider::by_id(&backend).is_none() {
        return Err(PieError::UnknownBackend {
            backend: backend.clone(),
            known: super::models::known_backends(),
        });
    }

    // One session per TUI run, minted before the loop starts so the id the status
    // line shows exists from the first frame. Minting writes nothing: the session
    // appears in `pie sessions` only once a turn has been recorded against it
    // (`DESIGN.md` §11).
    let session = tui::begin_session(&backend);
    let mut app = App::new(backend, model, session.id());

    tui::run_session(&mut app, cfg).await
}

// ------------------------------------------------------- the palette --------
//
// One function per row would be a second list of command names beside
// `slash::SLASH_COMMANDS` (the B26 shape, `DESIGN.md` §21.3). So the dispatch is a
// single `match` here, and `tests/tui_palette.rs` walks the registry asserting that
// every row reaches an arm of it: a row added to the table without an answer here
// fails `every_registry_row_is_answered_or_declared` rather than doing nothing
// quietly.
//
// Two kinds of row are below, and the difference is stated rather than implied:
//
// * **wired** — the row's answer is produced here, from the same functions the
//   subcommand or the one-shot path uses (`provider::resolve_target` for a pinned
//   `/model`, `tui::begin_session` for `/new`, `schedule::status_text` for
//   `/schedule status`, the `KEYBINDINGS` and the registry tables themselves for
//   `/hotkeys` and `/help`). Nothing here is `async` and nothing spawns, which is what
//   makes the palette gate an offline test.
// * **declared** — the row names the function the subcommand reaches and why this file
//   does not call it yet. `every_declared_target_names_a_function_that_exists` reads the
//   named source file and fails if that function is renamed or deleted, so a declaration
//   cannot rot into a lie.

/// What one palette row produces when it runs.
#[derive(Debug)]
pub enum Answer {
    /// Lines for the transcript, now.
    Lines(Vec<String>),
    /// A stated limitation: the function the subcommand reaches, and what is missing.
    Declared {
        /// The path of the function `pie <command>` runs, spelled as a Rust path so the
        /// declaration can be checked against the source.
        target: &'static str,
        /// Why this stage does not call it, in one line.
        note: &'static str,
    },
    /// No arm matched the name: a registry row nothing dispatches.
    Unplaced,
}

/// The lines a local command answers with, from whichever of the three shapes it is.
///
/// A thin wrapper over [`answer`]: `/memory` arrives as M7's `LocalCommand` and every
/// other row as a name, and both are looked up in the one dispatch. That is deliberate —
/// a `match` here that answered a row itself would be the second dispatch §21.3 warns
/// about, and `/memory` would stop being a registry row.
pub fn local_lines(cfg: &Config, app: &mut App, command: &LocalCommand) -> Vec<String> {
    let (name, args) = match command {
        LocalCommand::Memory => (slash::MEMORY, ""),
        LocalCommand::Slash { name, args } => (*name, args.as_str()),
        LocalCommand::Unknown(word) => return unknown_lines(word),
    };

    match answer(cfg, app, name, args) {
        Answer::Lines(lines) => lines,
        Answer::Declared { target, note } => declared_lines(name, target, note),
        Answer::Unplaced => vec![format!(
            "`/{name}` is in the palette's table and no arm of `commands::tui::answer` \
             answers it — the registry and the dispatch have drifted apart"
        )],
    }
}

/// What the row named `name` does, for the arguments `args`.
pub fn answer(cfg: &Config, app: &mut App, name: &str, args: &str) -> Answer {
    match name {
        "model" => Answer::Lines(model_lines(cfg, app, args)),
        "effort" => Answer::Lines(effort_lines(app, args)),
        "backend" => Answer::Lines(backend_lines(app, args)),
        "new" => Answer::Lines(new_lines(app)),
        "status" => Answer::Lines(status_lines(app)),
        "help" => Answer::Lines(help_lines()),
        "hotkeys" => Answer::Lines(hotkey_lines()),
        "tools" => Answer::Lines(tools_stage_d(args)),
        // `/memory` keeps M7's renderer — the same bytes `pie memory show` prints, from
        // the same function — reached here through its registry row like every other
        // command, which is what absorbing the ad-hoc branch means (`DESIGN.md` §21.3).
        "memory" => Answer::Lines(super::memory::local_lines(&LocalCommand::Memory)),
        "schedule" if args.trim() != "run" => Answer::Lines(schedule_lines(cfg)),
        "usage" => Answer::Declared {
            target: "crate::commands::usage::run",
            note: "its answer is composed there — a per-backend quota probe plus \
                   `report::render_totals` over the session log — and composing the same \
                   two sources again here is the drift §21.3's one-implementation rule \
                   exists to prevent. It spends no model tokens, which is why `usage` is \
                   worth reaching for first.",
        },
        "doctor" => Answer::Declared {
            target: "crate::commands::backends::doctor",
            note: "it probes every backend CLI, and a spawn belongs to the loop rather \
                   than to a row's answer.",
        },
        "profile" => Answer::Declared {
            target: "crate::commands::profile::run",
            note: "the projection is rendered there from `profile.toml`; a second renderer \
                   here would be a second answer to \"what persona is being sent\".",
        },
        "resume" => Answer::Declared {
            target: "crate::commands::sessions::run",
            note: "resuming needs the session store read and the transcript rebuilt; \
                   `pie sessions` lists what there is to resume.",
        },
        "compact" => Answer::Declared {
            target: "crate::compact::compact",
            note: "compaction acts on the recorded turns, and the TUI's transcript is not \
                   the record — the store is (`DESIGN.md` §11).",
        },
        "learn" => Answer::Declared {
            target: "crate::commands::learn::run",
            note: "it writes the learned memory and its journal; `/memory` shows what the \
                   last apply wrote.",
        },
        "ocr" => Answer::Declared {
            target: "crate::commands::ocr::run",
            note: "OCR is answered by the mitsmini MCP server rather than by pie's own \
                   process (`DESIGN.md` §11).",
        },
        "watch" => Answer::Declared {
            target: "crate::commands::watch::run",
            note: "its `Options` are assembled from `config.toml` in `commands.rs`; \
                   assembling them again here would make `/watch` and `pie watch` two \
                   implementations, which is the one thing §21.3 forbids. `pie watch \
                   --dry-run` is free.",
        },
        "schedule" => Answer::Declared {
            target: "crate::commands::schedule::run",
            note: "a `run` consults the budget and then spawns; `pie schedule run <job>` \
                   does that today, and refuses under the shipped zero budget.",
        },
        // Never reached from the palette: the row's handler returns `Action::Quit`, so
        // the loop leaves before any answer is asked for. Stated rather than left to
        // `Unplaced`, because "nothing dispatches it" would be false.
        "quit" => Answer::Lines(vec![
            "`/quit` leaves the TUI — its handler returns `Action::Quit` directly, so \
             there is nothing to render"
                .to_string(),
        ]),
        _ => Answer::Unplaced,
    }
}

/// `/model [provider/model]` with an argument: the pinned form.
///
/// Resolved by [`provider::resolve_target`], the function `pie -p --model` and `pie tui`
/// both use, so a misspelt backend is refused with the same message as everywhere else —
/// and refused *here*, before any turn exists.
fn model_lines(cfg: &Config, app: &mut App, args: &str) -> Vec<String> {
    let spec = args.trim();
    match provider::resolve_target(cfg, Some(spec)) {
        Ok((backend, model)) => {
            app.backend = backend;
            app.model = model;
            vec![format!(
                "target  {} — sent as `--model` on the next turn, exactly as `--model {spec}` \
                 would have",
                app.target()
            )]
        }
        Err(error) => vec![format!("`/model {spec}` did not resolve: {error}")],
    }
}

/// `/effort <level>`: the same `ChatRequest::effort` field `--effort` fills.
fn effort_lines(app: &mut App, args: &str) -> Vec<String> {
    let level = args.trim();
    if level.is_empty() {
        return match &app.effort {
            Some(level) => vec![format!(
                "effort  {level} — set this session; the backend's own default applies when \
                 it is not"
            )],
            None => vec![
                "effort  not set — the backend's configured default applies \
                 (`agy.default_effort`), as it does for `pie -p`"
                    .to_string(),
            ],
        };
    }
    app.effort = Some(level.to_string());
    vec![format!(
        "effort  {level} — `--effort {level}` on the next turn's argv"
    )]
}

/// `/backend <name>`: validated through the registry `resolve_target` consults.
fn backend_lines(app: &mut App, args: &str) -> Vec<String> {
    let known = super::models::known_backends().join(", ");
    let name = args.trim();
    if name.is_empty() {
        return vec![format!("target  {} — backends: {known}", app.target())];
    }
    if provider::by_id(name).is_none() {
        return vec![format!(
            "`{name}` is not a backend pie can drive — backends: {known}"
        )];
    }

    let switched = app.backend != name;
    app.backend = name.to_string();
    if switched && app.model.is_some() {
        // A bare model id belongs to one backend. Keeping it would send the next turn to
        // a subscription that may not have it, so it is dropped *and said* rather than
        // silently carried.
        app.model = None;
        return vec![format!(
            "target  {} — the pinned model was dropped, because a model id belongs to one \
             backend (`/model` pins another)",
            app.target()
        )];
    }
    vec![format!("target  {}", app.target())]
}

/// `/new`: a new session, through the same minter `pie tui` opens with.
fn new_lines(app: &mut App) -> Vec<String> {
    let session = tui::begin_session(&app.backend);
    app.session = session.id().to_string();
    app.vendor_id = None;
    app.transcript.clear();
    app.tool_events.clear();
    app.scroll = 0;
    app.status = Status::Idle;
    vec![
        format!("session {} — a new conversation", app.session),
        "the transcript was cleared; `pie sessions` lists the recorded ones".to_string(),
    ]
}

/// `/status`: the fields the footer shows, spelled out for the transcript.
fn status_lines(app: &App) -> Vec<String> {
    let status = match &app.status {
        Status::Idle => "idle".to_string(),
        Status::Running => "running".to_string(),
        Status::Done => "done".to_string(),
        Status::Failed(reason) => format!("failed — {reason}"),
    };
    let mut lines = vec![
        format!("folder  {}", paths::pie_home().display()),
        format!("session {}", app.session),
        format!("target  {}", app.target()),
        format!("status  {status}"),
        match &app.effort {
            Some(effort) => format!("effort  {effort}"),
            None => "effort  the backend's default".to_string(),
        },
    ];

    match &app.last_usage {
        Some(usage) => {
            lines.push(format!(
                "tokens  {} in · {} out · {} thinking",
                usage.input_tokens, usage.output_tokens, usage.thinking_tokens
            ));
            // Zero means "not reported", never "free" (`Usage::cost_usd_micros`).
            lines.push(if usage.cost_usd_micros > 0 {
                format!("cost    ${:.4}", usage.cost_usd_micros as f64 / 1_000_000.0)
            } else {
                "cost    not reported by this backend".to_string()
            });
        }
        None => lines.push("tokens  no turn yet in this session".to_string()),
    }
    lines
}

/// `/help`: the registry, rendered by the registry.
fn help_lines() -> Vec<String> {
    let mut lines = vec![format!(
        "{} commands — this list is `slash::SLASH_COMMANDS`, so a row is in the palette \
         the moment it is in the table:",
        slash::SLASH_COMMANDS.len()
    )];
    lines.extend(slash::menu_lines());
    lines
}

/// `/hotkeys`: the keymap, rendered by the keymap.
fn hotkey_lines() -> Vec<String> {
    let mut lines = vec![format!(
        "{} keys — this list is `app::KEYBINDINGS`, the table the footer draws:",
        KEYBINDINGS.len()
    )];
    lines.extend(
        KEYBINDINGS
            .iter()
            .map(|binding| format!("/{} — {}", binding.keys, binding.description)),
    );
    lines
}

/// What a typo is told: the sentence M7 froze, then the commands that do exist.
///
/// The list is generated, which is the point — M7's version named `/memory` inside a
/// hand-written sentence, so a second command would have had to be remembered there.
/// `the_unknown_answer_names_every_registry_row` fails if this stops being generated.
fn unknown_lines(word: &str) -> Vec<String> {
    let mut lines = vec![format!(
        "`/{word}` is not a pie command, and nothing was sent to a backend"
    )];
    lines.extend(slash::menu_lines());
    lines
}

/// `/schedule status`: the same function `pie schedule` prints from.
fn schedule_lines(cfg: &Config) -> Vec<String> {
    match super::schedule::status_text(cfg, &paths::pie_home(), session_log::now_millis()) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(error) => vec![format!("`pie schedule status` could not be read: {error}")],
    }
}

/// The sentence a declared row gets: what it reaches, and what is missing.
fn declared_lines(name: &str, target: &str, note: &str) -> Vec<String> {
    vec![
        format!(
            "`/{name}` is declared and not wired into the TUI yet — it reaches `{target}`, \
             which `pie` runs from the command line today"
        ),
        note.to_string(),
    ]
}

/// `/tools` — **M12 stage D**, and the reason the palette is worth building.
///
/// `DESIGN.md` §21.4 is the specification: `list` prints every answered tool with its
/// *effective* state, read from the resolved `Tools`; `run <name> <json-args>` invokes
/// one tool with no model turn, which is the interactive path M11's tier otherwise lacks
/// on a machine with no API key.
///
/// Both need `config.toml`'s `tools` section resolved and M11's tier handed to this
/// function, so this stage declares the surface and states the limitation rather than
/// assembling a tool list from anywhere else. `tests/tui_tools.rs` is the gate that
/// replaces this answer with the real one, and the first assertion in it should be that
/// this sentence is gone.
pub fn tools_stage_d(args: &str) -> Vec<String> {
    let asked = match args.trim() {
        "" => "list".to_string(),
        other => other.to_string(),
    };
    vec![
        format!(
            "`/tools {asked}` is stage D of M12 (`DESIGN.md` §21.4) and is not wired yet: \
             it reads the resolved `tools` table and runs one tool with no model turn, \
             which needs M11's tier handed in here"
        ),
        "nothing was spawned, no configuration was read for this answer, and none was \
         written"
            .to_string(),
    ]
}
