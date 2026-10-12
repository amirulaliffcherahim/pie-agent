//! M6 stage A gate: the reducer, and the fold that turns a backend's events into
//! a transcript.
//!
//! `planning/ACCEPTANCE.md` §9 names `cargo test --test tui_app` as stage A's
//! gate and says why a stage gate must name the stage's own binary; the contract
//! is `planning/DESIGN.md` §12's keybinding table and `apply_event` behaviour
//! table, enforced here row by row. The property that makes that possible is that
//! no terminal appears anywhere in the surface under test — a `KeyEvent` in
//! `reduce`'s signature would have dragged one into every assertion below.
//!
//! The table-driven tests are built so an `Input` variant **cannot** be added
//! without a decision: the two `documented_action*` helpers are exhaustive
//! matches with no wildcard, so a new variant stops this file *compiling* rather
//! than quietly going untested. A mere array of rows would not have that
//! property, which is the difference between a table and a checklist.
//!
//! Nothing here spawns a process, opens a socket or needs a vendor CLI. The
//! `apply_event` cases build `StreamEvent`s of the shape the backends emit (the
//! recorded ones live in `tests/fixtures/`), so the suite is offline and spends
//! no subscription quota.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pie::provider::{StreamEvent, ToolState, Usage};
use pie::tui::app::{apply_event, reduce, Action, App, Entry, Input, Picker, PickerItem, Status};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Every `Input` variant, once. `Ctrl('q')` stands in for "any other `Ctrl`",
/// which is a documented row of its own.
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

/// The documented `Action` for each input in the ordinary state: `Idle`, a
/// submittable prompt, no picker (`DESIGN.md` §12).
fn documented_action(input: Input) -> Action {
    match input {
        Input::Char(_) | Input::Backspace | Input::Left | Input::Right => Action::None,
        Input::Enter => Action::Submit,
        Input::Esc => Action::Quit,
        Input::Up => Action::ScrollUp,
        Input::Down => Action::ScrollDown,
        Input::Tab => Action::OpenPicker,
        Input::Ctrl('c') => Action::Quit,
        Input::Ctrl(_) => Action::None,
    }
}

/// The documented `Action` for each input while the picker is modal. `Up`/`Down`
/// change meaning, and `Ctrl('c')` does not (`DESIGN.md` §12).
///
/// `Tab` is `None` here, not `OpenPicker`: the picker is already open, and
/// re-opening it would discard the catalog the caller just filled in. Stage A
/// wrote this helper against the **first** version of §12 and its table was
/// corrected afterwards (bug B26), which is why the row reads the way it does —
/// and why the corrected row has its own named regression test in
/// `tests/tui_keys.rs` rather than existing only as a row here.
fn documented_action_with_picker_open(input: Input) -> Action {
    match input {
        Input::Enter => Action::PickerAccept,
        Input::Esc => Action::Cancel,
        Input::Up => Action::PickerPrev,
        Input::Down => Action::PickerNext,
        Input::Tab => Action::None,
        Input::Ctrl('c') => Action::Quit,
        Input::Char(_) | Input::Backspace | Input::Left | Input::Right | Input::Ctrl(_) => {
            Action::None
        }
    }
}

/// An app with a submittable prompt already typed, so the "otherwise" row of the
/// table is the one under test.
fn app() -> App {
    let mut app = App::new("codex", Some("gpt-6-luna".to_string()), "s-1-2");
    app.prompt = "hello".to_string();
    app.cursor = 5;
    app
}

fn idle_app() -> App {
    App::new("codex", None, "s-1-2")
}

/// Type `text` one `Char` at a time, through the reducer rather than by assigning
/// `prompt`: the point is to exercise the insert path the user drives.
fn typed(text: &str) -> App {
    let mut app = idle_app();
    for c in text.chars() {
        reduce(&mut app, Input::Char(c));
    }
    app
}

/// Two rows on **different backends**, because a picker that only ever set the
/// model would pass a single-backend test (`ACCEPTANCE.md` §9 stage D).
fn picker_with_two_backends(selected: usize) -> Picker {
    Picker {
        items: vec![
            PickerItem {
                backend: "codex".to_string(),
                model: "gpt-6-luna".to_string(),
                label: "codex/gpt-6-luna".to_string(),
            },
            PickerItem {
                backend: "agy".to_string(),
                model: "gemini-3.8-flash".to_string(),
                label: "agy/gemini-3.8-flash".to_string(),
            },
        ],
        selected,
    }
}

/// Answer text already on screen, in order.
fn assistant_texts(app: &App) -> Vec<&str> {
    app.transcript
        .iter()
        .filter_map(|entry| match entry {
            Entry::Assistant(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_steps(app: &App) -> Vec<&Entry> {
    app.transcript
        .iter()
        .filter(|entry| matches!(entry, Entry::Tool { .. }))
        .collect()
}

// ---------------------------------------------------------------------------
// The keybinding table
// ---------------------------------------------------------------------------

/// Every row of `DESIGN.md` §12's table, in the state the row describes.
///
/// The `assert_eq!` message names the input, so a failure says *which* row drifted
/// rather than only that one did.
#[test]
fn every_input_maps_to_its_documented_action() {
    for input in EVERY_INPUT {
        let mut app = app();
        let action = reduce(&mut app, input);
        assert_eq!(
            action,
            documented_action(input),
            "{input:?} does not map to its documented action"
        );
    }
}

/// The picker is modal, so the same keys mean different things while it is open.
/// Without this table, a `reduce` that ignored `app.picker` for `Up`/`Down` would
/// pass every other test in this file.
#[test]
fn every_input_maps_to_its_documented_action_while_the_picker_is_open() {
    for input in EVERY_INPUT {
        let mut app = app();
        app.picker = Some(picker_with_two_backends(0));
        let action = reduce(&mut app, input);
        assert_eq!(
            action,
            documented_action_with_picker_open(input),
            "{input:?} does not map to its documented action while the picker is open"
        );
    }
}

/// The ordinary-state table above proves `Esc` is `Quit` when there is nothing to
/// cancel; this is the other half of the same row.
#[test]
fn esc_cancels_instead_of_quitting_while_a_turn_is_running() {
    let mut app = app();
    app.status = Status::Running;

    assert_eq!(reduce(&mut app, Input::Esc), Action::Cancel);
    assert_eq!(app.status, Status::Idle);
    assert!(!app.should_quit);
}

/// The row says `Esc` closes the picker as well as cancelling a turn, so the
/// picker half is asserted on its own.
#[test]
fn esc_closes_the_picker_and_returns_the_status_to_idle() {
    let mut app = app();
    app.picker = Some(picker_with_two_backends(1));

    assert_eq!(reduce(&mut app, Input::Esc), Action::Cancel);
    assert!(app.picker.is_none());
    assert_eq!(app.status, Status::Idle);
    assert!(!app.should_quit);
}

/// `Ctrl('c')` is the escape hatch, so it must work in every state rather than
/// only in the idle one — including while a turn is running and while the picker
/// is modal, where `Esc` means `Cancel` and cannot serve.
#[test]
fn ctrl_c_quits_from_every_state() {
    let running = Status::Running;
    let states: [(&str, Option<Picker>, Status); 4] = [
        ("idle", None, Status::Idle),
        ("running", None, running),
        (
            "picker open",
            Some(picker_with_two_backends(0)),
            Status::Idle,
        ),
        (
            "picker open after a failure",
            Some(picker_with_two_backends(0)),
            Status::Failed("codex exited 1".to_string()),
        ),
    ];

    for (name, picker, status) in states {
        let mut app = app();
        app.picker = picker;
        app.status = status;
        assert_eq!(reduce(&mut app, Input::Ctrl('c')), Action::Quit, "{name}");
        assert!(app.should_quit, "{name}");
    }
}

#[test]
fn any_other_ctrl_key_does_nothing_at_all() {
    for c in ['q', 'a', 'z', ' '] {
        let mut app = app();
        assert_eq!(reduce(&mut app, Input::Ctrl(c)), Action::None, "Ctrl({c})");
        assert!(!app.should_quit);
        assert_eq!(app.prompt, "hello", "Ctrl({c}) must not edit the prompt");
        assert_eq!(app.cursor, 5);
    }
}

// ---------------------------------------------------------------------------
// Typing: the cursor is counted in characters
// ---------------------------------------------------------------------------

#[test]
fn char_inserts_at_the_cursor_not_only_at_the_end() {
    let mut app = idle_app();
    app.prompt = "heo".to_string();
    app.cursor = 2;

    assert_eq!(reduce(&mut app, Input::Char('l')), Action::None);
    assert_eq!(app.prompt, "helo");
    assert_eq!(app.cursor, 3);
}

#[test]
fn char_inserts_into_the_middle_of_a_multi_byte_prompt() {
    let mut app = idle_app();
    app.prompt = "日本語".to_string();
    app.cursor = 1;

    reduce(&mut app, Input::Char('x'));

    assert_eq!(app.prompt, "日x本語");
    assert_eq!(app.cursor, 2);
}

#[test]
fn backspace_deletes_the_character_before_the_cursor_and_stops_at_the_start() {
    let mut app = idle_app();
    app.prompt = "hello".to_string();
    app.cursor = 3;

    assert_eq!(reduce(&mut app, Input::Backspace), Action::None);
    assert_eq!(app.prompt, "helo", "the character before the cursor goes");
    assert_eq!(app.cursor, 2);

    app.cursor = 0;
    assert_eq!(reduce(&mut app, Input::Backspace), Action::None);
    assert_eq!(app.prompt, "helo", "there is nothing before the cursor");
    assert_eq!(app.cursor, 0);
}

#[test]
fn left_and_right_move_the_cursor_and_clamp_to_the_prompt() {
    let mut app = idle_app();
    app.prompt = "abc".to_string();
    app.cursor = 0;

    assert_eq!(reduce(&mut app, Input::Left), Action::None);
    assert_eq!(
        app.cursor, 0,
        "the start of the prompt is the end of the line"
    );

    reduce(&mut app, Input::Right);
    reduce(&mut app, Input::Right);
    assert_eq!(app.cursor, 2);

    reduce(&mut app, Input::Left);
    assert_eq!(app.cursor, 1);

    for _ in 0..5 {
        reduce(&mut app, Input::Right);
    }
    assert_eq!(
        app.cursor, 3,
        "the end of the prompt is the end of the line"
    );
}

/// The classic bug this file exists to catch. `Char('é')` is one character and two
/// bytes, so a cursor taken from (or clamped by) `prompt.len()` is off by one — and
/// on `日本語` it is off by six. Removing half a character is not possible, so the
/// failure mode is a wrong edit or a panic on a non-boundary offset.
#[test]
fn a_multi_byte_character_is_removed_as_one_unit() {
    let mut app = typed("é");
    assert_eq!(app.prompt, "é");
    assert_eq!(app.cursor, 1, "one character, not two bytes");
    assert_eq!(
        app.prompt.len(),
        2,
        "the byte length is what a bug would use"
    );

    assert_eq!(reduce(&mut app, Input::Backspace), Action::None);
    assert_eq!(app.prompt, "", "the whole character goes, not half of it");
    assert_eq!(app.cursor, 0);
}

/// `Left` from the end of a two-byte character must land *before* it, so the next
/// insert belongs at the start. With a byte cursor this would either insert
/// mid-character or panic on the offset, which is the "broken offset" the stage
/// asks for.
#[test]
fn left_from_the_end_of_a_multi_byte_prompt_lands_on_a_character_boundary() {
    let mut app = typed("é");
    reduce(&mut app, Input::Left);
    assert_eq!(app.cursor, 0);

    reduce(&mut app, Input::Char('x'));
    assert_eq!(app.prompt, "xé");
    assert!(app.prompt.is_char_boundary(1));

    reduce(&mut app, Input::Left);
    reduce(&mut app, Input::Left);
    assert_eq!(app.cursor, 0, "clamped, not negative");
}

/// Every edit is driven against a `Vec<char>` model of the same prompt, so the
/// assertion is about the whole edit sequence rather than one hand-written case.
/// The `cursor == model.len()` comparison is the byte-index bug stated directly:
/// on `日本語` the two differ by six.
#[test]
fn a_prompt_of_any_alphabet_edits_identically_and_the_cursor_never_escapes_it() {
    for prompt in [
        "ascii",
        "é",
        "日本語",
        "aé日b",
        "🙂ok",
        "  é  ",
        "naïve—dash",
    ] {
        let mut app = typed(prompt);
        let mut model: Vec<char> = prompt.chars().collect();
        let mut cursor = model.len();

        assert_eq!(app.prompt, prompt);
        assert_eq!(app.cursor, cursor, "typing {prompt:?}");

        // Two lefts, then delete the character before the cursor: an edit in the
        // middle, where a byte offset and a character offset disagree.
        reduce(&mut app, Input::Left);
        reduce(&mut app, Input::Left);
        cursor = cursor.saturating_sub(2);
        reduce(&mut app, Input::Backspace);
        if cursor > 0 {
            model.remove(cursor - 1);
            cursor -= 1;
        }
        assert_eq!(
            app.prompt,
            model.iter().collect::<String>(),
            "editing {prompt:?}"
        );
        assert_eq!(app.cursor, cursor, "editing {prompt:?}");

        // Backspace all the way out: the cursor walks to the end of the line, then
        // deletes everything in front of it and stops at the start rather than
        // running past it. `Right` at the end is a no-op, so the extra presses are
        // the clamp absorbing them.
        for _ in 0..prompt.chars().count() {
            reduce(&mut app, Input::Right);
            assert!(
                app.cursor <= app.prompt.chars().count(),
                "the cursor escaped the prompt ({:?})",
                app.prompt
            );
        }
        assert_eq!(app.cursor, app.prompt.chars().count());

        for _ in 0..prompt.chars().count() {
            reduce(&mut app, Input::Backspace);
        }
        assert_eq!(app.prompt, "", "emptying {prompt:?}");
        assert_eq!(app.cursor, 0, "emptying {prompt:?}");
    }
}

// ---------------------------------------------------------------------------
// Enter: submit, and the two refusals
// ---------------------------------------------------------------------------

#[test]
fn enter_on_an_empty_prompt_is_refused() {
    let mut app = idle_app();

    assert_eq!(reduce(&mut app, Input::Enter), Action::None);
    assert_eq!(app.status, Status::Idle);
    assert!(
        app.transcript.is_empty(),
        "nothing was asked, nothing recorded"
    );
}

/// The one keystroke that costs real subscription quota if it slips through.
#[test]
fn enter_on_a_whitespace_only_prompt_is_refused() {
    let mut app = idle_app();
    app.prompt = "  \n\t ".to_string();
    app.cursor = app.prompt.chars().count();

    assert_eq!(reduce(&mut app, Input::Enter), Action::None);
    assert_eq!(app.status, Status::Idle);
    assert!(app.transcript.is_empty());
    assert_eq!(app.prompt, "  \n\t ", "the prompt is not cleared either");
}

/// `Submit` clears the prompt and starts the turn, and the question is recorded
/// before it goes — `Action::Submit` carries no payload, so this is the only place
/// the text can be captured (`DESIGN.md` §12's table names only "prompt cleared;
/// `Status::Running`"; see the note on `reduce` in `src/tui/app.rs`).
#[test]
fn enter_on_a_real_prompt_submits_it() {
    let mut app = idle_app();
    app.prompt = "explain the reducer".to_string();
    app.cursor = app.prompt.chars().count();

    assert_eq!(reduce(&mut app, Input::Enter), Action::Submit);
    assert_eq!(app.status, Status::Running);
    assert_eq!(app.prompt, "");
    assert_eq!(app.cursor, 0);
    assert_eq!(
        app.transcript,
        vec![Entry::User("explain the reducer".to_string())]
    );
}

/// `Enter` is refused unless the turn has finished, so a second prompt cannot be
/// sent into a vendor CLI that is already running one.
#[test]
fn enter_while_a_turn_is_running_is_ignored() {
    let mut app = idle_app();
    app.prompt = "second question".to_string();
    app.cursor = app.prompt.chars().count();
    app.status = Status::Running;

    assert_eq!(reduce(&mut app, Input::Enter), Action::None);
    assert_eq!(app.status, Status::Running);
    assert_eq!(app.prompt, "second question", "the draft is kept");
    assert!(app.transcript.is_empty());
}

// ---------------------------------------------------------------------------
// Tab and the picker
// ---------------------------------------------------------------------------

/// `reduce` opens the picker **empty** on purpose: filling it needs the backend's
/// catalog, which is I/O, and I/O in the reducer is what the layer split removes.
#[test]
fn tab_opens_an_empty_picker_for_the_caller_to_fill() {
    let mut app = app();

    assert_eq!(reduce(&mut app, Input::Tab), Action::OpenPicker);
    let picker = app.picker.expect("the picker is open");
    assert!(
        picker.items.is_empty(),
        "opened empty, for the caller to fill"
    );
    assert_eq!(picker.selected, 0);
}

#[test]
fn up_and_down_move_the_picker_highlight_and_clamp_at_both_ends() {
    let mut app = app();
    app.picker = Some(picker_with_two_backends(0));

    assert_eq!(reduce(&mut app, Input::Down), Action::PickerNext);
    assert_eq!(app.picker.as_ref().unwrap().selected, 1);

    assert_eq!(reduce(&mut app, Input::Down), Action::PickerNext);
    assert_eq!(app.picker.as_ref().unwrap().selected, 1, "the last row");

    assert_eq!(reduce(&mut app, Input::Up), Action::PickerPrev);
    assert_eq!(app.picker.as_ref().unwrap().selected, 0);

    assert_eq!(reduce(&mut app, Input::Up), Action::PickerPrev);
    assert_eq!(app.picker.as_ref().unwrap().selected, 0, "the first row");
}

/// `Enter` with the picker open must set the *backend* as well as the model.
/// `PickerItem` exists precisely because a `Vec<String>` of labels could not
/// (`DESIGN.md` §12), so the rows below are on different backends and the starting
/// state is the second one: a reducer that copied only the model would fail here
/// and nowhere else.
#[test]
fn enter_takes_both_the_backend_and_the_model_from_the_highlighted_row() {
    let mut app = idle_app();
    app.backend = "agy".to_string();
    app.model = Some("gemini-3.8-flash".to_string());
    app.picker = Some(picker_with_two_backends(0));

    assert_eq!(reduce(&mut app, Input::Enter), Action::PickerAccept);
    assert!(app.picker.is_none(), "accepting closes the picker");
    assert_eq!(app.backend, "codex");
    assert_eq!(app.model.as_deref(), Some("gpt-6-luna"));
    assert_eq!(app.target(), "codex/gpt-6-luna");
}

#[test]
fn enter_takes_the_row_the_highlight_is_actually_on() {
    let mut app = idle_app();
    app.picker = Some(picker_with_two_backends(0));

    reduce(&mut app, Input::Down);
    assert_eq!(reduce(&mut app, Input::Enter), Action::PickerAccept);

    assert_eq!(app.backend, "agy");
    assert_eq!(app.model.as_deref(), Some("gemini-3.8-flash"));
}

/// The caller fills the picker asynchronously, so `Enter` can arrive at a picker
/// with no rows. Closing without changing the target is the only safe answer:
/// picking "nothing" must not silently clear the model.
#[test]
fn enter_on_an_empty_picker_closes_it_and_leaves_the_target_alone() {
    let mut app = app();
    app.picker = Some(Picker::default());

    assert_eq!(reduce(&mut app, Input::Enter), Action::PickerAccept);
    assert!(app.picker.is_none());
    assert_eq!(app.backend, "codex");
    assert_eq!(app.model.as_deref(), Some("gpt-6-luna"));
}

// ---------------------------------------------------------------------------
// Scroll
// ---------------------------------------------------------------------------

#[test]
fn up_and_down_scroll_the_transcript_when_the_picker_is_closed() {
    let mut app = app();

    assert_eq!(reduce(&mut app, Input::Down), Action::ScrollDown);
    assert_eq!(app.scroll, 1);
    assert_eq!(reduce(&mut app, Input::Down), Action::ScrollDown);
    assert_eq!(app.scroll, 2);

    assert_eq!(reduce(&mut app, Input::Up), Action::ScrollUp);
    assert_eq!(app.scroll, 1);
}

/// `u16` underflow panics in debug, and `lib.rs` denies panics on external input
/// for the same reason, so the floor is asserted rather than assumed.
#[test]
fn scrolling_up_never_goes_below_the_top() {
    let mut app = app();

    assert_eq!(reduce(&mut app, Input::Up), Action::ScrollUp);
    assert_eq!(app.scroll, 0);
    assert_eq!(reduce(&mut app, Input::Up), Action::ScrollUp);
    assert_eq!(app.scroll, 0);
}

// ---------------------------------------------------------------------------
// apply_event
// ---------------------------------------------------------------------------

fn init(conversation_id: Option<&str>, tools: &[&str]) -> StreamEvent {
    StreamEvent::Init {
        conversation_id: conversation_id.map(str::to_string),
        cwd: None,
        tools: tools.iter().map(|tool| tool.to_string()).collect(),
    }
}

#[test]
fn init_records_the_vendor_id_and_pushes_exactly_one_notice() {
    let mut app = idle_app();

    apply_event(
        &mut app,
        &init(
            Some("8f3d9b2a-4c17-4e6a-9f0b-2d5c1a7e6b93"),
            &["read", "bash", "write"],
        ),
    );

    assert_eq!(
        app.vendor_id.as_deref(),
        Some("8f3d9b2a-4c17-4e6a-9f0b-2d5c1a7e6b93"),
        "the id --resume would need, kept rather than shown only in --json"
    );
    assert_eq!(app.transcript.len(), 1, "one notice, not one per field");
    let Entry::Notice(text) = &app.transcript[0] else {
        panic!("init must push a Notice, got {:?}", app.transcript[0]);
    };
    assert!(
        text.contains("8f3d9b2a-4c17-4e6a-9f0b-2d5c1a7e6b93"),
        "the notice names the conversation: {text:?}"
    );
    assert!(
        text.contains("3 tools"),
        "the notice names the tool count: {text:?}"
    );
}

/// A resumed turn's `Init` can report no conversation id. Overwriting a known id
/// with `None` would lose exactly the value the field exists to keep, and the loss
/// would only show up at the next `--resume`.
#[test]
fn an_init_that_reports_no_conversation_id_does_not_erase_the_one_already_known() {
    let mut app = idle_app();
    apply_event(&mut app, &init(Some("kept"), &[]));

    apply_event(&mut app, &init(None, &[]));

    assert_eq!(app.vendor_id.as_deref(), Some("kept"));
    assert_eq!(app.transcript.len(), 2, "each init reports itself");
}

/// One entry per delta would grow the transcript without bound on a long answer,
/// and would also destroy the scrollback's relationship to the answer's shape.
#[test]
fn consecutive_text_deltas_append_to_one_assistant_entry() {
    let mut app = idle_app();

    apply_event(&mut app, &StreamEvent::TextDelta("Hello".to_string()));
    apply_event(&mut app, &StreamEvent::TextDelta(", world".to_string()));
    apply_event(&mut app, &StreamEvent::TextDelta("!".to_string()));

    assert_eq!(app.transcript.len(), 1, "one entry, growing");
    assert_eq!(
        app.transcript,
        vec![Entry::Assistant("Hello, world!".to_string())]
    );
}

/// The tool step interrupts the answer, so the next delta cannot append to text it
/// is no longer contiguous with — otherwise the transcript would read as if the
/// tool ran mid-sentence with no break.
#[test]
fn text_after_a_tool_step_starts_a_new_assistant_entry() {
    let mut app = idle_app();

    apply_event(&mut app, &StreamEvent::TextDelta("before".to_string()));
    apply_event(
        &mut app,
        &StreamEvent::ToolStep {
            name: "bash".to_string(),
            state: ToolState::Done,
            info: Some("cargo test".to_string()),
        },
    );
    apply_event(&mut app, &StreamEvent::TextDelta("after".to_string()));

    assert_eq!(assistant_texts(&app), vec!["before", "after"]);
    assert_eq!(app.transcript.len(), 3);
}

/// Both halves of the row: the summary list the status line keeps, and the
/// transcript row stage B attaches `VENDOR_TOOL_NOTE` to. A reducer that filled
/// only one of them would still satisfy a test that looked at only one of them.
#[test]
fn a_tool_step_lands_in_the_tool_list_and_in_the_transcript() {
    let mut app = idle_app();

    apply_event(
        &mut app,
        &StreamEvent::ToolStep {
            name: "bash".to_string(),
            state: ToolState::Active,
            info: None,
        },
    );

    assert_eq!(
        app.tool_events,
        vec![("bash".to_string(), ToolState::Active)]
    );
    assert_eq!(
        tool_steps(&app),
        vec![&Entry::Tool {
            name: "bash".to_string(),
            state: ToolState::Active,
        }]
    );
}

/// Asserted rather than assumed, because the failure is silent: a `Usage` assigned
/// wholesale reads as success and shows a smaller number than the backend reported.
#[test]
fn usage_merges_field_wise_so_a_later_partial_event_does_not_zero_an_earlier_field() {
    let mut app = idle_app();

    apply_event(
        &mut app,
        &StreamEvent::Usage(Usage {
            input_tokens: 100,
            thinking_tokens: 7,
            ..Usage::default()
        }),
    );
    apply_event(
        &mut app,
        &StreamEvent::Usage(Usage {
            input_tokens: 120,
            output_tokens: 5,
            ..Usage::default()
        }),
    );

    let usage = app.last_usage.expect("usage was reported");
    assert_eq!(usage.input_tokens, 120, "the later value replaces");
    assert_eq!(usage.output_tokens, 5, "the later field arrives");
    assert_eq!(usage.thinking_tokens, 7, "the omitted field survives");
}

/// The twin of B21's fix on the backend side: a terminal event must not duplicate
/// what has already been streamed. Duplication is invisible in a screenshot and
/// doubles the length of a long answer.
#[test]
fn done_after_streamed_deltas_does_not_duplicate_the_answer() {
    let mut app = idle_app();
    app.status = Status::Running;

    apply_event(&mut app, &StreamEvent::TextDelta("Hello".to_string()));
    apply_event(&mut app, &StreamEvent::TextDelta(" world".to_string()));
    apply_event(
        &mut app,
        &StreamEvent::Done {
            text: "Hello world".to_string(),
            status: "SUCCESS".to_string(),
        },
    );

    assert_eq!(app.status, Status::Done);
    assert_eq!(assistant_texts(&app), vec!["Hello world"]);
}

/// The other half: some backends send the whole answer only in the terminal event.
/// Suppressing that text because the transcript is non-empty would render a
/// successful turn as nothing at all.
#[test]
fn done_with_nothing_streamed_pushes_the_terminal_text() {
    let mut app = idle_app();
    app.status = Status::Running;

    apply_event(
        &mut app,
        &StreamEvent::Done {
            text: "the whole answer".to_string(),
            status: "SUCCESS".to_string(),
        },
    );

    assert_eq!(app.status, Status::Done);
    assert_eq!(
        app.transcript,
        vec![Entry::Assistant("the whole answer".to_string())]
    );
}

/// A turn that ends on a tool step still streamed its answer, so `Done` must not
/// append the reply again just because the *last* entry is no longer the answer.
#[test]
fn done_after_a_trailing_tool_step_does_not_duplicate_streamed_text() {
    let mut app = idle_app();
    app.status = Status::Running;

    apply_event(&mut app, &StreamEvent::TextDelta("the answer".to_string()));
    apply_event(
        &mut app,
        &StreamEvent::ToolStep {
            name: "bash".to_string(),
            state: ToolState::Done,
            info: None,
        },
    );
    apply_event(
        &mut app,
        &StreamEvent::Done {
            text: "the answer".to_string(),
            status: "SUCCESS".to_string(),
        },
    );

    assert_eq!(assistant_texts(&app), vec!["the answer"]);
}

/// The second turn is a new turn: deltas streamed in the first one must not make
/// the reducer suppress the second turn's terminal text forever.
#[test]
fn a_later_turns_terminal_text_is_still_pushed() {
    let mut app = idle_app();

    apply_event(&mut app, &StreamEvent::TextDelta("first".to_string()));
    apply_event(
        &mut app,
        &StreamEvent::Done {
            text: "first".to_string(),
            status: "SUCCESS".to_string(),
        },
    );

    // Nothing in the frozen table moves the status back to `Idle` after `Done`,
    // and `Enter` is refused unless the status *is* `Idle`: the reset is stage C's
    // loop or it does not happen, and without it the TUI could send exactly one
    // turn. Reported as a gap between §12's two tables rather than patched here,
    // because patching it would change a row the other stages are written against.
    app.status = Status::Idle;
    app.prompt = "second question".to_string();
    app.cursor = app.prompt.chars().count();
    assert_eq!(reduce(&mut app, Input::Enter), Action::Submit);

    apply_event(
        &mut app,
        &StreamEvent::Done {
            text: "second answer".to_string(),
            status: "SUCCESS".to_string(),
        },
    );

    assert_eq!(assistant_texts(&app), vec!["first", "second answer"]);
}

/// Protocol noise, deliberately dropped. A `Notice` per unparsed banner line is
/// what this prevents (`DESIGN.md` §12), and `PIE_LOG` already holds them.
#[test]
fn other_and_unparsed_events_push_nothing_at_all() {
    let mut app = idle_app();

    apply_event(
        &mut app,
        &StreamEvent::Other {
            kind: "item.started".to_string(),
            raw: serde_json::json!({"type": "item.started"}),
        },
    );
    apply_event(
        &mut app,
        &StreamEvent::Unparsed("codex banner v0.9".to_string()),
    );

    assert!(app.transcript.is_empty());
    assert!(app.tool_events.is_empty());
    assert!(app.last_usage.is_none());
    assert_eq!(app.status, Status::Idle);
}

// ---------------------------------------------------------------------------
// The structural rule
// ---------------------------------------------------------------------------

/// `DESIGN.md` §12: **the reducer must never see a crossterm type** — "that single
/// rule is what makes M6 testable at all". A doc comment cannot enforce it, so the
/// file's own source is read and the rule is asserted where the code lives.
///
/// The file's *comments* have to name the rule, and therefore name the crates (the
/// module doc says "no crossterm type appears in this file"), so what is forbidden
/// is a mention in **code**. A `use crossterm::…`, a `ratatui::…` path or a bare
/// `KeyEvent` in a code line is the regression; prose that forbids it is not.
#[test]
fn the_reducer_source_never_names_a_terminal_crate_in_code() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/app.rs"))
        .expect("src/tui/app.rs is part of the crate");

    let forbidden = ["crossterm", "ratatui", "KeyEvent"];
    let offenders: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| forbidden.iter().any(|name| line.contains(name)))
        .collect();

    assert!(
        offenders.is_empty(),
        "the reducer must reach no terminal type (DESIGN.md §12); offenders: {offenders:?}"
    );

    // The rule is only enforced if the reducer is *here*, on pie's own types.
    // Without this, deleting the body and the match would pass the check above by
    // having nothing to look at.
    assert!(
        source.contains("pub fn reduce(app: &mut App, input: Input) -> Action"),
        "reduce must keep taking pie's own Input and App"
    );
    assert!(source.contains("pub fn apply_event(app: &mut App, event: &StreamEvent)"));
}
