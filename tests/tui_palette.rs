//! M12 stage A gate: the `/` palette's registry (`planning/DESIGN.md` §21.3).
//!
//! `planning/ACCEPTANCE.md` §20's stage A row is the specification, verbatim:
//!
//! > the const registry itself: every row is reachable and produces its handler through
//! > `reduce`; typing filters by prefix and the argument hint is displayed but never
//! > matched; Up/Down clamp rather than wrap; Esc closes and leaves the prompt untouched;
//! > an unknown command is answered naming what exists and starts **no turn**; `/memory`
//! > arrives through the registry, not through a special case beside it
//!
//! Two properties are what make the assertions below means rather than ceremony:
//!
//! * **the walk has an oracle that is not the code under test.** Driving a row through
//!   `reduce` and comparing against *the same handler* would be tautological: a handler
//!   that returns the wrong [`Action`] would agree with itself. So the expected action and
//!   the expected [`Effect`] are written here, from §21.3's table, and a row that gives
//!   the wrong one fails against the expectation rather than against its own reflection.
//! * **the two front-ends are compared, not described.** `Enter` on a palette row and the
//!   same invocation typed by hand must reach one effect; the equality is asserted for
//!   every row, which is what stops the palette becoming a second dispatch (§21.3).
//!
//! Nothing here spawns a process, opens a socket, reads a real `~/.pie` or makes a model
//! turn: the gate is offline, which is what `ACCEPTANCE.md` §20's G5 row records.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pie::commands::tui::{answer, local_lines, tools_stage_d, Answer};
use pie::config::Config;
use pie::session::Session;
use pie::tui::app::{reduce, Action, App, Input, Status, KEYBINDINGS};
use pie::tui::{effect_for, local_command, slash, Effect, InFlight, LocalCommand, Step};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The rows §21.3 requires, as the specification lists them.
///
/// Written out because the requirement is a *list from the design document*, and a test
/// cannot read the document: this and [`slash::SLASH_COMMANDS`] are asserted to be the
/// same set, so a row cannot be dropped from the table without failing here, and a row
/// cannot be added to the table without a line here. It is the one place the names exist
/// twice, and it exists to make the table's completeness checkable rather than assumed.
const REQUIRED: [&str; 19] = [
    "model", "effort", "backend", "new", "resume", "compact", "usage", "doctor", "profile",
    "status", "tools", "ocr", "learn", "memory", "schedule", "watch", "help", "hotkeys", "quit",
];

/// A concrete argument for each row, so the walk drives real invocations rather than
/// bare names: a row that only works with no argument is half a row.
fn sample_args(name: &str) -> &'static str {
    match name {
        "model" => "agy/gemini-3.8-flash",
        "effort" => "high",
        "backend" => "agy",
        "resume" => "s-1-2",
        "compact" => "keep the decisions",
        "usage" => "codex",
        "tools" => "list",
        "ocr" => "shot.png",
        "schedule" => "status",
        "watch" => "all",
        _ => "",
    }
}

/// The invocation a user would type for one row with [`sample_args`].
fn invocation(row: &slash::SlashCommand) -> String {
    slash::settled(row, sample_args(row.name))
}

/// Type `text` one key at a time, through [`reduce`] rather than by assigning the
/// prompt: opening the palette is part of what is under test.
fn typed(text: &str) -> App {
    let mut app = App::new("codex", None, "s-1-2");
    for c in text.chars() {
        reduce(&mut app, Input::Char(c));
    }
    app
}

/// The highlight's row name, read without holding a borrow across `app`.
fn highlighted(app: &App) -> Option<&'static str> {
    let palette = app.palette;
    palette
        .and_then(|palette| palette.highlight(&app.prompt))
        .map(|row| row.name)
}

/// The lines an [`Answer`] becomes, so a test can read any row's answer through the same
/// function the loop calls.
fn lines_of(answer: Answer) -> Vec<String> {
    match answer {
        Answer::Lines(lines) => lines,
        Answer::Declared { target, note } => vec![target.to_string(), note.to_string()],
        Answer::Unplaced => vec![],
    }
}

/// A `PIE_HOME` pinned for the length of one test, with the environment lock
/// `tests/memory_cmd.rs` uses for the same reason: `paths::pie_home()` is process-wide.
struct EnvPin {
    saved: Option<String>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl EnvPin {
    fn new(pie_home: &std::path::Path) -> Self {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("PIE_HOME").ok();
        std::env::set_var("PIE_HOME", pie_home);
        Self {
            saved,
            _guard: guard,
        }
    }
}

impl Drop for EnvPin {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(value) => std::env::set_var("PIE_HOME", value),
            None => std::env::remove_var("PIE_HOME"),
        }
    }
}

// ---------------------------------------------------------------------------
// The table itself — "every registry row is a row and not a claim"
// ---------------------------------------------------------------------------

#[test]
fn the_table_has_one_row_per_required_command_and_no_blanks() {
    let names: Vec<&str> = slash::SLASH_COMMANDS.iter().map(|row| row.name).collect();

    for required in REQUIRED {
        assert!(
            names.contains(&required),
            "`/{required}` is required by DESIGN §21.3 and has no row: {names:?}"
        );
    }
    assert_eq!(
        names.len(),
        REQUIRED.len(),
        "the table and the required list must be the same set: {names:?}"
    );

    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        names.len(),
        "two rows share a name: {names:?}"
    );

    for row in slash::SLASH_COMMANDS {
        assert!(!row.name.trim().is_empty(), "a row has no name: {row:?}");
        assert!(
            !row.description.trim().is_empty(),
            "`/{}` has no description, so the menu would show a gap",
            row.name
        );
        assert!(
            row.menu_line().starts_with(&format!("/{}", row.name)),
            "`/{}` does not name itself in its own menu line: {}",
            row.name,
            row.menu_line()
        );
    }
}

// ---------------------------------------------------------------------------
// Open, filter, select, close — §21.3's behaviour table
// ---------------------------------------------------------------------------

#[test]
fn a_slash_at_an_empty_prompt_opens_the_palette_and_nowhere_else_does() {
    let mut app = App::new("codex", None, "s-1-2");
    assert_eq!(reduce(&mut app, Input::Char('/')), Action::None);
    assert!(app.palette.is_some(), "`/` at an empty prompt opens it");
    assert_eq!(
        app.palette_rows().len(),
        slash::SLASH_COMMANDS.len(),
        "it opens on the whole table"
    );
    assert_eq!(app.prompt, "/", "the slash stays in the composer");

    // A slash anywhere else is a slash: in the middle of a prompt, and after one.
    let mut app = typed("hello");
    reduce(&mut app, Input::Char('/'));
    assert!(
        app.palette.is_none(),
        "`/` must not open the palette inside a prompt"
    );

    // Deleting the slash closes it again: a prompt that is not a slash word is not a
    // command, and the overlay would be offering rows for text it is not about.
    let mut app = typed("/");
    assert!(app.palette.is_some());
    reduce(&mut app, Input::Backspace);
    assert!(app.palette.is_none(), "removing the slash closes it");
    assert_eq!(app.prompt, "");
}

#[test]
fn typing_filters_by_prefix_on_the_name() {
    let rows = |filter: &str| {
        slash::matching(filter)
            .iter()
            .map(|row| row.name)
            .collect::<Vec<_>>()
    };

    assert_eq!(rows(""), REQUIRED.to_vec(), "the empty filter is the menu");
    assert_eq!(rows("mo"), vec!["model"]);
    assert_eq!(rows("re"), vec!["resume"]);
    assert_eq!(rows("nosuchthing"), Vec::<&str>::new());

    // Through the reducer, which is the path a user drives.
    let app = typed("/wa");
    assert_eq!(highlighted(&app), Some("watch"));
    let app = typed("/w");
    assert_eq!(highlighted(&app), Some("watch"));
    let app = typed("/memory");
    assert_eq!(highlighted(&app), Some("memory"));
}

#[test]
fn the_argument_hint_is_displayed_and_never_matched() {
    for row in slash::SLASH_COMMANDS {
        let displayed = row.menu_line();
        if row.argument.is_empty() {
            continue;
        }
        assert!(
            displayed.contains(row.argument),
            "`/{}` does not display its hint: {displayed}",
            row.name
        );

        // The first word of the hint, used as a filter, must not reach this row. A filter
        // that also matched the hint would answer `/provider` with `/model`, which is the
        // difference between a command palette and a documentation search.
        let word = row
            .argument
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or_default();
        if word.is_empty() {
            continue;
        }
        assert!(
            !slash::matching(word)
                .iter()
                .any(|other| other.name == row.name),
            "the hint word `{word}` of `/{}` matches the row, so the hint is being matched: {:?}",
            row.name,
            slash::matching(word)
        );
    }
}

#[test]
fn up_and_down_clamp_rather_than_wrap() {
    let mut app = typed("/");
    let len = app.palette_rows().len();

    reduce(&mut app, Input::Down);
    assert_eq!(highlighted(&app), Some(slash::SLASH_COMMANDS[1].name));

    for _ in 0..len + 3 {
        reduce(&mut app, Input::Down);
    }
    assert_eq!(
        highlighted(&app),
        Some(slash::SLASH_COMMANDS[len - 1].name),
        "the last row is the end: a wrap would hide that you have reached it"
    );

    for _ in 0..len + 3 {
        reduce(&mut app, Input::Up);
    }
    assert_eq!(
        highlighted(&app),
        Some(slash::SLASH_COMMANDS[0].name),
        "the first row is the start"
    );

    // Narrowing the filter must pull the highlight back in, or `Enter` would run a row
    // the user cannot see.
    let mut app = typed("/");
    reduce(&mut app, Input::Down);
    assert_eq!(highlighted(&app), Some("effort"));
    reduce(&mut app, Input::Char('h'));
    let narrowed = slash::matching("h");
    assert_eq!(narrowed.len(), 2, "the filter matched: {narrowed:?}");
    assert_eq!(
        highlighted(&app),
        Some("hotkeys"),
        "the highlight stayed in range rather than pointing past the narrowed list"
    );
}

#[test]
fn esc_closes_the_palette_and_leaves_the_prompt_alone() {
    let mut app = typed("/wa");
    let before = app.prompt.clone();
    assert!(app.palette.is_some());

    assert_eq!(reduce(&mut app, Input::Esc), Action::None);
    assert!(app.palette.is_none(), "Esc closes it");
    assert_eq!(app.prompt, before, "and leaves the text alone");
    assert!(!app.should_quit, "closing an overlay is not quitting");
    assert_eq!(app.status, Status::Idle, "and is not a status change");

    // The settled behaviour after it: `Esc` again is the ordinary `Esc` (`Quit` when
    // there is nothing to cancel), which is M6's frozen row and not this stage's to move.
    assert_eq!(reduce(&mut app, Input::Esc), Action::Quit);
    assert!(app.should_quit);
}

#[test]
fn enter_runs_the_highlighted_row_even_when_it_is_not_what_was_typed() {
    // The menu is open on the whole table and the highlight walks onto a row the user has
    // not typed a letter of; `Enter` must run **that** row — the highlight is the
    // selection, not the text.
    let mut app = typed("/");
    let expected = slash::SLASH_COMMANDS[3];
    for _ in 0..3 {
        reduce(&mut app, Input::Down);
    }
    assert_eq!(highlighted(&app), Some(expected.name));

    let action = reduce(&mut app, Input::Enter);
    assert_eq!(action, (expected.handler)(""));
    assert_eq!(
        app.prompt,
        format!("/{}", expected.name),
        "`Enter` settles the row it ran, not the text that was typed"
    );
}

// ---------------------------------------------------------------------------
// The walk — every row is reachable and dispatches
// ---------------------------------------------------------------------------

/// What each row must reach, written from §21.3 rather than read from the row's own
/// handler — this is the oracle that makes a handler returning the wrong action a failure
/// rather than self-consistency.
fn expected_effect(row: &slash::SlashCommand, args: &str) -> Effect {
    match row.name {
        "quit" => Effect::Quit,
        "memory" => Effect::Local(LocalCommand::Memory),
        "model" if args.trim().is_empty() => Effect::Catalog,
        name => Effect::Local(LocalCommand::Slash {
            name,
            args: args.to_string(),
        }),
    }
}

/// What each row's handler must answer, for the same reason: an independent statement of
/// the row's meaning.
fn expected_action(row: &slash::SlashCommand, args: &str) -> Action {
    match row.name {
        "quit" => Action::Quit,
        "model" if args.trim().is_empty() => Action::OpenPicker,
        _ => Action::PaletteRun,
    }
}

#[test]
fn every_registry_row_is_reachable_and_dispatches_through_reduce() {
    for row in slash::SLASH_COMMANDS {
        let text = invocation(row);
        let args = slash::arguments_of(&text, row.name).to_string();

        // Reachable: typing the row's name leaves the palette open on it.
        let mut app = typed(&text);
        assert!(
            app.palette.is_some(),
            "`{text}` did not leave the palette open"
        );
        assert_eq!(
            highlighted(&app),
            Some(row.name),
            "`{text}` does not reach `{}`: {:?}",
            row.name,
            app.palette_rows()
                .iter()
                .map(|row| row.name)
                .collect::<Vec<_>>()
        );

        // Dispatches: `Enter` returns the row's action, and the oracle above agrees.
        let action = reduce(&mut app, Input::Enter);
        assert_eq!(
            action,
            expected_action(row, &args),
            "`/{}` produced `{}`, which is not what §21.3 says it means",
            row.name,
            action.label()
        );
        assert_eq!(
            action,
            (row.handler)(&args),
            "`/{}` did not dispatch through its own handler",
            row.name
        );
        assert!(app.palette.is_none(), "`Enter` must close the overlay");

        // …and the action reaches an effect the loop executes.
        assert_ne!(
            effect_for(action, text.clone()),
            Effect::Continue,
            "`/{}` reaches no effect, so the row does nothing",
            row.name
        );
    }
}

#[test]
fn every_row_of_the_table_reaches_its_stated_effect_and_both_front_ends_agree() {
    for row in slash::SLASH_COMMANDS {
        let text = invocation(row);
        let args = slash::arguments_of(&text, row.name).to_string();
        let action = (row.handler)(&args);

        assert_eq!(
            effect_for(action, text.clone()),
            expected_effect(row, &args),
            "`{text}` reaches the wrong effect"
        );

        // The palette and the typed path are one dispatch: whatever the prompt settled
        // into, submitting it by hand means the same thing.
        assert_eq!(
            effect_for(Action::PaletteRun, text.clone()),
            effect_for(Action::Submit, text.clone()),
            "`{text}` means two different things depending on how it was chosen"
        );
    }
}

#[test]
fn the_palette_runs_the_row_its_handler_names_not_a_second_reading_of_it() {
    // `/quit` from the palette leaves, and says so through the loop's own action.
    let mut app = typed("/quit");
    assert_eq!(reduce(&mut app, Input::Enter), Action::Quit);
    assert!(app.should_quit);

    // `/model` bare opens M6's picker, and the picker must actually be open: the loop
    // takes a catalog only while `App::picker` is `Some`, so an `OpenPicker` action that
    // left it closed would fill nothing and show nothing.
    let mut app = typed("/model");
    assert_eq!(reduce(&mut app, Input::Enter), Action::OpenPicker);
    assert!(
        app.picker.is_some(),
        "the picker must be open before the catalog arrives"
    );

    // `/model provider/model` pins the target through `provider::resolve_target`.
    let cfg = Config::default();
    let mut app = App::new("codex", None, "s-1-2");
    let lines = local_lines(
        &cfg,
        &mut app,
        &LocalCommand::Slash {
            name: "model",
            args: "agy/gemini-3.8-flash".to_string(),
        },
    );
    assert_eq!(app.backend, "agy");
    assert_eq!(app.model.as_deref(), Some("gemini-3.8-flash"));
    assert!(lines.join(" ").contains("agy/gemini-3.8-flash"));

    // A misspelt backend is refused with the shared message, and sets nothing.
    let mut app = App::new("codex", None, "s-1-2");
    let lines = local_lines(
        &cfg,
        &mut app,
        &LocalCommand::Slash {
            name: "model",
            args: "nosuchmodel".to_string(),
        },
    );
    assert_eq!(app.backend, "codex", "a refusal must not switch the target");
    assert!(lines.join(" ").contains("did not resolve"));
}

#[test]
fn a_palette_row_starts_no_turn_and_records_nothing() {
    let scratch = tempfile::tempdir().unwrap();
    let mut session = Session::start_in(scratch.path(), "codex", std::path::Path::new("."));
    let mut app = typed("/watch all");
    let mut in_flight = InFlight::default();

    let effect = pie::tui::act(
        &mut app,
        &mut session,
        &mut in_flight,
        Step::Key(Input::Enter),
    );

    assert_eq!(
        effect,
        Effect::Local(LocalCommand::Slash {
            name: "watch",
            args: "all".to_string()
        })
    );
    assert!(
        in_flight.started.is_none() && in_flight.prompt.is_empty(),
        "a palette row is not a turn, so no turn clock may start"
    );
    assert_eq!(session.turns(), 0, "and nothing may be recorded");
}

// ---------------------------------------------------------------------------
// The unknown command
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_command_is_answered_naming_what_exists_and_starts_no_turn() {
    // The reducer's half: nothing matches, so `Enter` runs nothing at all — no `User`
    // entry, no `Running`, no settled prompt.
    let mut app = typed("/nosuchthing");
    let transcript = app.transcript.len();
    assert!(app.palette_rows().is_empty(), "no row matches");
    assert_eq!(reduce(&mut app, Input::Enter), Action::None);
    assert_eq!(
        app.transcript.len(),
        transcript,
        "an unknown command pushes no entry"
    );
    assert!(
        !app.status.is_running(),
        "and starts no turn — the thing the rule exists for"
    );
    assert!(app.palette.is_none(), "and closes the overlay");

    // The answer: through the same dispatch a settled command takes, naming what exists.
    let cfg = Config::default();
    let mut app = App::new("codex", None, "s-1-2");
    for action in [Action::PaletteRun, Action::Submit] {
        let effect = effect_for(action, "/nosuchthing".to_string());
        let command = match effect {
            Effect::Local(command) => command,
            other => panic!("an unknown command must not reach the loop as a turn: {other:?}"),
        };
        let lines = local_lines(&cfg, &mut app, &command);
        let joined = lines.join(" ");
        assert!(joined.contains("is not a pie command"), "{joined}");
        assert!(
            joined.contains("nothing was sent to a backend"),
            "a typo must not cost quota:\n{joined}"
        );
        for row in slash::SLASH_COMMANDS {
            assert!(
                joined.contains(&format!("/{}", row.name)),
                "the answer must name `{}`, which does exist:\n{joined}",
                row.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// `/memory` arrives through the registry
// ---------------------------------------------------------------------------

#[test]
fn the_memory_row_answers_through_the_registry_rather_than_a_branch_beside_it() {
    // The row exists, and the name M7's renderer is selected by is a row rather than a
    // word nothing checks.
    assert!(
        slash::SLASH_COMMANDS
            .iter()
            .any(|row| row.name == slash::MEMORY),
        "`/memory` must be a row of the table"
    );
    assert_eq!(
        slash::find(slash::MEMORY).map(|row| (row.handler)("")),
        Some(Action::PaletteRun)
    );

    // Both front-ends reach it through the same lookup, and both reach M7's renderer.
    let expected = Effect::Local(LocalCommand::Memory);
    assert_eq!(local_command("/memory"), Some(LocalCommand::Memory));
    assert_eq!(
        effect_for(Action::Submit, "/memory".to_string()),
        expected.clone()
    );
    assert_eq!(
        effect_for(Action::PaletteRun, "/memory".to_string()),
        expected
    );

    // And the palette reaches it by typing a prefix, which is what "reachable" means for
    // a menu row.
    let mut app = typed("/me");
    assert_eq!(highlighted(&app), Some("memory"));
    assert_eq!(reduce(&mut app, Input::Enter), Action::PaletteRun);
    assert_eq!(
        app.prompt, "/memory",
        "the invocation settled into the prompt"
    );

    // Every row, not just this one, is dispatchable by typing it: a row that the text
    // path does not recognise would be a command that works from the menu alone.
    for row in slash::SLASH_COMMANDS {
        let text = invocation(row);
        assert!(
            local_command(&text).is_some(),
            "`{text}` is in the table and does not dispatch from the text path"
        );
    }
}

// ---------------------------------------------------------------------------
// Every row answers something — wired, or declared with its target
// ---------------------------------------------------------------------------

#[test]
fn every_registry_row_is_answered_or_declared_with_at_least_one_line() {
    let cfg = Config::default();
    for row in slash::SLASH_COMMANDS {
        let args = sample_args(row.name);
        let mut app = App::new("codex", None, "s-1-2");
        let action = (row.handler)(args);
        let answer = answer(&cfg, &mut app, row.name, args);

        if action == Action::PaletteRun {
            assert!(
                !matches!(answer, Answer::Unplaced),
                "`/{}` is a row of the table and no arm of `commands::tui::answer` answers it",
                row.name
            );
        }

        let lines = lines_of(answer);
        assert!(
            lines.iter().any(|line| !line.trim().is_empty()),
            "`/{}` answers with nothing, which looks exactly like a hang (M7's rule)",
            row.name
        );
    }

    // The unreachable-from-the-palette rows still answer: `/quit`'s handler returns
    // `Action::Quit`, so no `PaletteRun` reaches it, and saying so beats a blank.
    let cfg = Config::default();
    let mut app = App::new("codex", None, "s-1-2");
    assert!(!lines_of(answer(&cfg, &mut app, "quit", "")).is_empty());
}

#[test]
fn every_declared_target_names_a_function_that_exists() {
    let cfg = Config::default();
    let mut app = App::new("codex", None, "s-1-2");
    let mut declared = Vec::new();

    for row in slash::SLASH_COMMANDS {
        if let Answer::Declared { target, .. } =
            answer(&cfg, &mut app, row.name, sample_args(row.name))
        {
            declared.push((row.name, target));
        }
    }

    assert!(
        declared.len() >= 8,
        "the declared half must not have quietly become the whole table: {declared:?}"
    );

    for (name, target) in declared {
        let (module, symbol) = target
            .rsplit_once("::")
            .unwrap_or_else(|| panic!("`{target}` for `/{name}` is not a Rust path"));
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "{}.rs",
            module.replace("crate::", "src/").replace("::", "/")
        ));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} for `/{name}`: {error}", path.display()));
        assert!(
            source.contains(&format!("fn {symbol}")),
            "`/{name}` declares that it reaches `{target}`, and {} has no `fn {symbol}` — the \
             declaration has rotted",
            path.display()
        );
    }
}

#[test]
fn the_tools_row_declares_its_stage_and_spends_nothing() {
    let cfg = Config::default();
    let mut app = App::new("codex", None, "s-1-2");
    for args in ["", "list", "run read {\"file_path\":\"Cargo.toml\"}"] {
        let lines = local_lines(
            &cfg,
            &mut app,
            &LocalCommand::Slash {
                name: "tools",
                args: args.to_string(),
            },
        );
        let joined = lines.join(" ");
        assert!(joined.contains("stage D"), "{joined}");
        assert!(joined.contains("not wired yet"), "{joined}");
        let asked = if args.is_empty() { "list" } else { args };
        assert!(
            joined.contains(asked),
            "the answer must name what was asked for:\n{joined}"
        );
    }

    // The row routes through the function this stage added, rather than through a
    // sentence written in the dispatch.
    assert!(tools_stage_d("run read {}")[0].contains("stage D"));
}

// ---------------------------------------------------------------------------
// Generated from the tables, and read-only
// ---------------------------------------------------------------------------

#[test]
fn the_help_and_hotkey_answers_are_generated_from_their_tables() {
    let cfg = Config::default();
    let mut app = App::new("codex", None, "s-1-2");

    let help = local_lines(
        &cfg,
        &mut app,
        &LocalCommand::Slash {
            name: "help",
            args: String::new(),
        },
    )
    .join("\n");
    for row in slash::SLASH_COMMANDS {
        assert!(
            help.contains(&row.menu_line()),
            "`/help` omits `{}`'s own line, so the menu is a second copy:\n{help}",
            row.name
        );
    }

    let hotkeys = local_lines(
        &cfg,
        &mut app,
        &LocalCommand::Slash {
            name: "hotkeys",
            args: String::new(),
        },
    )
    .join("\n");
    for binding in KEYBINDINGS {
        assert!(
            hotkeys.contains(binding.keys) && hotkeys.contains(binding.description),
            "`/hotkeys` omits `{}` ({}), so the keymap is a second copy:\n{hotkeys}",
            binding.keys,
            binding.description
        );
    }
}

#[test]
fn answering_tools_usage_and_watch_leaves_config_toml_byte_identical() {
    // §20's check: the palette reads state and runs tools, and never makes the
    // enable-a-tool decision on the user's behalf.
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("pie-home");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("config.toml");
    std::fs::write(
        &path,
        "[general]\ndefault_backend = \"codex\"\n\n[tools]\nworkspace = \".\"\n",
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();

    let _env = EnvPin::new(&home);
    let cfg = Config::default();
    let mut app = App::new("codex", None, "s-1-2");
    for (name, args) in [("tools", "list"), ("usage", ""), ("watch", "all")] {
        let lines = local_lines(
            &cfg,
            &mut app,
            &LocalCommand::Slash {
                name,
                args: args.to_string(),
            },
        );
        assert!(!lines.is_empty(), "`/{name}` must answer something");
    }

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a palette session must leave config.toml byte-identical"
    );
}
