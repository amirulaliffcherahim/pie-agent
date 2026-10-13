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
use pie::tui::app::{reduce, App, Entry, Input, Picker, PickerItem, Status, VENDOR_TOOL_NOTE};
use pie::tui::render;
use pie::tui::slash;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
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

// ---------------------------------------------------------------------------
// M12 stage B: the shell
//
// `planning/ACCEPTANCE.md` §20's stage B row is the specification, verbatim:
//
// > the layout, asserted in a `TestBackend` buffer: the composer is fixed at the bottom
// > while the transcript scrolls (the same `App` rendered twice at two scroll offsets and
// > two terminals sizes); the composer border reflects each `Status`; the footer names
// > folder, session, model, usage and cost; a tool block renders collapsed with its name,
// > state and duration, and expanded after `Ctrl+O`; truncation is **stated in the block**;
// > the palette draws above the composer; and none of it panics at a narrow, a tiny or a
// > very tall terminal
//
// Each of those is a claim about a *buffer*, which is why every assertion below reads the
// rows or the cells `render` actually drew rather than the fields it was handed. Where a
// claim cannot be made about the frame — see `a_tool_block_…`, which drives the block
// builder directly because `Entry::Tool` carries neither a duration nor output — the test
// says so in its own doc, and the file's report records it as a gap rather than dressing a
// weaker assertion up as the criterion.
// ---------------------------------------------------------------------------

/// The rows one region covers, as drawn.
fn region_rows(rows: &[String], area: Rect) -> Vec<String> {
    let top = usize::from(area.y).min(rows.len());
    let bottom = (top + usize::from(area.height)).min(rows.len());
    rows[top..bottom].to_vec()
}

/// The footer at a size, as one string.
fn footer_at(rows: &[String], width: u16, height: u16) -> String {
    region_rows(rows, render::regions(width, height).footer).join(" ")
}

/// The composer border's colour and modifier, read from a border cell the box's title
/// cannot reach: the bottom-right corner.
fn border_style(buffer: &Buffer, width: u16, height: u16) -> (Color, Modifier) {
    let composer = render::regions(width, height).composer;
    let cell = &buffer[(
        composer.x + composer.width - 1,
        composer.y + composer.height - 1,
    )];
    (cell.fg, cell.modifier)
}

/// An app whose palette was opened by *typing*, through [`reduce`].
///
/// The open state is the reducer's rather than a fixture's, so the palette these tests draw
/// is the one a user gets, and a change to what opens it reddens these tests too.
fn palette_app(text: &str) -> App {
    let mut app = app();
    for ch in text.chars() {
        reduce(&mut app, Input::Char(ch));
    }
    app
}

/// A tool block's rows as one string.
fn block_text(rows: &[Line<'static>]) -> String {
    rows.iter()
        .map(|line| format!("{line}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The row budget, as numbers: which region gets how many rows, and in what order.
///
/// Asserted as the *sum* plus the two sizes the criterion names, rather than as a table of
/// constants, because the numbers that matter are the ones a user would notice if they
/// moved: the footer is the bottom row, the composer starts where the transcript ends, and
/// at 80x24 the transcript keeps its floor.
#[test]
fn the_row_budget_gives_the_transcript_its_floor_and_the_footer_the_bottom_row() {
    let ordinary = render::regions(80, 24);
    assert_eq!(
        ordinary.composer.height, 3,
        "the composer is a border, a prompt row and a border"
    );
    assert_eq!(ordinary.footer.height, 1, "the footer is one summary row");
    assert_eq!(
        usize::from(ordinary.footer.y),
        23,
        "the footer is the bottom row of the frame"
    );
    assert!(
        usize::from(ordinary.transcript.height)
            + usize::from(ordinary.composer.height)
            + usize::from(ordinary.help.height)
            + usize::from(ordinary.footer.height)
            == 24,
        "the four regions must tile the frame exactly: {ordinary:?}"
    );
    assert!(
        ordinary.transcript.height >= 8,
        "the transcript keeps its floor even where the whole keymap fits: {ordinary:?}"
    );
    assert_eq!(ordinary.composer.y, ordinary.transcript.height);
    assert_eq!(
        ordinary.help.y,
        ordinary.composer.y + ordinary.composer.height
    );
    assert_eq!(ordinary.footer.y, ordinary.help.y + ordinary.help.height);

    // 20x5: a tiny terminal, where something has to give. The order is the priority — the
    // composer and one footer row are what a user needs, and the transcript is what is
    // squeezed out of a five-row frame.
    let tiny = render::regions(20, 5);
    assert_eq!(
        tiny.composer.height, 3,
        "the tiny frame keeps the whole composer"
    );
    assert_eq!(tiny.footer.height, 1);
    assert_eq!(
        usize::from(tiny.footer.y),
        4,
        "the footer is still the bottom row"
    );
    assert_eq!(
        usize::from(tiny.transcript.height)
            + usize::from(tiny.composer.height)
            + usize::from(tiny.help.height)
            + usize::from(tiny.footer.height),
        5,
        "the four regions must tile five rows too: {tiny:?}"
    );
}

/// The claim that makes the shell a shell: the composer and the footer are **not** part of the
/// scroll region.
///
/// Drawn as the criterion asks — one `App` at two scroll offsets, at two terminal sizes — and
/// compared row by row below the transcript. A layout that scrolled everything would move the
/// composer and this test would see it; a layout that only *looked* fixed because the
/// transcript happened to fit would fail the second half, which asserts the transcript really
/// did move.
#[test]
fn the_composer_and_footer_are_fixed_while_the_transcript_scrolls() {
    for (width, height) in [(80u16, 24u16), (40, 12)] {
        let at_top = app_with_lines(30);
        let mut scrolled = app_with_lines(30);
        scrolled.scroll = 9;

        let top_rows = screen(&at_top, width, height);
        let scrolled_rows = screen(&scrolled, width, height);
        let boundary = usize::from(render::regions(width, height).composer.y);

        assert_eq!(
            top_rows[boundary..],
            scrolled_rows[boundary..],
            "{width}x{height}: the composer, the help and the footer sit outside the scroll \
             region, so a scroll must not touch a single one of their cells"
        );
        assert_ne!(
            top_rows[..boundary],
            scrolled_rows[..boundary],
            "{width}x{height}: the transcript is the part that scrolls, so it must have moved"
        );
        assert!(
            visible_lines(&top_rows).contains(&1),
            "{width}x{height}: the fixture must start at the top of the transcript"
        );
        assert!(
            !visible_lines(&scrolled_rows).contains(&1),
            "{width}x{height}: scroll 9 left the first entry on screen, so nothing was \
             actually scrolled: {:?}",
            visible_lines(&scrolled_rows)
        );
    }
}

/// The same claim at the two heights §20 names, stated as what a user would see: the composer
/// box is on screen, with the draft in it, however long the conversation is.
#[test]
fn a_transcript_longer_than_the_viewport_does_not_push_the_composer_off_the_bottom() {
    for height in [24u16, 5] {
        let mut app = app_with_lines(30);
        app.prompt = "still here".to_string();
        app.cursor = app.prompt.chars().count();

        let rows = screen(&app, 60, height);
        let composer = region_rows(&rows, render::regions(60, height).composer);

        assert_eq!(rows.len(), usize::from(height), "the frame is the terminal");
        assert_eq!(
            composer.len(),
            3,
            "at {height} rows the composer is still its three rows: {rows:?}"
        );
        assert!(
            composer[0].contains('┌') && composer[2].contains('└'),
            "the composer's border must be on screen: {composer:?}"
        );
        assert!(
            composer.join(" ").contains("still here"),
            "the draft must survive a long transcript at {height} rows: {composer:?}"
        );
        assert!(
            rows.last().unwrap().contains("session s-1-2"),
            "the footer must still be the last row: {rows:?}"
        );
    }
}

/// The composer's border is where `Status` is drawn (`DESIGN.md` §21.2), so it has to differ
/// between statuses — and the failure this test exists for is a border that is a constant
/// colour with a `match` beside it that looks like it does something.
///
/// Every pair is compared, not one: two statuses sharing a colour would pass a test that only
/// checked `Idle` against `Failed`, and `Running` against `Done` is exactly the pair a
/// careless palette collapses.
#[test]
fn the_composer_border_reflects_each_status() {
    let statuses = [
        Status::Idle,
        Status::Running,
        Status::Done,
        Status::Failed("boom".to_string()),
    ];

    let mut borders = Vec::new();
    for status in &statuses {
        let mut app = app();
        app.status = status.clone();
        borders.push(border_style(&draw(&app, 60, 16), 60, 16));
    }

    for (index, left) in borders.iter().enumerate() {
        for (other, right) in borders.iter().enumerate() {
            assert!(
                index == other || left != right,
                "{:?} and {:?} draw the same border {left:?}, so the border does not carry \
                 the status",
                statuses[index],
                statuses[other]
            );
        }
    }
}

/// `DESIGN.md` §21.2's footer: folder, session, model, last usage, cost.
///
/// Read from the row that was drawn, at a width that fits it whole, so a field that fell off
/// the end (the way a narrow terminal clips) fails here rather than passing because it exists
/// somewhere in the string.
#[test]
fn the_footer_names_the_folder_the_session_the_model_the_usage_and_the_cost() {
    let mut app = App::new("agy", Some("gemini-3.8-flash".to_string()), "s-9-9");
    app.status = Status::Done;
    app.last_usage = Some(Usage {
        input_tokens: 1200,
        output_tokens: 34,
        thinking_tokens: 7,
        cost_usd_micros: 1_500_000,
        ..Usage::default()
    });

    // The width must fit the path **on this machine**, not the path on the machine this test was
    // written on. It was a fixed 160, which fits `C:\Users\amiru\.pie` and does not fit the long
    // temp directory gate G3d redirects `HOME` to (`…\Temp\pie-g3d-<guid>\.pie`) - the footer line
    // came to exactly 160 columns and the `.pie` was clipped, so `contains(pie_home)` passed for
    // the author's home directory and failed everywhere else. That is B32's shape exactly, and it
    // is why the doc comment above says the width must fit the row whole *so that a field which
    // fell off the end fails here*: deriving the width keeps that intent instead of abandoning it.
    let folder_width = u16::try_from(pie::paths::pie_home().display().to_string().chars().count())
        .unwrap_or(u16::MAX);
    let (width, height) = (140u16.saturating_add(folder_width), 12u16);
    let footer = footer_at(&screen(&app, width, height), width, height);

    assert!(
        footer.contains(&pie::paths::pie_home().display().to_string()),
        "the folder field must be pie's own state folder, the one `/status` prints: {footer:?}"
    );
    assert!(footer.contains("session s-9-9"), "{footer:?}");
    assert!(
        footer.contains("model agy/gemini-3.8-flash"),
        "the footer must name the target: {footer:?}"
    );
    assert!(footer.contains("done"), "{footer:?}");
    assert!(
        footer.contains("tokens in 1200 out 34 think 7 total 1241"),
        "the last usage, with the computed total: {footer:?}"
    );
    assert!(footer.contains("cost $1.50"), "{footer:?}");
}

/// The rule `a_status_line_with_no_usage_says_so_rather_than_showing_zeros` states, held on the
/// fields that carry figures after the folder joined the row.
///
/// The folder is a real path and a digit inside a path is not an invented figure, so the
/// assertion is scoped to everything before it — while the whole-row version stays as it is, so
/// the witness that has been red since M6 keeps its teeth in this environment.
#[test]
fn a_footer_with_no_usage_says_so_rather_than_showing_zeros_in_its_figures() {
    let (width, height) = (160u16, 12u16);
    let footer = footer_at(&screen(&app(), width, height), width, height);
    let figures = footer.split("folder").next().unwrap_or("").to_string();

    assert!(
        figures.contains("tokens not reported"),
        "a turn that reported nothing is not a turn that cost nothing: {figures:?}"
    );
    assert!(
        figures.contains("cost not reported"),
        "zero cost means *not reported*, never free: {figures:?}"
    );
    assert!(
        !figures.contains('0'),
        "no figure may be invented for a turn that reported none: {figures:?}"
    );
}

/// A cost that is real but rounds to nothing says so instead of printing the `$0.00` that
/// would read as free — the same rule as above, one decimal place further down.
#[test]
fn a_cost_too_small_to_round_to_a_cent_is_not_drawn_as_free() {
    let mut app = app();
    app.last_usage = Some(Usage {
        input_tokens: 3,
        cost_usd_micros: 2_500,
        ..Usage::default()
    });

    let (width, height) = (160u16, 12u16);
    let footer = footer_at(&screen(&app, width, height), width, height);

    assert!(footer.contains("cost < $0.01"), "{footer:?}");
    assert!(!footer.contains("$0.00"), "{footer:?}");
}

/// A tool block on screen: collapsed, with its name, its state, and an honest word where the
/// duration would go.
///
/// `Entry::Tool` carries no duration, so the block says so rather than inventing a `0ms`. The
/// expanded form is driven through [`render::tool_block`] directly by the tests below, and the
/// reason it cannot be driven through the frame is recorded there.
#[test]
fn a_tool_block_shows_its_name_its_state_and_says_when_the_duration_is_missing() {
    let mut app = app();
    app.transcript = vec![Entry::Tool {
        name: "bash".to_string(),
        state: ToolState::Error,
    }];

    // Wide enough that the whole block, note included, stays on its one row: at 120 columns
    // `a_tool_step_carries_the_vendor_tool_note` pins that same row, and this test must not
    // pass merely because the note wrapped.
    let rows = screen(&app, 140, 12);
    let row = &rows[row_of(&rows, "bash")];

    assert!(
        row.contains('▸'),
        "collapsed by default, not expanded: {row:?}"
    );
    assert!(
        !row.contains('▾'),
        "nothing is expanded without Ctrl+O: {row:?}"
    );
    assert!(
        row.contains("tool: bash"),
        "the block names the tool: {row:?}"
    );
    assert!(row.contains("error"), "the block names the state: {row:?}");
    assert!(
        row.contains(render::NO_DURATION),
        "a duration nothing reported must say so rather than showing a zero: {row:?}"
    );
    assert!(row.contains(VENDOR_TOOL_NOTE), "{row:?}");
}

/// The block builder itself, on the two settings the frame cannot reach yet.
///
/// **Why this is not a frame test, stated rather than hidden.** `Entry::Tool { name, state }`
/// carries no duration and no output, and `App` has no field naming an expanded block, so no
/// `App` a test can build produces an expanded block or a duration. Driving the builder with
/// the values the criterion describes is the strongest available check; the report for this
/// stage records the `App`-side change that would let these become frame assertions.
#[test]
fn a_tool_block_states_the_duration_it_has_and_says_so_when_it_has_none() {
    let known = block_text(&render::tool_block(
        "read",
        ToolState::Done,
        Some(std::time::Duration::from_millis(120)),
        None,
        false,
    ));
    assert!(
        known.contains("120ms"),
        "§21.2's first duration shape: {known:?}"
    );
    assert!(known.contains("done"), "{known:?}");
    assert!(
        !known.contains("output collapsed"),
        "there is no output to collapse, so the block must not promise one: {known:?}"
    );

    let seconds = block_text(&render::tool_block(
        "bash",
        ToolState::Error,
        Some(std::time::Duration::from_millis(3_100)),
        None,
        false,
    ));
    assert!(
        seconds.contains("3.1s"),
        "§21.2's second shape: {seconds:?}"
    );

    let unknown = block_text(&render::tool_block(
        "bash",
        ToolState::Active,
        None,
        None,
        false,
    ));
    assert!(unknown.contains(render::NO_DURATION), "{unknown:?}");
    assert!(
        !unknown.contains("0ms"),
        "an unreported duration is not a zero-length one: {unknown:?}"
    );
}

/// Collapsed means the output is not on screen — and that the block *says* it is collapsed,
/// rather than leaving a reader to guess whether there was any output at all.
#[test]
fn a_collapsed_tool_block_hides_its_output_and_says_that_it_did() {
    let text = block_text(&render::tool_block(
        "bash",
        ToolState::Done,
        Some(std::time::Duration::from_millis(3_100)),
        Some("SECRET-OUTPUT\nsecond line"),
        false,
    ));

    assert!(
        !text.contains("SECRET-OUTPUT"),
        "a collapsed block must not draw its output: {text:?}"
    );
    assert!(
        text.contains("output collapsed"),
        "and it must say so, rather than looking like a block with nothing in it: {text:?}"
    );
}

/// Expanded means the output is there — including a stated count of what was dropped.
///
/// This is the `DESIGN.md` §21.5 / I5 / B22 rule: a truncated block that does not say so is a
/// lie about the output. Deleting the counting row is what this test is for, and it reddens
/// immediately.
#[test]
fn an_expanded_tool_block_shows_its_output_and_states_the_lines_it_hid() {
    let output = (1..=9)
        .map(|n| format!("out-{n:02}"))
        .collect::<Vec<_>>()
        .join("\n");

    let rows = render::tool_block("bash", ToolState::Done, None, Some(&output), true);
    let text = block_text(&rows);

    assert!(
        text.contains('▾'),
        "expanded blocks carry the open marker: {text:?}"
    );
    assert!(text.contains("out-01"), "{text:?}");
    assert!(text.contains("out-06"), "{text:?}");
    assert!(
        !text.contains("out-07"),
        "the block stops at its ceiling: {text:?}"
    );
    assert!(
        text.contains("3 more output lines"),
        "the block must count what it dropped, not drop it silently: {text:?}"
    );
    assert!(text.contains("truncated"), "{text:?}");

    // And with the whole output inside the ceiling there is nothing to state: a note that
    // always appeared would be as misleading as one that never did.
    let short = block_text(&render::tool_block(
        "bash",
        ToolState::Done,
        None,
        Some("one\ntwo"),
        true,
    ));
    assert!(short.contains("two"), "{short:?}");
    assert!(!short.contains("truncated"), "{short:?}");
}

/// `DESIGN.md` §21.2: the palette is drawn **above** the composer, as an overlay over the
/// bottom of the transcript.
///
/// The strongest form of the claim is positional: the panel's bottom border is the row directly
/// above the composer's top border, and nothing the overlay draws lands on or below it. A
/// palette drawn under the composer — the break this test exists for — fails on both.
#[test]
fn the_palette_draws_above_the_composer_and_never_over_it() {
    let app = palette_app("/");
    let (width, height) = (60u16, 20u16);
    let rows = screen(&app, width, height);
    let regions = render::regions(width, height);
    let composer_top = usize::from(regions.composer.y);

    assert!(
        rows[composer_top - 1].contains('┘'),
        "the palette panel's bottom border must sit immediately above the composer: {:?}",
        rows[composer_top - 1]
    );
    assert!(
        rows[composer_top].contains('┌'),
        "and the composer's own border must still be there: {:?}",
        rows[composer_top]
    );
    assert!(
        region_rows(&rows, regions.composer)
            .join(" ")
            .contains("> /"),
        "the composer is not covered by the overlay: {rows:?}"
    );

    for (index, row) in rows.iter().enumerate() {
        if row.contains("  ·  ") || row.contains(" commands ") {
            assert!(
                index < composer_top,
                "an overlay row is drawn on or below the composer: {row:?}"
            );
        }
    }
}

/// `DESIGN.md` §21.3: each row shows its name, its argument hint and its description — and the
/// rows are the registry's, filtered by the prompt.
///
/// The oracle is the registry itself and not a list copied here: `slash::matching(filter)` is
/// what the overlay must be showing, so a renderer that cached its own rows, or invented one,
/// disagrees with this assertion rather than with itself.
#[test]
fn the_palette_names_each_rows_command_its_argument_hint_and_its_description() {
    let app = palette_app("/mo");
    let rows = screen(&app, 70, 18);
    let whole = flat_text(&rows);

    let matching = slash::matching("mo");
    assert_eq!(
        matching.len(),
        1,
        "`/mo` is a prefix of one row: {matching:?}"
    );
    for row in &matching {
        assert!(
            whole.contains(row.name),
            "the overlay omits `{}`: {whole:?}",
            row.name
        );
        assert!(
            whole.contains(row.argument),
            "the overlay omits the argument hint `{}`: {whole:?}",
            row.argument
        );
        assert!(
            whole.contains(row.description),
            "the overlay omits the description of `/{}`: {whole:?}",
            row.name
        );
    }
    assert!(
        whole.contains("/model [provider/model] · choose the model"),
        "name, hint and description belong on one row: {whole:?}"
    );
    assert!(
        !whole.contains("/effort"),
        "a row the filter does not match must not be drawn: {whole:?}"
    );
}

/// The highlight: a marker on the highlighted row and on no other, moving with the key that
/// moves it.
#[test]
fn the_palette_marks_only_the_highlighted_row() {
    let (width, height) = (70u16, 20u16);

    let at_first = draw(&palette_app("/"), width, height);
    let first_rows = rows_of(&at_first);
    let highlighted = row_of(&first_rows, "> /model");
    let other = row_of(&first_rows, "/effort");
    assert!(
        !first_rows[other].contains("> /effort"),
        "an unhighlighted row must not be marked: {:?}",
        first_rows[other]
    );
    assert!(
        reversed_cells(&at_first)
            .iter()
            .any(|(_, y)| usize::from(*y) == highlighted),
        "the mark must carry a style too, not just a prefix: {:?}",
        reversed_cells(&at_first)
    );
    assert!(
        reversed_cells(&at_first)
            .iter()
            .all(|(_, y)| usize::from(*y) != other),
        "the other row must not be highlighted: {:?}",
        reversed_cells(&at_first)
    );

    let mut moved = palette_app("/");
    reduce(&mut moved, Input::Down);
    reduce(&mut moved, Input::Down);
    let after = draw(&moved, width, height);
    let moved_rows = rows_of(&after);
    let now = row_of(&moved_rows, "> /backend");

    assert!(
        !moved_rows[row_of(&moved_rows, "/model")].contains("> /model"),
        "the mark must leave the row it was on: {:?}",
        moved_rows[row_of(&moved_rows, "/model")]
    );
    assert!(
        reversed_cells(&after)
            .iter()
            .any(|(_, y)| usize::from(*y) == now),
        "the highlight must move with the mark: {:?}",
        reversed_cells(&after)
    );
}

/// `ACCEPTANCE.md` §14's S13, answered: a list that is scrolled must say that it is scrolled,
/// and how much of it is off the panel.
///
/// This is the break `the palette draws all rows without a more indicator` produces — at 70x14
/// the panel holds five rows of nineteen — and the test fails on it twice over: the note row
/// disappears, and the number of rows actually drawn no longer adds up to the registry's.
#[test]
fn the_palette_says_how_many_rows_it_is_not_showing() {
    let app = palette_app("/");
    let (width, height) = (70u16, 14u16);
    let rows = screen(&app, width, height);
    let composer_top = usize::from(render::regions(width, height).composer.y);

    let overlay: Vec<&String> = rows[..composer_top]
        .iter()
        .filter(|row| row.contains("  ·  ") || row.contains("more of"))
        .collect();
    let drawn = overlay.iter().filter(|row| row.contains("  ·  ")).count();
    let note = overlay
        .iter()
        .find(|row| row.contains("more of"))
        .unwrap_or_else(|| panic!("the overlay hid rows without saying so: {rows:?}"));
    let hidden: usize = note
        .split("more of")
        .next()
        .unwrap_or("")
        // The row is a bordered panel row, so the count is the last token before the words:
        // `│  … 14 more of 19 rows …`.
        .split_whitespace()
        .last()
        .unwrap_or("")
        .parse()
        .unwrap_or_else(|_| panic!("the count is unreadable in {note:?}"));

    let total = slash::matching("").len();
    assert!(
        hidden > 0,
        "the panel must be hiding something at this size"
    );
    assert!(
        drawn + hidden == total,
        "the note must account for every row: {drawn} drawn + {hidden} hidden != {total}\n{rows:?}"
    );
    assert!(drawn < total, "sanity: the whole table cannot fit here");
}

/// An open palette whose filter matches nothing says so, rather than drawing an empty box a
/// user cannot tell from a frozen UI.
#[test]
fn a_palette_with_nothing_matching_says_so_rather_than_drawing_an_empty_box() {
    let app = palette_app("/zzzz");
    assert!(
        app.palette.is_some(),
        "the overlay stays open with no rows to show — that is the state being tested"
    );
    assert!(
        slash::matching("zzzz").is_empty(),
        "the fixture matches nothing"
    );

    let rows = screen(&app, 60, 16);
    assert!(
        flat_text(&rows).contains("nothing matches /zzzz"),
        "an empty palette must say why it is empty: {rows:?}"
    );
}

/// A prompt with a line break in it, which is what a paste produces: the caret's row is the one
/// the composer draws, because the interior is a single row.
///
/// Without [`render`]'s own splitting of the prompt, ratatui drops the `\n` inside the span and
/// the caret is drawn on a row the paragraph never draws — a prompt you cannot see the end of.
#[test]
fn a_multi_line_prompt_scrolls_the_composer_so_the_caret_is_on_screen() {
    let mut app = app();
    app.prompt = "first line\nsecond line".to_string();
    app.cursor = app.prompt.chars().count();

    let (width, height) = (60u16, 12u16);
    let buffer = draw(&app, width, height);
    let rows = rows_of(&buffer);
    let interior = usize::from(render::regions(width, height).composer.y) + 1;

    let (_, caret_row) = the_caret(&buffer);
    assert_eq!(
        usize::from(caret_row),
        interior,
        "the caret must be on the composer's one prompt row: {rows:?}"
    );
    assert!(
        rows[interior].contains("second line"),
        "the row the caret is on must be the row that is drawn: {:?}",
        rows[interior]
    );
    assert!(
        !rows[interior].contains("first line"),
        "the scrolled-away row must be the one that is hidden: {:?}",
        rows[interior]
    );
}

/// The sizes that must not panic, swept rather than sampled.
///
/// M6's B29 was a scroll-offset overflow inside ratatui that only an adversarial grid found,
/// and B30-adjacent work found a two-column `WordWrapper` panic — so this draws the busiest
/// frame this module can be handed (a palette over a long transcript carrying a wide character,
/// a failed status, a saturated usage and a scroll offset past the end) at every width from 1
/// to 48 and at three heights, plus the named corners of the space: 40x12, 20x5, 80x24, a very
/// tall frame, and a very wide one.
#[test]
fn no_layout_panics_at_a_narrow_a_tiny_or_a_very_tall_terminal() {
    let mut busy = palette_app("/mo");
    busy.transcript = vec![
        Entry::User("question".to_string()),
        Entry::Assistant(format!("{} 語🚀 tail", "x".repeat(19))),
        Entry::Tool {
            name: "bash".to_string(),
            state: ToolState::Error,
        },
        Entry::Notice("switched to codex/gpt-6-luna".to_string()),
    ];
    busy.status = Status::Failed("backend exited 1\nno subscription".to_string());
    busy.last_usage = Some(Usage {
        input_tokens: 1 << 40,
        output_tokens: 1,
        thinking_tokens: 2,
        cost_usd_micros: u64::MAX,
        ..Usage::default()
    });
    busy.scroll = u16::MAX;

    let corners = [
        (1u16, 1u16),
        (2, 2),
        (3, 2),
        (10, 3),
        (20, 5),
        (24, 8),
        (40, 12),
        (60, 20),
        (80, 24),
        (200, 60),
        (300, 5),
        (80, 300),
    ];

    for (width, height) in corners {
        for app in [&busy, &picker_app(1)] {
            let rows = screen(app, width, height);
            assert_eq!(
                rows.len(),
                usize::from(height),
                "{width}x{height}: the frame lost rows, so something drew outside it"
            );
        }
    }

    for height in [5u16, 12, 24] {
        for width in 1..=48u16 {
            let rows = screen(&busy, width, height);
            assert_eq!(
                rows.len(),
                usize::from(height),
                "{width}x{height}: the palette sweep must not panic or lose a row"
            );
        }
    }
}
