//! M6 stage D gate: the keymap, the model picker, and the defects the first frozen
//! contract produced.
//!
//! `planning/ACCEPTANCE.md` §9 names `cargo test --test tui_keys` as stage D's gate and
//! states its shape: *"the documented keybindings each produce their action; choosing a
//! model updates `App::model`; every keybinding is documented in help; a second turn can
//! be submitted after the first completes"*. The contract is `planning/DESIGN.md` §12's
//! keybinding table and its turn-lifecycle table; the bug this file exists to keep
//! closed is **B26** in `ACCEPTANCE.md` §13 (the TUI could send exactly one turn), plus
//! **B29** (a large `App::scroll` panicked inside ratatui).
//!
//! Two properties make the assertions below means rather than ceremony:
//!
//! * **the help and the reducer read one table.** `app::KEYBINDINGS` is driven through
//!   `reduce` row by row — a row that lies about its key or its action fails, and so does
//!   a key that works but is not in the table. The rendered footer is then asserted to be
//!   that table's own text, so a hand-written help string cannot pass.
//! * **the enumeration is guarded by the compiler.** [`documented_label`] names every
//!   `Action` variant in an exhaustive match with no wildcard arm and [`every_action`]
//!   names every `Input` variant, so adding a variant stops this file compiling instead
//!   of joining the enum with nothing documenting it — the same trick
//!   `tests/tui_app.rs` uses.
//!
//! Both defects were written as failing tests first and confirmed red against the
//! previous `src/tui/app.rs` and `src/tui/render.rs` (`ACCEPTANCE.md` §10, the bug
//! protocol). Nothing here spawns a process, opens a socket, reads `~/.pie` or calls a
//! backend's catalog: the picker is fed a `&[Model]` fixture, so this gate passes with no
//! network, no vendor CLI and no subscription quota spent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pie::provider::{Model, StreamEvent};
use pie::tui::app::{
    apply_event, help_text, reduce, Action, App, BindingState, Entry, Input, KeyBinding, Picker,
    PickerItem, Status, KEYBINDINGS,
};
use pie::tui::render::{self, render, HELP_ROWS};

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Draw `app` into an off-screen buffer of exactly `width` × `height`.
///
/// The whole reason the render layer is asserted through `TestBackend`: this file's
/// panic tests need to *draw* without a terminal, and `TestBackend` makes a panic a
/// failed assertion rather than a wrecked session.
fn draw(app: &App, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// The buffer's rows as strings, one per screen row, left to right.
///
/// Unlike `tests/tui_render.rs`'s helper this one does not have to know how many columns
/// a symbol covers, because nothing in this file carries a wide glyph — the keymap's own
/// words are ASCII. The wide-character knowledge lives in the file that sweeps widths
/// with CJK and emoji in them.
fn rows_of(buffer: &Buffer) -> Vec<String> {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// The rows of a freshly drawn screen.
fn screen(app: &App, width: u16, height: u16) -> Vec<String> {
    rows_of(&draw(app, width, height))
}

/// The whole screen, whitespace-collapsed.
///
/// Wrapping replaces the space it breaks on with a row break, so an assertion on a
/// sentence has to compare words rather than raw rows: this rejoins the rows and
/// collapses the seams.
fn flat_text(rows: &[String]) -> String {
    rows.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The rows the help footer occupies, joined and collapsed.
///
/// Read by *position* — the [`render::help_rows`] rows directly above the status line —
/// rather than by searching for a word. A test that looked for the text it expects would
/// pass on a footer built from anything that happened to contain it. The height comes from
/// the same function the layout uses, so the region this reads and the region `render`
/// reserved cannot drift; hard-coding it is what let a two-row footer pass a test written
/// against the 80x24 criterion.
fn help_region(rows: &[String], width: u16, height: u16) -> String {
    let help = usize::from(render::help_rows(width, height));
    assert!(
        rows.len() > help,
        "the screen must be tall enough for the layout: {rows:?}"
    );
    let status_row = rows.len() - 1;
    flat_text(&rows[status_row - help..status_row])
}

/// An app with a submittable prompt and no picker.
fn app_with_prompt() -> App {
    let mut app = App::new("codex", Some("gpt-6-luna".to_string()), "s-1-2");
    app.prompt = "hello".to_string();
    app.cursor = 5;
    app
}

/// The state a keymap row is documented in, built so that row is the one under test.
///
/// `DESIGN.md` §12's table is modal — five keys mean two things each — so a row is only
/// a contract together with its state, and the state is a field of the table rather than
/// something this file infers from the description.
fn app_in(state: BindingState) -> App {
    let mut app = app_with_prompt();
    match state {
        BindingState::PromptReady => {}
        BindingState::Running => app.status = Status::Running,
        BindingState::PickerOpen => app.picker = Some(picker_with_two_backends(0)),
    }
    app
}

/// A picker whose two rows are on **different backends**, built by the constructor the
/// loop uses.
///
/// Two backends on purpose: a picker that only ever set the model would pass a
/// single-backend test, and picking a row has to set `App::backend` as well
/// (`DESIGN.md` §12).
fn picker_with_two_backends(selected: usize) -> Picker {
    let mut picker = Picker::from_models("codex", &[Model::plain("gpt-6-luna", "GPT-6 Luna")]);
    picker.items.extend(
        Picker::from_models(
            "agy",
            &[Model::plain("gemini-3.8-flash", "gemini-3.8-flash")],
        )
        .items,
    );
    picker.selected = selected;
    picker
}

/// An app whose transcript holds `n` one-row user entries, `line-01` … `line-nn`.
fn app_with_lines(n: usize) -> App {
    let mut app = App::new("codex", None, "s-1-2");
    app.transcript = (1..=n)
        .map(|i| Entry::User(format!("line-{i:02}")))
        .collect();
    app
}

/// A terminal `Done` event, the shape both backends emit (`DESIGN.md` §6 rule 7).
fn done(text: &str) -> StreamEvent {
    StreamEvent::Done {
        text: text.to_string(),
        status: "SUCCESS".to_string(),
    }
}

// ---------------------------------------------------------------------------
// The keymap table is the specification
// ---------------------------------------------------------------------------

/// Every `Action` the UI can produce.
///
/// Listed by hand, and checked against an exhaustive match in [`documented_label`] — the
/// pair is deliberate: the match is what makes a *new* variant a compile error, and this
/// list is what the coverage assertion below walks.
const EVERY_ACTION: [Action; 10] = [
    Action::None,
    Action::Quit,
    Action::Submit,
    Action::Cancel,
    Action::ScrollUp,
    Action::ScrollDown,
    Action::OpenPicker,
    Action::PickerPrev,
    Action::PickerNext,
    Action::PickerAccept,
];

/// The name each variant carries, as an exhaustive match with no wildcard arm.
///
/// Two jobs, and both are needed. The exhaustive arms mean a new `Action` cannot be added
/// without stopping this file from compiling, rather than joining the enum with no row of
/// `KEYBINDINGS` documenting it. The comparison against [`Action::label`] in the test
/// below means the crate's own words and this list cannot drift apart, which a
/// hand-written list alone cannot notice.
fn documented_label(action: Action) -> &'static str {
    match action {
        Action::None => "no action",
        Action::Quit => "quit",
        Action::Submit => "submit",
        Action::Cancel => "cancel",
        Action::ScrollUp => "scroll up",
        Action::ScrollDown => "scroll down",
        Action::OpenPicker => "pick a model",
        Action::PickerPrev => "previous model",
        Action::PickerNext => "next model",
        Action::PickerAccept => "take the model",
    }
}

/// Every `Input` variant the terminal can produce, as an exhaustive list rather than a
/// sampled one: a new `Input` that the loop can deliver has to be documented here or the
/// keymap test will not drive it.
const EVERY_INPUT: [Input; 11] = [
    Input::Char('x'),
    Input::Enter,
    Input::Esc,
    Input::Backspace,
    Input::Up,
    Input::Down,
    Input::Left,
    Input::Right,
    Input::Tab,
    Input::Ctrl('c'),
    Input::Ctrl('q'),
];

/// The row signature a `KeyBinding` matches on, for "is this input documented at all".
fn documents_input(binding: &KeyBinding, input: Input) -> bool {
    // `Ctrl` is the one family a single `Input` value cannot enumerate: the table has a
    // row for `Ctrl('c')` and a row for "any other `Ctrl`, which the loop lower-cases
    // before it gets here. Comparing the discriminant would need `PartialEq` on a
    // character the table does not carry, so the family is matched explicitly.
    match (binding.input, input) {
        (Input::Ctrl(a), Input::Ctrl(b)) => (a == 'c') == (b == 'c'),
        (documented, actual) => documented == actual,
    }
}

/// The assertion that stops the help and the reducer drifting: every documented row is
/// driven through `reduce` in the state its row documents, and must return the `Action`
/// the row claims.
///
/// A string check on the help cannot catch a key that stopped working, and a test that
/// only checked the reducer cannot catch a key that was never documented. This is the
/// behavioural half of both, which is what `ACCEPTANCE.md` §9 asks for in as many words.
#[test]
fn every_documented_keybinding_produces_its_documented_action() {
    for binding in KEYBINDINGS {
        let mut app = app_in(binding.state);
        let action = reduce(&mut app, binding.input);
        assert_eq!(
            action,
            binding.action,
            "`{}` ({}) is documented as `{}` in {:?}, but `reduce` returned `{}`",
            binding.keys,
            binding.description,
            binding.action.label(),
            binding.state,
            action.label()
        );
    }
}

/// The other direction: an input the terminal can deliver and the reducer answers must
/// have a row, or the keymap is incomplete and its help claims more than it shows.
#[test]
fn every_input_the_terminal_can_deliver_is_documented() {
    for input in EVERY_INPUT {
        assert!(
            KEYBINDINGS
                .iter()
                .any(|binding| documents_input(binding, input)),
            "{input:?} is deliverable by the loop but no row of KEYBINDINGS documents it"
        );
    }
}

/// Coverage in the third direction: an `Action` the UI can produce with no row
/// documenting it is a behaviour a user cannot discover.
#[test]
fn every_action_variant_the_ui_can_produce_is_documented() {
    for action in EVERY_ACTION {
        assert_eq!(
            action.label(),
            documented_label(action),
            "the footer's word for `{action:?}` and this file's list disagree"
        );
        assert!(
            KEYBINDINGS.iter().any(|binding| binding.action == action),
            "`{}` is reachable but no row of KEYBINDINGS documents it",
            action.label()
        );
    }
}

/// A row with no key, no words or no action is not documentation: it is a blank that
/// would render as a gap in the footer and pass every assertion above.
#[test]
fn no_keybinding_row_is_blank() {
    for binding in KEYBINDINGS {
        assert!(
            !binding.keys.trim().is_empty(),
            "a row has no key: {binding:?}"
        );
        assert!(
            !binding.description.trim().is_empty(),
            "`{}` has no description: {binding:?}",
            binding.keys
        );
    }
}

// ---------------------------------------------------------------------------
// The help footer is that table, rendered
// ---------------------------------------------------------------------------

/// The footer at a generous width, and the one assertion a string check cannot make: the
/// rendered footer is `app::help_text`'s own words, in its own order, so the help cannot be
/// a second copy of the keymap that nothing contradicts.
///
/// 240 columns is the *comfortable* case, not the acceptance one — `ACCEPTANCE.md` §9's
/// criterion is 80x24, and [`every_documented_key_is_visible_at_eighty_by_twenty_four`] is
/// the test that holds it. Keeping both is deliberate: this one pins the footer to the
/// table, that one pins it to a terminal a person actually has.
#[test]
fn the_help_footer_is_the_generated_keymap() {
    let app = App::new("codex", None, "s-1-2");
    let rows = screen(&app, 240, 30);

    assert!(
        rows.last().unwrap().contains("session s-1-2"),
        "the help must sit above the status line, which stays the last row: {:?}",
        rows.last().unwrap()
    );

    let help = help_region(&rows, 240, 30);

    assert_eq!(
        help,
        flat_text(&[help_text()]),
        "the footer must be app::help_text's own words, so the help cannot be a second \
         copy of the keymap"
    );

    for binding in KEYBINDINGS {
        assert!(
            help.contains(binding.keys),
            "the rendered help omits `{}` ({}): {help:?}",
            binding.keys,
            binding.description
        );
    }
}

/// `ACCEPTANCE.md` §9's criterion, at the size it names: **80x24 — an ordinary terminal —
/// every key string in `KEYBINDINGS` appears in the buffer**.
///
/// The defect this replaces: stage D rendered the table into two reserved rows, which fitted
/// whole only from about 200 columns, so the criterion held on a terminal nobody has while
/// `char` … `enter` was all an 80-column user ever saw. Asserting the *keys* rather than the
/// prose is also what makes the test survive the descriptions being reworded — a key that
/// becomes unreachable is the regression, a shorter sentence is not.
///
/// `HELP_ROWS` is the ceiling, so this additionally pins the property that makes the
/// criterion reachable at all: the footer asks for a row count the frame can pay for.
#[test]
fn every_documented_key_is_visible_at_eighty_by_twenty_four() {
    let app = App::new("codex", None, "s-1-2");
    let (width, height) = (80u16, 24u16);
    let rows = screen(&app, width, height);
    let buffer_text = flat_text(&rows);
    let help = help_region(&rows, width, height);

    assert!(
        render::help_rows(width, height) >= 1 && render::help_rows(width, height) <= HELP_ROWS,
        "the footer must reserve at least one row and never more than the table's own: {}",
        render::help_rows(width, height)
    );

    for binding in KEYBINDINGS {
        assert!(
            buffer_text.contains(binding.keys),
            "at {width}x{height} the buffer omits `{}` ({}), so the keymap is documented \
             only on a wider terminal: {rows:?}",
            binding.keys,
            binding.description
        );
        assert!(
            help.contains(binding.keys),
            "`{}` is only outside the footer at {width}x{height}: {help:?}",
            binding.keys
        );
    }

    // Every row of the table, in order, at a terminal a person has: the footer is complete
    // rather than truncated, which a keys-only check would also pass if the last row were
    // half drawn.
    assert_eq!(
        help,
        flat_text(&[help_text()]),
        "the 80x24 footer must be the whole table, not its first rows"
    );
}

/// The same criterion on a terminal narrower than the one it names.
///
/// Worth its own test rather than a comment: 80 columns was never the binding constraint,
/// and a footer that fits only there would be the same defect moved one terminal size down.
/// The table was measured to fit whole from 35 columns at 24 rows, where the wrap needs
/// exactly the twelve rows the frame has left for it; 40 is asserted here so a description
/// may grow a little without a failure, and the failure it does catch is the honest one —
/// the keymap has outgrown a terminal people use, and either the wording or the row ceiling
/// has to give.
#[test]
fn the_whole_keymap_fits_a_forty_column_terminal() {
    let app = App::new("codex", None, "s-1-2");
    let (width, height) = (40u16, 24u16);
    let rows = screen(&app, width, height);
    let help = help_region(&rows, width, height);

    for binding in KEYBINDINGS {
        assert!(
            help.contains(binding.keys),
            "at {width}x{height} the footer drops `{}` ({}): {rows:?}",
            binding.keys,
            binding.description
        );
    }
    assert_eq!(
        help,
        flat_text(&[help_text()]),
        "at {width}x{height} the footer must be the whole table"
    );
}

/// A narrow terminal clips the footer; it must not panic, and `Input` deliberately has no
/// help-toggle key (`DESIGN.md` §12), so there is no second surface to fall back on.
///
/// The sweep starts at one column and includes heights shorter than the layout asks for,
/// because a window manager resizes terminals: a panic here is a crashed UI. The help also
/// yields rather than eats the frame here — below the height that fits both, the transcript
/// keeps its [`render::help_rows`] floor and the footer clips, which is what the position
/// read below has to survive.
#[test]
fn the_help_footer_survives_a_narrow_terminal() {
    let app = App::new("codex", None, "s-1-2");

    for width in 1..=40u16 {
        let rows = screen(&app, width, 16);
        assert_eq!(
            rows.len(),
            16,
            "width {width}: the frame lost rows, so something was drawn outside it"
        );
    }

    for (width, height) in [(1u16, 1u16), (3, 2), (10, 3), (20, 5), (60, 20)] {
        let rows = screen(&app, width, height);
        assert_eq!(
            rows.len(),
            usize::from(height),
            "{width}x{height}: the frame must be exactly the terminal"
        );
    }

    let rows = screen(&app, 20, 16);
    let help = help_region(&rows, 20, 16);
    assert!(
        help.contains("char"),
        "at 20 columns the help is clipped, not dropped: {rows:?}"
    );
    // The status line truncates rather than wrapping (it is one row), so at this width the
    // session id itself is off the end: what has to survive is the row.
    assert!(
        rows.last().unwrap().contains("session"),
        "the status line must survive a narrow terminal: {:?}",
        rows.last().unwrap()
    );
}

// ---------------------------------------------------------------------------
// B26 (a): the TUI could only ever send one turn
// ---------------------------------------------------------------------------

/// `ACCEPTANCE.md` §13's B26 regression test, by name.
///
/// The first freeze let `Enter` submit only from `Status::Idle`, and `apply_event(Done)`
/// set `Status::Done` with nothing anywhere returning to `Idle`. The code was faithful to
/// the table and the table was wrong, so the symptom was not a wrong answer but a TUI
/// that accepted one prompt and then ignored every key that mattered — which looks like
/// working software for exactly as long as it takes to type a second question.
#[test]
fn a_second_turn_can_be_submitted_after_the_first_completes() {
    let mut app = app_with_prompt();

    assert_eq!(reduce(&mut app, Input::Enter), Action::Submit);
    apply_event(&mut app, &done("the first answer"));
    assert_eq!(
        app.status,
        Status::Done,
        "the stream's terminal event is what ends the turn"
    );

    app.prompt = "second question".to_string();
    app.cursor = app.prompt.chars().count();
    assert_eq!(
        reduce(&mut app, Input::Enter),
        Action::Submit,
        "`Enter` must submit whenever the status is not `Running`, not only from `Idle`"
    );
    assert_eq!(app.status, Status::Running);
    assert_eq!(
        app.transcript,
        vec![
            Entry::User("hello".to_string()),
            Entry::Assistant("the first answer".to_string()),
            Entry::User("second question".to_string()),
        ],
        "both questions and the first answer are on the transcript, in order"
    );
}

/// The other half of the same lifecycle row: `DESIGN.md` §12's table sends
/// `Done`/`Failed` to `Running` on `Submit`, so a turn that failed can be retried without
/// restarting the TUI.
#[test]
fn a_turn_can_be_submitted_again_after_a_failure() {
    let mut app = app_with_prompt();
    app.status = Status::Failed("codex exited 1".to_string());

    assert_eq!(reduce(&mut app, Input::Enter), Action::Submit);
    assert_eq!(app.status, Status::Running);
}

/// The refusal that must survive the fix: a submission while a turn is in flight would
/// send a second prompt into a vendor CLI that is already running one, which is real
/// subscription quota.
#[test]
fn enter_is_still_refused_while_a_turn_is_running() {
    let mut app = app_with_prompt();
    app.status = Status::Running;

    assert_eq!(reduce(&mut app, Input::Enter), Action::None);
    assert_eq!(app.status, Status::Running);
    assert_eq!(app.prompt, "hello", "the draft is kept");
    assert!(
        app.transcript.is_empty(),
        "nothing was sent, nothing recorded"
    );
}

/// The status predicate the fix rests on, asserted directly: `Done` and `Failed` are
/// finished turns, not running ones, and a `Status` that answered `is_running` for either
/// would put the single-turn bug back on the very next `Done`.
#[test]
fn only_running_counts_as_a_turn_in_flight() {
    assert!(Status::Running.is_running());
    assert!(!Status::Idle.is_running());
    assert!(!Status::Done.is_running());
    assert!(!Status::Failed("anything".to_string()).is_running());
}

// ---------------------------------------------------------------------------
// B29: a large scroll offset panicked inside ratatui
// ---------------------------------------------------------------------------

/// `ACCEPTANCE.md` §13's B29 regression test, in the shape its entry asks for: "a sweep
/// over `App::scroll` including `u16::MAX`, at several sizes".
///
/// `render` used to hand `App::scroll` straight to `Paragraph::scroll`, and ratatui 0.29
/// then computes `offset + area.height` and panics with `attempt to add with overflow`
/// (`paragraph.rs:483`). On a 60×20 terminal the measured boundary was last-OK 65519,
/// first-panic 65520. Reaching it takes ~65 520 `ScrollDown` presses, so a human cannot —
/// but `App::scroll` is a public field, `src/lib.rs` denies panics, and S10 claims "a
/// scroll past the end is an empty window rather than a panic". The claim was false; the
/// clamp in `render` is what makes it true.
#[test]
fn a_scroll_offset_larger_than_ratatui_can_add_is_an_empty_window_not_a_panic() {
    const SIZES: [(u16, u16); 5] = [(60, 20), (40, 12), (80, 24), (200, 60), (24, 8)];
    // 65 519 and 65 520 are the measured boundary at 60×20, named so the numbers in
    // the bug log stay meaningful if someone re-measures.
    const OFFSETS: [u16; 7] = [0, 1, 16, 65_519, 65_520, u16::MAX - 1, u16::MAX];

    for (width, height) in SIZES {
        for offset in OFFSETS {
            let mut app = app_with_lines(30);
            app.scroll = offset;

            let rows = screen(&app, width, height);

            assert_eq!(
                rows.len(),
                usize::from(height),
                "{width}x{height} at scroll {offset}: the frame lost rows"
            );
            assert!(
                rows.last().unwrap().contains("session"),
                "{width}x{height} at scroll {offset}: the status line must survive any offset"
            );
            if offset >= 65_519 {
                assert!(
                    !flat_text(&rows).contains("line-"),
                    "{width}x{height} at scroll {offset}: a scroll past the end is an empty \
                     window, not a panic and not a reflowed jumble: {rows:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// B26 (c): `Esc` erased a failure reason
// ---------------------------------------------------------------------------

/// `ACCEPTANCE.md` §13's B26 regression test, by name.
///
/// `Esc` used to set `Idle` whenever anything was cancelled — including when the only
/// thing to cancel was the picker. So a failed turn whose reason was on the status line
/// lost that reason the moment the user closed the overlay, before they could read it.
/// Closing an overlay is not a status change (`DESIGN.md` §12).
#[test]
fn a_failed_reason_survives_closing_the_picker() {
    let mut app = app_with_prompt();
    app.status = Status::Failed("codex exited 1".to_string());
    app.picker = Some(picker_with_two_backends(0));

    assert_eq!(reduce(&mut app, Input::Esc), Action::Cancel);
    assert!(app.picker.is_none(), "`Esc` still closes the picker");
    assert_eq!(
        app.status,
        Status::Failed("codex exited 1".to_string()),
        "the reason the user has not read yet must survive the overlay closing"
    );
    assert!(!app.should_quit, "a cancel is not a quit");
}

/// The other half, and the reason the fix is a condition rather than a deletion: a turn
/// that was actually running still returns to `Idle` on `Esc`, or the interface would sit
/// in `Running` forever after an abort.
#[test]
fn cancelling_a_running_turn_still_returns_to_idle() {
    let mut app = app_with_prompt();
    app.status = Status::Running;

    assert_eq!(reduce(&mut app, Input::Esc), Action::Cancel);
    assert_eq!(app.status, Status::Idle);
}

/// A `Done` status is left alone for the same reason a `Failed` one is: the picker is an
/// overlay, and `Done` is a fact about the turn.
#[test]
fn closing_the_picker_leaves_a_done_status_alone() {
    let mut app = app_with_prompt();
    app.status = Status::Done;
    app.picker = Some(picker_with_two_backends(1));

    assert_eq!(reduce(&mut app, Input::Esc), Action::Cancel);
    assert_eq!(app.status, Status::Done);
}

// ---------------------------------------------------------------------------
// B26 (d): a second `Tab` discarded the catalog
// ---------------------------------------------------------------------------

/// `ACCEPTANCE.md` §13's B26 regression test, by name.
///
/// `Tab` opens the picker **empty**, for the loop to fill from a catalog that is read
/// asynchronously (`src/tui.rs`'s `fill_picker`). A second `Tab` used to reset it to
/// `Picker::default()`, so a user who tapped `Tab` twice watched every row vanish with
/// nothing on screen to explain it — and the loop's next delivery only arrives once.
#[test]
fn tab_twice_does_not_discard_the_catalog() {
    let mut app = app_with_prompt();

    assert_eq!(reduce(&mut app, Input::Tab), Action::OpenPicker);
    assert!(
        app.picker.as_ref().expect("opened").items.is_empty(),
        "opened empty, for the caller to fill"
    );

    // The caller's delivery, exactly as the loop delivers it.
    app.picker = Some(picker_with_two_backends(1));

    assert_eq!(
        reduce(&mut app, Input::Tab),
        Action::None,
        "a second `Tab` must be a no-op"
    );

    let picker = app.picker.as_ref().expect("still open");
    assert_eq!(
        picker.items.len(),
        2,
        "the catalog the caller filled in must survive a repeated keypress"
    );
    assert_eq!(
        picker.selected, 1,
        "and the highlight with it — a reset would also move the user's cursor"
    );
}

// ---------------------------------------------------------------------------
// The picker
// ---------------------------------------------------------------------------

/// The constructor is the only place a catalog becomes a picker, and it is pure: no
/// catalog read, no clock. Determinism across two calls is the half of that a test can
/// see, and it is the half that matters here — the loop compares nothing, but a picker
/// built twice from one catalog must not differ.
#[test]
fn a_catalog_becomes_one_row_per_model_and_two_calls_are_identical() {
    let models = [
        Model::plain("gpt-6-luna", "GPT-6 Luna"),
        Model::plain("gemini-3.8-flash", "gemini-3.8-flash"),
        Model::plain("no-display", ""),
    ];

    let picker = Picker::from_models("codex", &models);

    assert_eq!(
        picker,
        Picker::from_models("codex", &models),
        "an offline, clock-free constructor must be deterministic"
    );
    assert_eq!(
        picker.selected, 0,
        "the first row is where a fresh picker starts"
    );
    assert_eq!(picker.items.len(), 3, "one row per model, in catalog order");
    assert_eq!(
        picker.items[0],
        PickerItem {
            backend: "codex".to_string(),
            model: "gpt-6-luna".to_string(),
            label: "codex/gpt-6-luna (GPT-6 Luna)".to_string(),
        },
        "the label names the backend as well as the model, because a row sets both"
    );
    assert_eq!(
        picker.items[1].label, "codex/gemini-3.8-flash",
        "a display name that adds nothing is not repeated in brackets"
    );
    assert_eq!(
        picker.items[2].label, "codex/no-display",
        "an empty display name is not rendered as an empty bracket"
    );
}

/// `DESIGN.md` §12: `PickerAccept` sets **both** `App::backend` and `App::model`. The
/// rows are on two different backends, so a reducer that copied only the model would
/// change the model and leave the subscription behind — a switch that silently does not
/// switch.
#[test]
fn choosing_a_row_sets_the_backend_as_well_as_the_model() {
    let mut app = app_with_prompt();
    app.backend = "agy".to_string();
    app.model = Some("gemini-3.8-flash".to_string());
    app.picker = Some(picker_with_two_backends(0));

    assert_eq!(reduce(&mut app, Input::Down), Action::PickerNext);
    assert_eq!(reduce(&mut app, Input::Enter), Action::PickerAccept);

    assert!(app.picker.is_none(), "accepting closes the picker");
    assert_eq!(
        app.backend, "agy",
        "the row's backend, not the one already set"
    );
    assert_eq!(app.model.as_deref(), Some("gemini-3.8-flash"));
    assert_eq!(app.target(), "agy/gemini-3.8-flash");
}

/// A catalog that came back empty is a state the loop can deliver (a backend with no
/// models to list, or a catalog read that returned nothing). The picker stays usable and
/// empty, and closing it must not invent a model.
#[test]
fn an_empty_catalog_leaves_a_usable_picker_that_accepting_cannot_change() {
    let mut app = app_with_prompt();

    app.picker = Some(Picker::from_models("codex", &[]));
    let picker = app.picker.as_ref().expect("open");
    assert!(picker.items.is_empty());
    assert!(picker.current().is_none(), "there is no highlighted row");
    assert_eq!(picker.selected, 0);

    // Moving the highlight in an empty picker is what a user's `Up`/`Down` does while the
    // catalog is still in flight, and it must not index anything.
    assert_eq!(reduce(&mut app, Input::Down), Action::PickerNext);
    assert_eq!(reduce(&mut app, Input::Up), Action::PickerPrev);
    assert_eq!(app.picker.as_ref().expect("open").selected, 0);

    assert_eq!(reduce(&mut app, Input::Enter), Action::PickerAccept);
    assert!(app.picker.is_none());
    assert_eq!(app.backend, "codex", "no row means no change of backend");
    assert_eq!(
        app.model.as_deref(),
        Some("gpt-6-luna"),
        "picking nothing must not clear the model"
    );
}
