//! Gate G4 for M6 stage B: the frame `render` draws, asserted through ratatui's
//! `TestBackend`.
//!
//! `planning/ACCEPTANCE.md` §9, stage B: *"with `TestBackend`: the prompt line,
//! the status, and both `User` and `Assistant` entries appear; a long transcript
//! scrolls rather than overflowing; a picker renders its options; the buffer is
//! asserted, not merely non-empty"*. `planning/DESIGN.md` §12 freezes the types
//! and the layout.
//!
//! Nothing here opens a terminal, spawns a vendor CLI, touches the network or
//! reads `~/.pie`: every screen is an in-memory `Buffer`, so this gate cannot
//! enter the TUI by accident and cannot spend subscription quota. This file is
//! separate from `tests/tui_app.rs` because a stage gate must name the stage's
//! own test binary (`HANDOFF.md` §3) — in M5 two of three stages shipped with a
//! gate that could not fail them.
//!
//! The helpers come first and every assertion goes through them. A render test
//! whose expectation is a rectangle of spaces is unreadable, and an unreadable
//! render test is the one that gets deleted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pie::provider::{ToolState, Usage};
use pie::tui::app::{App, Entry, Picker, PickerItem, Status, VENDOR_TOOL_NOTE};
use pie::tui::render;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use ratatui::Terminal;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Draw `app` into an off-screen buffer of exactly `width` × `height`.
fn draw(app: &App, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// How many columns one cell's symbol occupies.
///
/// The buffer stores a wide glyph in the **first** of the cells it needs and
/// leaves the next cell as it found it, so joining raw symbols reads `日 本` where
/// a terminal shows `日本`. Reading a frame back faithfully therefore means
/// knowing how many columns a symbol covers. This is a small subset of the East
/// Asian Wide and Fullwidth property — the ranges these tests use, not all of
/// Unicode — which is why no fixture here carries an exotic wide glyph.
fn columns(symbol: &str) -> u16 {
    symbol
        .chars()
        .map(|c| {
            let wide = matches!(
                c as u32,
                0x1100..=0x115F
                    | 0x2E80..=0x303E
                    | 0x3041..=0x33FF
                    | 0x3400..=0x4DBF
                    | 0x4E00..=0x9FFF
                    | 0xA000..=0xA4CF
                    | 0xAC00..=0xD7A3
                    | 0xF900..=0xFAFF
                    | 0xFE30..=0xFE6F
                    | 0xFF00..=0xFF60
                    | 0xFFE0..=0xFFE6
                    | 0x1F300..=0x1FAFF
                    | 0x20000..=0x3FFFD
            );
            if wide {
                2
            } else {
                1
            }
        })
        .sum()
}

/// The buffer's rows as strings, one per screen row, left to right.
///
/// Cells covered by a wide symbol are skipped, so a row reads as the columns a
/// terminal would actually paint.
fn rows_of(buffer: &Buffer) -> Vec<String> {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0;
            while x < area.width {
                let symbol = buffer[(x, y)].symbol();
                row.push_str(symbol);
                x += columns(symbol).max(1);
            }
            row
        })
        .collect()
}

/// The rows of a freshly drawn screen.
fn screen(app: &App, width: u16, height: u16) -> Vec<String> {
    rows_of(&draw(app, width, height))
}

/// The whole screen as one line, whitespace-collapsed.
///
/// Wrapping replaces the space it breaks on with a row break, so an assertion on
/// a long sentence has to compare words rather than raw rows: this rejoins the
/// rows and collapses the seams.
fn flat_text(rows: &[String]) -> String {
    rows.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The zero-based row index of the first row containing `needle`.
fn row_of(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| {
            panic!(
                "no row contains {needle:?}; screen was:\n{}",
                rows.join("\n")
            )
        })
}

/// Every cell drawn with `Modifier::REVERSED`, as `(x, y)`.
///
/// Two things in this module are drawn reversed — the prompt's caret and the
/// picker's highlighted row — and they are how `App::cursor` and
/// `Picker::selected` are visible to a test at all.
fn reversed_cells(buffer: &Buffer) -> Vec<(u16, u16)> {
    let area = buffer.area;
    let mut found = Vec::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if buffer[(x, y)].modifier.contains(Modifier::REVERSED) {
                found.push((x, y));
            }
        }
    }
    found
}

/// The single reversed cell, or a failure naming how many were found.
fn the_caret(buffer: &Buffer) -> (u16, u16) {
    let found = reversed_cells(buffer);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one caret cell, found {found:?}"
    );
    found[0]
}

/// An app with an empty prompt and an empty transcript.
fn app() -> App {
    App::new("codex", None, "s-1-2")
}

/// A picker row, with its backend and model read out of its `backend/model`
/// label — the same shape `App::target` prints and the picker displays.
fn item(label: &str) -> PickerItem {
    let (backend, model) = label
        .split_once('/')
        .expect("picker labels are backend/model");
    PickerItem {
        backend: backend.to_string(),
        model: model.to_string(),
        label: label.to_string(),
    }
}

/// An app whose transcript holds `n` one-row user entries, `line-01` … `line-nn`.
fn app_with_lines(n: usize) -> App {
    let mut app = app();
    app.transcript = (1..=n)
        .map(|i| Entry::User(format!("line-{i:02}")))
        .collect();
    app
}

/// The entry numbers visible on screen, in row order.
///
/// Read back from the rendered rows rather than computed from the fixture, so the
/// assertion is about what a user would see.
fn visible_lines(rows: &[String]) -> Vec<usize> {
    rows.iter()
        .filter_map(|row| {
            let rest = row.split("line-").nth(1)?;
            rest.get(..2)?.parse::<usize>().ok()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The prompt line
// ---------------------------------------------------------------------------

/// The prompt is the one thing the user is typing: if it does not appear, the UI
/// is not usable, whatever else it draws.
#[test]
fn the_prompt_line_shows_what_the_user_has_typed() {
    let mut app = app();
    app.prompt = "summarise the last turn".to_string();
    app.cursor = app.prompt.chars().count();

    let rows = screen(&app, 64, 12);
    let prompt_row = row_of(&rows, "summarise the last turn");

    assert!(
        rows[prompt_row].contains("> summarise the last turn"),
        "the prompt must be marked as input, not shown as transcript: {:?}",
        rows[prompt_row]
    );
}

/// `App::cursor` is a *character* index, so the caret must land on the character
/// the user is editing, and must move when it moves.
#[test]
fn the_caret_is_drawn_on_the_character_app_cursor_names() {
    let mut app = app();
    app.prompt = "abcd".to_string();

    app.cursor = 1;
    let at_one = draw(&app, 40, 10);
    let (x1, y1) = the_caret(&at_one);
    assert_eq!(
        at_one[(x1, y1)].symbol(),
        "b",
        "cursor 1 edits the second character"
    );

    app.cursor = 3;
    let at_three = draw(&app, 40, 10);
    let (x3, y3) = the_caret(&at_three);
    assert_eq!(
        at_three[(x3, y3)].symbol(),
        "d",
        "cursor 3 edits the fourth character"
    );

    assert!(
        x3 > x1 && y3 == y1,
        "the caret must move with the cursor: ({x1}, {y1}) then ({x3}, {y3})"
    );
}

/// A cursor at the end of the prompt has no character to invert, and a caret that
/// vanished there is the state a user spends most of their time in.
#[test]
fn the_caret_survives_at_the_end_of_the_prompt() {
    let mut app = app();
    app.prompt = "hi".to_string();
    app.cursor = 2;

    let buffer = draw(&app, 40, 10);
    let (x, y) = the_caret(&buffer);
    assert_eq!(
        buffer[(x, y)].symbol(),
        " ",
        "past the last character the caret is a space"
    );
    assert_eq!(
        row_of(&rows_of(&buffer), "> hi") as u16,
        y,
        "the caret sits on the prompt row"
    );
}

// ---------------------------------------------------------------------------
// The status line
// ---------------------------------------------------------------------------

/// `ACCEPTANCE.md` §9: the status line names the target, the session, the status
/// and the last usage.
#[test]
fn the_status_line_names_the_target_the_session_the_status_and_the_last_usage() {
    let mut app = App::new("agy", Some("gemini-3.8-flash".to_string()), "s-9-9");
    app.status = Status::Done;
    app.last_usage = Some(Usage {
        input_tokens: 1200,
        output_tokens: 34,
        thinking_tokens: 7,
        cache_read_tokens: 900,
        cache_write_tokens: 0,
        total_tokens: 0,
        cost_usd_micros: 0,
    });

    let rows = screen(&app, 100, 10);
    let status = &rows[row_of(&rows, "session s-9-9")];

    assert!(
        status.contains("agy/gemini-3.8-flash"),
        "target missing: {status:?}"
    );
    assert!(status.contains("done"), "status missing: {status:?}");
    assert!(status.contains("1200"), "input tokens missing: {status:?}");
    assert!(status.contains("34"), "output tokens missing: {status:?}");
    assert!(status.contains("7"), "thinking tokens missing: {status:?}");
    assert!(
        status.contains("1241"),
        "the computed total (1200+34+7) missing: {status:?}"
    );
}

/// A turn that reported nothing is not a turn that cost nothing: showing `0`
/// would be an invented figure, and `Usage::merge` treats zero as "not reported"
/// for exactly this reason.
#[test]
fn a_status_line_with_no_usage_says_so_rather_than_showing_zeros() {
    let app = app();
    let rows = screen(&app, 100, 10);
    let status = &rows[row_of(&rows, "session s-1-2")];

    assert!(
        status.contains("not reported"),
        "expected an honest blank: {status:?}"
    );
    assert!(status.contains("idle"), "status missing: {status:?}");
    assert!(
        !status.contains('0'),
        "no figure may be invented for a turn that reported none: {status:?}"
    );
}

/// A vendor error string is external input and may carry newlines. The status
/// line is one row, so a newline must be collapsed rather than welded into the
/// neighbouring text (ratatui drops `\n` inside a span instead of breaking).
#[test]
fn a_failed_status_is_collapsed_onto_one_row() {
    let mut app = app();
    app.status = Status::Failed("backend exited 1\nno subscription".to_string());

    let rows = screen(&app, 100, 10);
    let status_row = row_of(&rows, "session s-1-2");

    assert!(
        rows[status_row].contains("failed: backend exited 1 no subscription"),
        "the reason must be one row, whole: {:?}",
        rows[status_row]
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("backend exited 1"))
            .count(),
        1,
        "the reason must not leak onto a second row: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------

/// Both halves of a conversation must appear, with their own text. Asserting only
/// that the buffer is non-empty would pass for a screen showing nothing but a
/// border — the failure mode worth catching.
#[test]
fn a_user_entry_and_an_assistant_entry_both_render_their_text() {
    let mut app = app();
    app.transcript = vec![
        Entry::User("explain the gate".to_string()),
        Entry::Assistant("the gate runs three commands".to_string()),
    ];

    let rows = screen(&app, 80, 12);
    let user_row = row_of(&rows, "explain the gate");
    let assistant_row = row_of(&rows, "the gate runs three commands");

    assert!(
        rows[user_row].contains("> explain the gate"),
        "the user's words must be marked as theirs: {:?}",
        rows[user_row]
    );
    assert!(
        assistant_row > user_row,
        "the answer must follow the question on screen: {user_row} then {assistant_row}"
    );
}

/// Top to bottom: transcript, prompt, status. Asserted as an *ordering* rather
/// than as fixed row indices, because the numbers are stage D's to adjust.
#[test]
fn the_transcript_keeps_the_prompt_and_the_status_apart() {
    let mut app = app();
    app.transcript = vec![Entry::User("hello".to_string())];
    app.prompt = "typing".to_string();
    app.cursor = app.prompt.chars().count();

    let rows = screen(&app, 80, 14);
    let transcript_row = row_of(&rows, "hello");
    let prompt_row = row_of(&rows, "typing");
    let status_row = row_of(&rows, "session s-1-2");

    assert!(
        transcript_row < prompt_row && prompt_row < status_row,
        "expected transcript {transcript_row} < prompt {prompt_row} < status {status_row}"
    );
}

/// The scroll test: the same app drawn with the window at the top and moved down.
///
/// A transcript that overflowed instead of scrolling would either paint over the
/// status line or clip the newest entry, so the check is that the *window* moves
/// — the first entry leaves and later ones arrive — while the rows below the
/// transcript stay put.
#[test]
fn a_transcript_longer_than_the_viewport_scrolls_instead_of_overflowing() {
    let top = app_with_lines(20);
    let mut scrolled = app_with_lines(20);
    scrolled.scroll = 16;

    let rows_at_top = screen(&top, 40, 12);
    let rows_scrolled = screen(&scrolled, 40, 12);

    let first = visible_lines(&rows_at_top);
    let later = visible_lines(&rows_scrolled);

    assert!(
        first.contains(&1),
        "at scroll 0 the first entry must be visible, saw {first:?}"
    );
    assert!(
        !later.contains(&1),
        "scrolling must move the window past the first entry, saw {later:?}"
    );
    assert!(
        later.iter().min() > first.iter().min(),
        "scroll did not advance: {first:?} then {later:?}"
    );
    assert_eq!(
        later,
        (later[0]..=*later.last().unwrap()).collect::<Vec<_>>(),
        "the visible entries must be a contiguous window, not a reflowed jumble: {later:?}"
    );
    assert!(
        first.len() < 20 && later.len() < 20,
        "the viewport holds a dozen rows, so the whole transcript cannot be on screen at once"
    );

    // Scrolling moves the transcript only: the two lines below it are untouched.
    for rows in [&rows_at_top, &rows_scrolled] {
        assert!(
            rows.last().unwrap().contains("session s-1-2"),
            "the status line must survive any scroll offset: {:?}",
            rows.last().unwrap()
        );
    }
}

/// A long answer is wrapped, not cut off at the right edge.
///
/// Asserted on the **whole sentence**, including its last word: a test that only
/// checked the beginning would pass for a frame that silently dropped the rest of
/// the line, which is the defect this test exists for.
#[test]
fn a_long_answer_wraps_rather_than_being_clipped_at_the_right_edge() {
    let long = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima";
    let mut app = app();
    app.transcript = vec![Entry::Assistant(long.to_string())];

    let width = 24;
    assert!(
        long.len() > usize::from(width),
        "the fixture must be wider than the terminal or the test proves nothing"
    );

    let rows = screen(&app, width, 20);
    let whole = flat_text(&rows);

    assert!(
        whole.contains(long),
        "the answer is not on screen whole; a clipped tail looks like this: {rows:?}"
    );
    assert!(
        row_of(&rows, "alpha") < row_of(&rows, "lima"),
        "the wrapped tail must continue below, not replace the beginning"
    );
}

/// `DESIGN.md` §12: the tool row carries [`VENDOR_TOOL_NOTE`], because delegate
/// mode means the vendor ran the tool and pie injects none (`RISKS.md` §1).
#[test]
fn a_tool_step_carries_the_vendor_tool_note() {
    let mut app = app();
    app.transcript = vec![Entry::Tool {
        name: "bash".to_string(),
        state: ToolState::Active,
    }];

    // Wide enough that the sentence stays on one row, so the note can be asserted
    // whole rather than partly wrapped.
    let rows = screen(&app, 120, 12);
    let tool_row = row_of(&rows, "bash");

    assert!(
        rows[tool_row].contains(VENDOR_TOOL_NOTE),
        "the note must sit beside the step it qualifies: {:?}",
        rows[tool_row]
    );
    assert!(
        rows[tool_row].contains("active"),
        "the step's state must be visible: {:?}",
        rows[tool_row]
    );
}

/// The negative control for the test above: with no tool step, the sentence must
/// not appear. Without this, a `render` that printed the note unconditionally
/// would pass the test above and still be wrong.
#[test]
fn the_vendor_tool_note_is_absent_when_no_tool_step_was_reported() {
    let mut app = app();
    app.transcript = vec![
        Entry::User("hello".to_string()),
        Entry::Assistant("hi".to_string()),
    ];

    let rows = screen(&app, 120, 12);

    assert!(
        !flat_text(&rows).contains(VENDOR_TOOL_NOTE),
        "no tool step was reported, so the note must not appear: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// The model picker
// ---------------------------------------------------------------------------

/// A working picker row.
fn picker_app(selected: usize) -> App {
    let mut app = app();
    app.picker = Some(Picker {
        items: vec![
            item("codex/gpt-6-luna"),
            item("agy/gemini-3.8-flash"),
            item("claude/sonnet-4.9"),
        ],
        selected,
    });
    app
}

/// Every alternative must be on screen: a picker that showed one row would make
/// "what am I about to switch to" unanswerable.
#[test]
fn an_open_picker_lists_every_row_it_was_given() {
    let app = picker_app(0);
    let rows = screen(&app, 80, 20);

    for label in [
        "codex/gpt-6-luna",
        "agy/gemini-3.8-flash",
        "claude/sonnet-4.9",
    ] {
        row_of(&rows, label);
    }
}

/// The highlight must be *visible*, and it must be the highlighted row that
/// carries it — a mark on the wrong row is worse than no mark.
#[test]
fn the_picker_marks_the_highlighted_row_and_only_that_row() {
    let rows = screen(&picker_app(0), 80, 20);

    let marked = row_of(&rows, "> codex/gpt-6-luna") as u16;
    let unmarked = row_of(&rows, "agy/gemini-3.8-flash") as u16;
    assert!(
        !rows[usize::from(unmarked)].contains("> agy"),
        "an unhighlighted row must not be marked: {:?}",
        rows[usize::from(unmarked)]
    );

    let buffer = draw(&picker_app(0), 80, 20);
    let reversed = reversed_cells(&buffer);
    assert!(
        reversed.iter().any(|(_, y)| *y == marked),
        "the highlighted row must be styled as well as prefixed, found {reversed:?}"
    );
    assert!(
        reversed.iter().all(|(_, y)| *y != unmarked),
        "the other row must not be reversed: {reversed:?}"
    );
}

/// Moving the highlight must visibly change the frame — asserting that the two
/// buffers differ, plus that the mark moved, because a buffer difference alone
/// could come from anything.
#[test]
fn moving_the_picker_highlight_moves_the_mark() {
    let first = draw(&picker_app(0), 80, 20);
    let second = draw(&picker_app(1), 80, 20);

    assert_ne!(
        first, second,
        "selected = 0 and selected = 1 must not render alike"
    );

    let first_rows = rows_of(&first);
    let second_rows = rows_of(&second);

    assert!(first_rows[row_of(&first_rows, "> codex/gpt-6-luna")].contains("> codex"));
    assert!(!first_rows[row_of(&first_rows, "agy/gemini-3.8-flash")].contains("> agy"));

    assert!(second_rows[row_of(&second_rows, "> agy/gemini-3.8-flash")].contains("> agy"));
    assert!(
        !second_rows[row_of(&second_rows, "codex/gpt-6-luna")].contains("> codex"),
        "the mark must have left the first row: {:?}",
        second_rows[row_of(&second_rows, "codex/gpt-6-luna")]
    );

    // The *style* has to move with the prefix, or the row is only half marked.
    let first_mark = row_of(&first_rows, "codex/gpt-6-luna") as u16;
    let second_mark = row_of(&second_rows, "agy/gemini-3.8-flash") as u16;
    assert!(
        reversed_cells(&first).iter().any(|(_, y)| *y == first_mark),
        "selected = 0 must highlight the first row: {:?}",
        reversed_cells(&first)
    );
    assert!(
        reversed_cells(&second)
            .iter()
            .any(|(_, y)| *y == second_mark),
        "selected = 1 must highlight the second row: {:?}",
        reversed_cells(&second)
    );
}

/// An open picker with no rows yet — the state the reducer creates before the
/// caller has filled it (`src/tui/app.rs`, `Action::OpenPicker`) — must draw an
/// empty picker rather than panicking on a highlight that indexes nothing.
#[test]
fn an_empty_picker_renders_without_panicking() {
    let mut app = app();
    app.picker = Some(Picker::default());

    let rows = screen(&app, 60, 16);
    assert!(
        !flat_text(&rows).contains("codex/gpt-6-luna"),
        "an empty picker must not invent rows"
    );
}

// ---------------------------------------------------------------------------
// Multi-byte and wide characters
// ---------------------------------------------------------------------------

/// The highest-value test in the file.
///
/// `App::cursor` counts characters, and the obvious way to honour that is
/// `&app.prompt[..cursor]` — a *byte* index, which panics on the second character
/// of `日本語` and would panic on the emoji in `🚀ab` at every offset. The cursor
/// is also placed here without measuring display width, so a wide character at
/// the cursor is inverted as one unit rather than split.
#[test]
fn a_multi_byte_prompt_with_the_caret_inside_it_neither_panics_nor_loses_a_character() {
    let cases = [
        ("日本語", 1),
        ("日本語", 2),
        ("日本語", 3),
        ("🚀ab", 1),
        ("a🚀b", 2),
        ("é", 1),
        ("naïve café", 7),
    ];

    for (prompt, cursor) in cases {
        let mut app = app();
        app.prompt = prompt.to_string();
        app.cursor = cursor;

        // 40 columns is wide enough that the prompt never wraps, so the whole
        // string has to be present on one row or something dropped it.
        let buffer = draw(&app, 40, 12);
        let rows = rows_of(&buffer);
        let prompt_row = row_of(&rows, &format!("> {prompt}"));

        let expected = prompt.chars().nth(cursor).unwrap_or(' ');
        let (x, y) = the_caret(&buffer);
        assert_eq!(
            buffer[(x, y)].symbol(),
            expected.to_string(),
            "{prompt:?} at cursor {cursor}: the caret is on the wrong character"
        );
        assert_eq!(
            y, prompt_row as u16,
            "{prompt:?} at cursor {cursor}: the caret left the prompt row"
        );
    }
}

/// A wide character wider than the space left on the row is the other half of the
/// multi-byte family: ratatui's reflow either moves the whole glyph to the next
/// row or omits it, and the failure it must not have is drawing past the last
/// column — which is not a cosmetic fault, it is an index out of the buffer's
/// width, i.e. a panic from a rendering path a user reaches by pasting CJK or an
/// emoji into the prompt.
///
/// Every width from 1 to 40 is drawn because the straddle moves with the width:
/// at one width the glyph lands exactly on the last column, at another it does
/// not. Content is then asserted at a comfortable width, where a lost glyph or a
/// swallowed tail would show up.
///
/// The sweep is also what found the ratatui 0.29 defect `MIN_WRAPPED_COLUMNS`
/// guards: at exactly two columns a long unbreakable run containing a wide glyph
/// makes the wrapper emit a line wider than the area, and the renderer panics
/// with `index outside of buffer`. Deleting that guard fails this test at width 2
/// with that panic, which is the point of keeping the guard.
#[test]
fn a_wide_character_at_any_width_neither_panics_nor_loses_a_marker() {
    let mut app = app();
    app.prompt = "日本語".to_string();
    app.cursor = 1;
    app.transcript = vec![
        Entry::Assistant(format!("{}語🚀 tail-marker", "x".repeat(19))),
        Entry::User("STABLE-MARKER".to_string()),
        Entry::Tool {
            name: "bash".to_string(),
            state: ToolState::Done,
        },
    ];

    for width in 1..=40u16 {
        let rows = screen(&app, width, 16);
        assert_eq!(
            rows.len(),
            16,
            "width {width}: the frame lost rows, so a glyph was drawn outside it"
        );
    }

    let rows = screen(&app, 40, 16);
    let whole = flat_text(&rows);

    assert!(
        whole.contains("STABLE-MARKER"),
        "the entry after the wide characters was lost: {rows:?}"
    );
    assert!(
        whole.contains("tail-marker"),
        "the tail of the answer was swallowed by a wide character: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("日本語")),
        "the wide prompt must render whole at a width that fits it: {rows:?}"
    );
    assert!(
        rows.last().unwrap().contains("session s-1-2"),
        "the status line must survive the transcript's wide characters: {:?}",
        rows.last().unwrap()
    );
}

// ---------------------------------------------------------------------------
// Purity and degenerate sizes
// ---------------------------------------------------------------------------

/// `render` takes `&App` precisely so that drawing cannot change state; this is
/// the rendered form of that rule. A second draw that differed would mean the
/// frame depended on something outside its argument — scroll position, a cache, a
/// counter — which no test could then pin down.
#[test]
fn drawing_the_same_app_twice_produces_byte_identical_frames() {
    let mut app = picker_app(1);
    app.prompt = "hello".to_string();
    app.cursor = 5;
    app.status = Status::Running;
    app.last_usage = Some(Usage {
        input_tokens: 12,
        output_tokens: 3,
        ..Usage::default()
    });
    app.transcript = vec![
        Entry::User("hello".to_string()),
        Entry::Assistant("world".to_string()),
        Entry::Tool {
            name: "bash".to_string(),
            state: ToolState::Error,
        },
        Entry::Notice("switched to codex/gpt-6-luna".to_string()),
    ];
    app.scroll = 3;

    assert_eq!(
        draw(&app, 72, 20),
        draw(&app, 72, 20),
        "the same App must draw the same frame, twice"
    );
}

/// A terminal smaller than the layout asks for — every pane minimum — must draw
/// something harmless rather than panicking on a negative rectangle. Window
/// managers resize terminals, and a panic here is a crashed UI, not a cosmetic
/// fault.
#[test]
fn a_terminal_too_small_for_the_layout_still_renders() {
    for (width, height) in [(1u16, 1u16), (3, 2), (10, 3), (20, 5), (200, 60)] {
        let mut app = picker_app(0);
        app.prompt = "x".to_string();
        app.cursor = 1;

        let rows = screen(&app, width, height);
        assert_eq!(
            rows.len(),
            usize::from(height),
            "{width}x{height}: the frame must be exactly the terminal"
        );
    }
}
