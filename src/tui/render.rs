//! Drawing: one screen, from one [`App`], with no state of its own.
//!
//! `DESIGN.md` §12. `Frame` is ratatui's and appears in exactly one signature in
//! this crate — here. Everything this module needs is already in [`App`], which
//! is what lets every assertion be made against an in-memory buffer through
//! ratatui's `TestBackend` instead of against a terminal.
//!
//! Three rules this file obeys deliberately, each because there is no terminal
//! in the test:
//!
//! * **Nothing here measures text.** Wrapping and the display width of a CJK or
//!   emoji character are left to ratatui's reflow, which is grapheme-aware. A
//!   hand-rolled `&text[..n]` is a *byte* index into a `String`, so on `日本語`
//!   it panics at a non-char boundary, and on a wide character at the last
//!   column it draws half a glyph. That is the *panic on external input*
//!   `ACCEPTANCE.md` §10 lists as a bug category to hunt for.
//! * **The caret is drawn into the buffer, not set as the hardware cursor.**
//!   Positioning a real cursor requires the display width of everything before
//!   it — that is, exactly the measurement above. Inverting a cell needs none,
//!   and `TestBackend` can see it, so the caret is assertable.
//! * **`crossterm` is not imported here.** This crate depends on `crossterm
//!   0.29` while ratatui 0.29 depends on `crossterm 0.28`, so a
//!   `crossterm::style::Color` is a *different type* from the one ratatui takes
//!   and does not compile. Nothing this module draws needs one.
//!
//! A `\n` inside a span is also handled here rather than expected to work:
//! ratatui's reflow *drops* zero-width symbols, so a multi-line answer left as
//! one span arrives as a single run-on row. Every entry's text is split on
//! `\n` first (see [`entry_rows`]).

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::provider::ToolState;

use super::app::{self, App, Entry, Picker, Status, VENDOR_TOOL_NOTE};

/// Rows reserved for the prompt.
///
/// Three rather than one so a long prompt wraps into view instead of hiding its
/// tail. That matters because the caret is drawn *inside* the text: a prompt
/// whose tail is off-screen is a prompt whose caret is off-screen. Wrapping, not
/// a horizontal scroll, is what makes the caret reachable without this module
/// knowing a single glyph's width.
const PROMPT_ROWS: u16 = 3;

/// Rows reserved for the status line: one, because it is a single summary row
/// (`ACCEPTANCE.md` §9 — target, session, status, last usage).
const STATUS_ROWS: u16 = 1;

/// The most rows the help footer may ever reserve: one per documented keymap row.
///
/// A ceiling rather than a spec: the footer wraps the table across as many rows as the
/// width needs and stops here, because past one row per binding there is nothing left to
/// show that is not a single word per row. Public so `tests/tui_keys.rs` can bound the
/// region it reads (`render::help_rows` is what the layout actually uses, and a test that
/// hard-coded a row count would keep passing after the layout changed — which is how the
/// two-row footer outlived the criterion it failed).
pub const HELP_ROWS: u16 = app::KEYBINDINGS.len() as u16;

/// Rows the transcript keeps, whatever the help would like.
///
/// The footer is an aid; the transcript is the product. Without a floor, a narrow terminal
/// lets the wrapped keymap take the whole frame and leave nothing to read — complete help,
/// useless screen. Eight covers the densest thing the transcript has to show at 40 columns:
/// a question, the blank row after it, a two-row answer, the blank after that, and a tool
/// row that itself wraps to three (`tests/tui_render.rs`'s wide-character case needs seven
/// and this clears it by one). It is the floor [`help_pane_height`] enforces **while the
/// frame can pay for it**; below that the footer keeps [`MIN_HELP_ROWS`] instead, because
/// past that point something has to give and neither answer is free.
const MIN_TRANSCRIPT_ROWS: u16 = 8;

/// Rows the help keeps when the frame is too short for [`MIN_TRANSCRIPT_ROWS`].
///
/// One, so the keymap is never *wholly* absent from a frame that has room for the prompt and
/// the status line: a footer that disappears at twelve rows would be the acceptance criterion
/// satisfied on paper and not on screen, which is exactly the shape of the defect being fixed
/// here. A single row shows the first few keys and clips the rest, which is clipping — the
/// stated behaviour below the width the whole table fits in.
const MIN_HELP_ROWS: u16 = 1;

/// Width of the picker overlay, as a share of the screen.
///
/// A share rather than a fixed width so the overlay is usable at 40 columns and
/// still framed at 200.
const PICKER_WIDTH_PERCENT: u16 = 60;

/// Height of the picker overlay, as a share of the screen.
const PICKER_HEIGHT_PERCENT: u16 = 50;

/// The narrowest a wrapped pane may be drawn at.
///
/// Not a style choice — a workaround with a reproduction behind it. At exactly
/// two columns, ratatui 0.29's word wrapper emits a wrapped line *wider* than the
/// area when the text contains a wide glyph (a long unbreakable run, e.g.
/// `"x" * 19 + "語🚀 tail"`, reproduces it), and `Paragraph`'s renderer then
/// indexes outside the buffer and panics with `index outside of buffer`
/// (`ratatui-0.29.0/src/buffer/buffer.rs:253`). Two columns cannot show a
/// conversation anyway, so the panes are left blank there rather than risking the
/// panic — and `a_wide_character_at_any_width_neither_panics_nor_loses_a_marker`
/// sweeps every width from 1 up, so removing this guard is caught.
const MIN_WRAPPED_COLUMNS: u16 = 3;

/// Draw the whole screen.
///
/// Frozen signature. The layout this must produce, and the reason for each part
/// (`ACCEPTANCE.md` §9, stage B):
///
/// * the transcript, scrolled by [`App::scroll`], where a long conversation
///   scrolls rather than overflowing, and where an offset too large to draw is clamped
///   rather than handed to ratatui to overflow (B29, `ACCEPTANCE.md` §13);
/// * the prompt line, showing [`App::prompt`] and [`App::cursor`];
/// * a status line naming [`App::target`], [`App::session`], [`App::status`] and
///   the most recent [`App::last_usage`];
/// * the keymap help, from [`app::KEYBINDINGS`] rather than from a string here, so the
///   documented keys cannot drift from the implemented ones, and wrapped here so the
///   pane can reserve the rows it actually needs — at 80x24, an ordinary terminal, the
///   whole table fits (`ACCEPTANCE.md` §9);
/// * the model picker overlay when [`App::picker`] is open, listing every row and
///   marking the highlighted one;
/// * [`VENDOR_TOOL_NOTE`] beside tool steps, and never the implication that pie
///   ran them.
///
/// `render` takes `&App`, not `&mut App`: it must be impossible for drawing to
/// change state, so that a screenshot cannot be the thing that moves the
/// conversation on.
pub fn render(frame: &mut Frame, app: &App) {
    // The keymap is wrapped once, here, and both the layout and the widget then use those
    // same rows: the pane reserves the height it is about to draw, so it can never be a row
    // short of its own content (`ACCEPTANCE.md` §9 — "every keybinding is documented in
    // help" at 80x24, not only at ~200 columns, which is what a reserved-two-rows guess
    // amounted to).
    let area = frame.area();
    let help_lines = help_lines(area.width);
    let help_height = help_pane_height(help_lines.len(), area.height);

    // `areas` destructures a fixed-size array, so there is no index to get wrong
    // and no panic when the terminal is smaller than the layout asks for: a short
    // terminal gets zero-height rects, and every widget below renders nothing
    // into an empty rect rather than drawing outside it.
    //
    // The order is the contract: transcript, prompt, help, status, top to bottom — and
    // the status line is **last** because `tests/tui_render.rs` asserts that the status
    // survives at the bottom row whatever the scroll offset, which is how a scrolled
    // transcript is proved not to have painted over it. The help sits between them and
    // yields to the transcript rather than the other way round: `help_pane_height` caps
    // it so the transcript always keeps [`MIN_TRANSCRIPT_ROWS`].
    let [transcript, prompt, help, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(PROMPT_ROWS),
        Constraint::Length(help_height),
        Constraint::Length(STATUS_ROWS),
    ])
    .areas(area);

    // The wrapped panes are skipped below `MIN_WRAPPED_COLUMNS`; the status
    // line is not wrapped at all (it truncates), so it always draws.
    if transcript.width >= MIN_WRAPPED_COLUMNS {
        frame.render_widget(transcript_widget(app, transcript.height), transcript);
    }
    if prompt.width >= MIN_WRAPPED_COLUMNS {
        frame.render_widget(prompt_widget(app), prompt);
    }
    if help.width >= MIN_WRAPPED_COLUMNS {
        frame.render_widget(help_widget(help_lines), help);
    }
    frame.render_widget(status_widget(app), status);

    if let Some(picker) = &app.picker {
        picker_overlay(frame, picker);
    }
}

/// The transcript: every entry, scrolled by [`App::scroll`].
///
/// Scrolling is delegated to `Paragraph::scroll` *with* wrapping on, whose offset
/// counts wrapped display rows (ratatui 0.29 `paragraph.rs`, `render_text`). That
/// is the unit the frozen `App::scroll` documents ("lines from the top"), so a
/// single long answer occupies the several rows it actually needs and `scroll`
/// moves the window by rows the user can see.
///
/// `height` is the transcript pane's own height, and it is passed in only so the
/// offset can be clamped against it: `Paragraph` computes `offset + area.height`
/// and panics with `attempt to add with overflow` (`paragraph.rs:483`) once that
/// leaves `u16`, which is what B29 (`ACCEPTANCE.md` §13) records and what makes
/// S10's "a scroll past the end is an empty window rather than a panic" true rather
/// than aspirational. The clamp lives **here** because only `render` knows the
/// pane's height, which is the same reason `reduce` has no ceiling for the offset.
fn transcript_widget(app: &App, height: u16) -> Paragraph<'static> {
    Paragraph::new(transcript_rows(app))
        .wrap(Wrap { trim: true })
        .scroll((app.scroll.min(max_scroll_offset(height)), 0))
}

/// The largest offset `Paragraph::scroll` can be given without overflowing.
///
/// ratatui adds the pane's height to the offset before drawing, so the last safe
/// value is `u16::MAX - height` and anything above it panics inside ratatui rather
/// than drawing nothing. `saturating_sub`, not `-`: a terminal is never 65 535 rows
/// tall, but a subtraction here is the very class of expression this function exists
/// to remove.
fn max_scroll_offset(height: u16) -> u16 {
    u16::MAX.saturating_sub(height)
}

/// The rows the keymap help occupies in a `width` × `height` frame.
///
/// The layout calls it, and so does `tests/tui_keys.rs` — which has to read the footer
/// **by position** (the rows directly above the status line) rather than by looking for
/// the words it expects, or it would pass on a footer built from anything that happened
/// to contain them. A shared function is what keeps the position and the assertion from
/// drifting apart; a second copy of the arithmetic in the test would be the same guess
/// the two-row footer was.
pub fn help_rows(width: u16, height: u16) -> u16 {
    help_pane_height(help_lines(width).len(), height)
}

/// The height the helper above and [`render`] both use, from a row count already made.
///
/// Three caps, tightest wins: what the table needs, [`HELP_ROWS`] (one row per binding),
/// and what is left once the prompt, the status line and [`MIN_TRANSCRIPT_ROWS`] have had
/// their share. `saturating_sub` rather than `-` for the last one: a terminal shorter than
/// the fixed panes is a real state (a window manager resizing), and an arithmetic panic in
/// a draw call is a crashed UI, which is what this module's guards exist to prevent.
///
/// When even that share is gone the footer still takes [`MIN_HELP_ROWS`], but only if the
/// frame has room for the fixed panes: the order of the last two lines is the whole priority
/// — the transcript first, and the keymap never less than one row rather than none.
fn help_pane_height(wanted: usize, height: u16) -> u16 {
    let fixed = PROMPT_ROWS.saturating_add(STATUS_ROWS);
    let floor = fixed.saturating_add(MIN_TRANSCRIPT_ROWS);

    let want = u16::try_from(wanted).unwrap_or(u16::MAX).min(HELP_ROWS);
    let leftover = height.saturating_sub(floor);
    let at_least = MIN_HELP_ROWS.min(height.saturating_sub(fixed));

    want.min(leftover.max(at_least))
}

/// The keymap, wrapped to `width` columns, one [`Line`] per row.
///
/// Wrapped **here**, and deliberately not by ratatui's `Wrap`: a pane that wraps its own
/// content cannot say how tall it needs to be, so the layout could only reserve a guess —
/// and two guessed rows fitted the whole sixteen-row table only from about 200 columns,
/// which is how `ACCEPTANCE.md` §9's *every keybinding is documented in help* came to hold
/// only on a terminal nobody has. Rows this module builds are rows this module has counted.
///
/// The break is greedy at whitespace and the width of each token comes from ratatui's own
/// [`Span::width`] — the same grapheme-aware measure it draws with — so no byte index and no
/// hand-rolled `char` width enters the arithmetic. A single token wider than the pane sits
/// alone on its row and is clipped when drawn, rather than being split: this is a footer,
/// and half a word of help is not more help than none.
fn help_lines(width: u16) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row = String::new();

    for token in app::help_text().split_whitespace() {
        if row.is_empty() {
            row.push_str(token);
            continue;
        }
        let used = Span::raw(row.as_str()).width();
        if used + 1 + Span::raw(token).width() <= usize::from(width) {
            row.push(' ');
            row.push_str(token);
        } else {
            // `mem::take` rather than a `clone`: the row is moved into the output and the
            // buffer starts empty for the next one, with no second allocation per row.
            rows.push(help_line(std::mem::take(&mut row)));
            row.push_str(token);
        }
    }
    if !row.is_empty() {
        rows.push(help_line(row));
    }

    rows
}

/// One help row, dimmed.
///
/// The style lives here rather than on the `Paragraph` so no `Wrap` is needed to apply it:
/// `help_lines` has already decided where the rows break.
fn help_line(text: String) -> Line<'static> {
    Line::styled(text, Style::default().add_modifier(Modifier::DIM))
}

/// The keymap help, drawn from rows [`help_lines`] already wrapped.
///
/// No `Wrap` here, on purpose: these rows are the rows that were counted, and re-wrapping
/// them would put the pane's height and its content back out of step — the defect this
/// fix removes. A pane shorter than the table clips the tail, which is the accepted
/// behaviour below the width at which the whole map fits.
fn help_widget(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
}

/// Every transcript entry as the rows it occupies, top to bottom.
///
/// A blank row separates entries, so an answer does not run straight into the
/// next question with nothing but a `> ` to tell them apart.
fn transcript_rows(app: &App) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::with_capacity(app.transcript.len() + 1);
    for entry in &app.transcript {
        if !rows.is_empty() {
            rows.push(Line::default());
        }
        rows.extend(entry_rows(entry));
    }
    rows
}

/// One entry, as one or more rows.
///
/// Text is split on `\n` rather than handed to ratatui whole: reflow drops
/// zero-width symbols, and `\n` is one, so a multi-line answer would render as a
/// single run-on row with no sign that anything was lost.
///
/// The tool row is the one place [`VENDOR_TOOL_NOTE`] appears, and it appears
/// *because* `Entry::Tool` means the vendor ran the tool (`RISKS.md` §1). Nothing
/// else in this file may suggest pie has tool cards of its own — that would claim
/// a capability delegate mode does not have.
fn entry_rows(entry: &Entry) -> Vec<Line<'static>> {
    match entry {
        Entry::User(text) => {
            prefixed_rows("> ", text, Style::default().add_modifier(Modifier::BOLD))
        }
        Entry::Assistant(text) => prefixed_rows("", text, Style::default()),
        Entry::Notice(text) => prefixed_rows(
            "* ",
            text,
            Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
        ),
        Entry::Tool { name, state } => vec![Line::from(vec![
            Span::styled(
                format!("[{} {}] ", one_row(name), tool_state_label(*state)),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw(VENDOR_TOOL_NOTE),
        ])],
    }
}

/// `text` split into rows, each carrying `prefix`.
///
/// An empty line inside `text` is kept as a row carrying only the prefix: a
/// paragraph break in an answer is content, not padding.
fn prefixed_rows(prefix: &str, text: &str, style: Style) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|part| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(2);
            if !prefix.is_empty() {
                spans.push(Span::styled(prefix.to_string(), style));
            }
            spans.push(Span::styled(part.to_string(), style));
            Line::from(spans)
        })
        .collect()
}

/// The prompt, with the caret drawn at [`App::cursor`].
///
/// `App::cursor` counts **characters** (`src/tui/app.rs`), so the split is by
/// `char` and never by byte: `&app.prompt[..cursor]` panics on the second
/// character of `日本語`, and that slice is the obvious way to write this.
///
/// The caret is `REVERSED` on the character at the cursor, and a `REVERSED` space
/// when the cursor is at the end. It is drawn into the buffer rather than placed
/// as the terminal's cursor because the buffer is what `TestBackend` can assert:
/// a render whose caret only exists in the hardware is a render whose caret no
/// gate can see.
fn prompt_widget(app: &App) -> Paragraph<'static> {
    let caret = Style::default().add_modifier(Modifier::REVERSED);

    let before: String = app.prompt.chars().take(app.cursor).collect();
    let at: String = app.prompt.chars().skip(app.cursor).take(1).collect();
    let after: String = app
        .prompt
        .chars()
        .skip(app.cursor.saturating_add(1))
        .collect();

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(
        "> ".to_string(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    spans.push(Span::raw(before));
    if at.is_empty() {
        // Past the end of the text, so there is no character to invert.
        spans.push(Span::styled(" ".to_string(), caret));
    } else {
        spans.push(Span::styled(at, caret));
    }
    spans.push(Span::raw(after));

    // `trim: false` here, unlike the transcript: leading spaces in a prompt are
    // the user's, and a prompt that silently eats them looks like a bug in edit
    // handling.
    Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false })
}

/// The status line: target, session, status, and what the last turn cost.
///
/// Usage is shown as `not reported` when the backend sent none, rather than as
/// zeros: a turn that reported nothing and a turn that really cost nothing are
/// different facts, and `Usage::merge` treats zero as "not reported" for the same
/// reason.
fn status_widget(app: &App) -> Paragraph<'static> {
    let usage = match &app.last_usage {
        Some(usage) => format!(
            "tokens in {} out {} think {} total {}",
            usage.input_tokens,
            usage.output_tokens,
            usage.thinking_tokens,
            usage.computed_total()
        ),
        None => "tokens not reported".to_string(),
    };

    let spans = vec![
        Span::styled(
            "pie".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}  session {}  ",
            app.target(),
            one_row(&app.session)
        )),
        Span::styled(status_label(&app.status), status_style(&app.status)),
        Span::raw(format!("  {usage}")),
    ];

    Paragraph::new(Line::from(spans))
}

/// The status as one word, because the status line is one row.
fn status_label(status: &Status) -> String {
    match status {
        Status::Idle => "idle".to_string(),
        Status::Running => "running".to_string(),
        Status::Done => "done".to_string(),
        Status::Failed(reason) => format!("failed: {}", one_row(reason)),
    }
}

/// A colour per state, so a failure is visible without reading the row.
fn status_style(status: &Status) -> Style {
    match status {
        Status::Idle => Style::default().add_modifier(Modifier::DIM),
        Status::Running => Style::default().add_modifier(Modifier::BOLD),
        Status::Done => Style::default().fg(Color::Green),
        Status::Failed(_) => Style::default().fg(Color::Red),
    }
}

/// The model picker, drawn over everything else.
///
/// Every row is rendered, not only the highlighted one: the point of a picker is
/// to see the alternatives, and a list that showed one row would make "which
/// model am I about to switch to" unanswerable.
///
/// [`Clear`] before the list so the overlay is an overlay — without it the
/// transcript underneath shows through the panel and the highlighted row becomes
/// unreadable, which is a rendering fault no assertion on the list itself would
/// catch.
fn picker_overlay(frame: &mut Frame, picker: &Picker) {
    let popup = centered(frame.area(), PICKER_WIDTH_PERCENT, PICKER_HEIGHT_PERCENT);
    if popup.is_empty() {
        // A terminal too small to frame the overlay: draw nothing rather than a
        // panel of zero size whose border would be the whole screen.
        return;
    }
    frame.render_widget(Clear, popup);

    let items: Vec<ListItem<'static>> = picker
        .items
        .iter()
        .map(|item| ListItem::new(item.label.clone()))
        .collect();

    let list = List::new(items)
        .block(Block::bordered().title("models — enter selects, esc closes"))
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !picker.items.is_empty() {
        // `Picker::selected` is documented as meaningless while `items` is empty,
        // and a `List` highlight past the end of the list is an index into
        // nothing, so it is clamped rather than trusted.
        state.select(Some(
            picker.selected.min(picker.items.len().saturating_sub(1)),
        ));
    }

    frame.render_stateful_widget(list, popup, &mut state);
}

/// A rect of `width_percent` × `height_percent`, centred in `area`.
fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let [popup] = Layout::vertical([Constraint::Percentage(height_percent)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Percentage(width_percent)])
        .flex(Flex::Center)
        .areas(popup);
    popup
}

/// The tool state as the short word that fits on a transcript row.
fn tool_state_label(state: ToolState) -> &'static str {
    match state {
        ToolState::Active => "active",
        ToolState::Done => "done",
        ToolState::Error => "error",
    }
}

/// Collapse every whitespace run in `text` to a single space.
///
/// The status line and the tool name are single rows, and a `\n` inside a `Span`
/// is not a line break in ratatui: it is dropped. A vendor error carrying a
/// newline would therefore weld two sentences together mid-row. Collapsing first
/// keeps it one honest row.
fn one_row(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
