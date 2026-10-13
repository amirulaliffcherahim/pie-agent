//! Drawing: one screen, from one [`App`], with no state of its own.
//!
//! `DESIGN.md` §12, and §21.2 for the shell this module now draws. The shell, top to
//! bottom:
//!
//! ```text
//! transcript (scrolls)                     <- Constraint::Min, everything left over
//!   / palette (an overlay over its last rows, above the composer)
//! composer  (bordered, fixed)              <- the border carries App::status
//! keymap help                              <- render::help_rows rows, see below
//! footer    (one row, the bottom of it)    <- folder, session, model, usage, cost, status
//! ```
//!
//! The composer and the footer are outside the transcript's scroll region, and that is
//! the whole point of the split: `tests/tui_render.rs` draws one `App` at two scroll
//! offsets and asserts the rows below the transcript are byte-identical. A layout that
//! scrolled everything together would move the composer off the bottom of the screen,
//! which is the `pi` behaviour §21.2 copies ("keeps the editor and status area fixed
//! while the transcript scrolls").
//!
//! `Frame` is ratatui's and appears in exactly one signature in this crate — here.
//! Everything this module needs is already in [`App`], which is what lets every
//! assertion be made against an in-memory buffer through ratatui's `TestBackend`
//! instead of against a terminal.
//!
//! Three rules this file obeys deliberately, each because there is no terminal
//! in the test:
//!
//! * **Nothing here measures text.** Wrapping and the display width of a CJK or
//!   emoji character are left to ratatui's reflow, which is grapheme-aware. A
//!   hand-rolled `&text[..n]` is a *byte* index into a `String`, so on `日本語`
//!   it panics at a non-char boundary, and on a wide character at the last
//!   column it draws half a glyph. That is the *panic on external input*
//!   `ACCEPTANCE.md` §10 lists as a bug category to hunt for. Where this module does
//!   measure — the keymap rows and the palette's row budget — it measures with
//!   ratatui's own [`Span::width`], the same grapheme-aware measure it draws with.
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
//!
//! Two more rules are new with the shell, and both are the same rule: **a field this
//! module cannot fill says so rather than showing a zero, and something this module
//! declines to draw says so rather than being silently absent.**
//!
//! * The footer prints `tokens not reported` / `cost not reported` when `App` has no
//!   usage, because `Usage` uses zero to mean "not reported" (`src/provider.rs`) and a
//!   drawn `$0.00` would be an invented figure.
//! * The palette prints how many rows it is **not** showing (`ACCEPTANCE.md` §14's S13:
//!   a list that scrolls with no indicator that it is scrollable is a missing
//!   affordance), and a tool block states how many output lines it truncated
//!   (`DESIGN.md` §21.5, the I5/B22 rule).

use std::time::Duration;

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::paths;
use crate::provider::ToolState;

use super::app::{self, App, Entry, Picker, Status, VENDOR_TOOL_NOTE};
use super::slash::{self, SlashCommand};

/// Rows the composer occupies: a top border, one text row, a bottom border.
///
/// The interior is **one** row rather than the three an unbordered prompt had, and the
/// caret stays reachable anyway because the prompt paragraph is scrolled to the row the
/// caret is on (see [`caret_row`]). The box is three rows rather than one because the
/// border is where [`Status`] is drawn: a one-row composer has no border to carry it.
///
/// Three, not four, because the help footer has a floor of its own: at 40 × 24 the whole
/// keymap needs the twelve rows that are left once this box, the footer and
/// [`MIN_TRANSCRIPT_ROWS`] have had their share, and a taller composer would push the
/// keymap's last rows off a terminal people use (`tests/tui_keys.rs`).
const COMPOSER_ROWS: u16 = 3;

/// Rows reserved for the footer: one, because it is a single summary row
/// (`DESIGN.md` §21.2 — folder, session, model, tokens, cost, status).
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
/// One, so the keymap is never *wholly* absent from a frame that has room for the composer
/// and the footer: a footer that disappears at twelve rows would be the acceptance criterion
/// satisfied on paper and not on screen, which is exactly the shape of the defect being fixed
/// here. A single row shows the first few keys and clips the rest, which is clipping — the
/// stated behaviour below the width the whole table fits in.
const MIN_HELP_ROWS: u16 = 1;

/// Interior rows the palette overlay will ever ask for.
///
/// A ceiling rather than a share of the screen: an overlay that grew with the terminal would
/// be a menu, not an overlay, and §21.2 puts it *above* the composer precisely so the
/// transcript underneath stays readable. Eight shows every row of a filtered list a user
/// narrows by typing, and the rows beyond that are stated rather than silently dropped.
const MAX_PALETTE_ROWS: u16 = 8;

/// Output lines an expanded tool block will show before it starts counting.
///
/// The block is a summary, not a log: past a handful of lines the reader is scrolling a
/// transcript to read a tool's output, and the whole output is one `/tools run` away. What
/// matters is that the cut is **stated** — see [`tool_block`].
const TOOL_OUTPUT_ROWS: usize = 6;

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
/// conversation anyway, so the wrapped panes are left blank there rather than risking the
/// panic — and `a_wide_character_at_any_width_neither_panics_nor_loses_a_marker`
/// sweeps every width from 1 up, so removing this guard is caught.
const MIN_WRAPPED_COLUMNS: u16 = 3;

/// The rows each region of the shell occupies in one frame.
///
/// Returned rather than recomputed in the test, for the reason [`help_rows`] exists: a test
/// that hard-codes "the composer is rows 21 to 23" passes on a layout that moved it, and
/// only a reader looking at the rectangle would notice. `tests/tui_render.rs` reads the rows
/// it asserts on through this, so the region it checks and the region `render` reserved
/// cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Regions {
    /// The transcript: every row above the composer, scrolled by [`App::scroll`].
    pub transcript: Rect,
    /// The composer box: top border, one prompt row, bottom border.
    pub composer: Rect,
    /// The wrapped keymap.
    pub help: Rect,
    /// The footer: one row, the bottom of the frame.
    pub footer: Rect,
}

/// The regions a `width` × `height` frame is divided into.
///
/// A free function of the two numbers, so a test can ask for the layout of a terminal it is
/// about to draw without building a `Frame` — the same reason [`help_rows`] is public.
pub fn regions(width: u16, height: u16) -> Regions {
    layout(Rect::new(0, 0, width, height)).0
}

/// Draw the whole screen.
///
/// Frozen signature. The layout this must produce, and the reason for each part
/// (`ACCEPTANCE.md` §20, stage B; `DESIGN.md` §21.2):
///
/// * the transcript, scrolled by [`App::scroll`], where a long conversation
///   scrolls rather than overflowing, and where an offset too large to draw is clamped
///   rather than handed to ratatui to overflow (B29, `ACCEPTANCE.md` §13);
/// * the composer, a bordered box **fixed at the bottom** with the prompt inside it, whose
///   border carries [`App::status`] — the claim is asserted by drawing two statuses and
///   comparing the border cells, because a border that is constant satisfies "the border
///   reflects the status" on nothing but the sentence;
/// * the palette, drawn **above** the composer, from [`App::palette_rows`] per frame;
/// * the keymap help, from [`app::KEYBINDINGS`] rather than from a string here, so the
///   documented keys cannot drift from the implemented ones, and wrapped here so the
///   pane can reserve the rows it actually needs — at 80x24, an ordinary terminal, the
///   whole table fits;
/// * the footer, naming the folder, the session, the target, the last usage and its cost,
///   and saying "not reported" where a backend reported nothing;
/// * the model picker overlay when [`App::picker`] is open, listing every row and
///   marking the highlighted one;
/// * [`VENDOR_TOOL_NOTE`] beside tool steps, and never the implication that pie
///   ran them.
///
/// `render` takes `&App`, not `&mut App`: it must be impossible for drawing to
/// change state, so that a screenshot cannot be the thing that moves the
/// conversation on. The one value read from outside `App` is [`paths::pie_home`], for the
/// footer's folder field, which [`App`] does not carry — read, never written, so the
/// property above still holds.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let (shell, help) = layout(area);

    // The wrapped panes are skipped below `MIN_WRAPPED_COLUMNS`; the footer
    // is not wrapped at all (it truncates), so it always draws.
    if shell.transcript.width >= MIN_WRAPPED_COLUMNS {
        frame.render_widget(
            transcript_widget(app, shell.transcript.height),
            shell.transcript,
        );
    }
    if shell.composer.width >= MIN_WRAPPED_COLUMNS {
        composer_widget(frame, app, shell.composer);
    }
    // The palette is drawn *after* the transcript it covers and *before* the picker: it is
    // an overlay on the bottom of the scroll region (the position §21.2 names, above the
    // composer), and the picker is the other modal surface. `reduce` cannot open both, so
    // the order only decides which one a future state would draw on top.
    palette_overlay(frame, app, shell.transcript);
    if shell.help.width >= MIN_WRAPPED_COLUMNS {
        frame.render_widget(help_widget(help), shell.help);
    }
    frame.render_widget(footer_widget(app), shell.footer);

    if let Some(picker) = &app.picker {
        picker_overlay(frame, picker);
    }
}

/// The regions, plus the keymap rows the help pane will draw.
///
/// Wrapping the keymap once and handing the same rows to the layout *and* the widget is what
/// keeps the pane from being a row short of its own content (`ACCEPTANCE.md` §9 — "every
/// keybinding is documented in help" at 80x24, not only at ~200 columns, which is what a
/// reserved-two-rows guess amounted to).
fn layout(area: Rect) -> (Regions, Vec<Line<'static>>) {
    let help = help_lines(area.width);
    let help_height = help_pane_height(help.len(), area.height);

    // The heights are computed here and handed over as four rigid lengths rather than as one
    // `Min` plus three `Length`s, and the reason is a defect this test file caught: at five rows
    // the four constraints summed to six, and ratatui's solver resolved the oversubscription by
    // shrinking the composer to two rows — top border, bottom border, and **no prompt row**, so
    // the draft and its caret were gone from a frame that still looked like a layout. The
    // remainder belongs to the transcript, and only the transcript: every other region gets what
    // it asked for, so the sum is the frame exactly and there is no slack for the solver to
    // distribute somewhere a user would notice.
    let footer = STATUS_ROWS.min(area.height);
    let composer = COMPOSER_ROWS.min(area.height.saturating_sub(footer));
    let transcript = area
        .height
        .saturating_sub(composer.saturating_add(help_height).saturating_add(footer));

    // `areas` destructures a fixed-size array, so there is no index to get wrong
    // and no panic when the terminal is smaller than the layout asks for: a short
    // terminal gets zero-height rects, and every widget below renders nothing
    // into an empty rect rather than drawing outside it.
    //
    // The order is the contract: transcript, composer, help, footer, top to bottom — and
    // the footer is **last** because `tests/tui_render.rs` asserts that the footer
    // survives at the bottom row whatever the scroll offset, which is how a scrolled
    // transcript is proved not to have painted over it. The help sits between the composer
    // and the footer, which is also the position `tests/tui_keys.rs` reads it from, and it
    // yields to the transcript rather than the other way round: `help_pane_height` caps it
    // so the transcript always keeps [`MIN_TRANSCRIPT_ROWS`] when the frame can pay for it.
    let [transcript, composer, help_area, footer] = Layout::vertical([
        Constraint::Length(transcript),
        Constraint::Length(composer),
        Constraint::Length(help_height),
        Constraint::Length(footer),
    ])
    .areas(area);

    (
        Regions {
            transcript,
            composer,
            help: help_area,
            footer,
        },
        help,
    )
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
/// **by position** (the rows directly above the last row) rather than by looking for
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
/// and what is left once the composer, the footer and [`MIN_TRANSCRIPT_ROWS`] have had
/// their share. `saturating_sub` rather than `-` for the last one: a terminal shorter than
/// the fixed panes is a real state (a window manager resizing), and an arithmetic panic in
/// a draw call is a crashed UI, which is what this module's guards exist to prevent.
///
/// When even that share is gone the footer still takes [`MIN_HELP_ROWS`], but only if the
/// frame has room for the fixed panes: the order of the last two lines is the whole priority
/// — the transcript first, and the keymap never less than one row rather than none.
fn help_pane_height(wanted: usize, height: u16) -> u16 {
    let fixed = COMPOSER_ROWS.saturating_add(STATUS_ROWS);
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
///
/// The tool arm is the one entry that is a **block** rather than a paragraph, so it is
/// built by [`tool_block`] rather than by [`prefixed_rows`]. The block's own doc says what
/// it would draw and what it draws today, which are not yet the same rows.
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
        Entry::Tool { name, state } => tool_block(name, *state, None, None, false),
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

/// One tool block: a header row, and the output rows when it is expanded.
///
/// `DESIGN.md` §21.5: one block per call carrying the name, the state
/// (`active` / `done` / `error`, from [`tool_state_label`]), the duration, and — when it is
/// expanded — the output. Collapsed by default, and two rules hold at both settings:
///
/// * **nothing is invented.** A duration nothing reported prints [`NO_DURATION`] rather
///   than a zero, `0ms` being a figure pie does not have.
/// * **nothing is hidden silently.** Collapsed, the header says the output is collapsed;
///   expanded, the rows beyond [`TOOL_OUTPUT_ROWS`] are replaced by a row that counts them.
///   The I5/B22 rule (§21.5): a truncated block that does not say so is a lie about the
///   output.
///
/// **What the frame passes today, and why it is not the whole block.** `Entry::Tool` carries
/// a name and a state — no duration, no output — and `App` has no field saying which block
/// is expanded, so [`entry_rows`] calls this with `None, None, false` and every tool row on
/// screen is the collapsed, duration-unknown form. The parameters are here, and asserted
/// (`tests/tui_render.rs` drives this function directly for the expanded and truncated
/// cases), so that the rendering is finished and checkable; the state it needs is an
/// `App`-side change this stage does not own. Stating that here rather than in a note is the
/// point: a test that asserted the expanded form *through* the frame would be asserting a
/// state nothing can reach.
pub fn tool_block(
    name: &str,
    state: ToolState,
    duration: Option<Duration>,
    output: Option<&str>,
    expanded: bool,
) -> Vec<Line<'static>> {
    let marker = if expanded { "▾" } else { "▸" };
    let mut header = format!(
        "{marker} tool: {}  {}  {}",
        one_row(name),
        tool_state_label(state),
        duration_text(duration)
    );
    if !expanded && output.is_some() {
        // Only when there *is* output. A collapsed block with nothing to expand must not
        // promise a detail the `Ctrl+O` would not reveal.
        header.push_str("  ·  output collapsed");
    }
    header.push_str(" · ");
    header.push_str(VENDOR_TOOL_NOTE);

    let mut rows = vec![Line::from(vec![Span::styled(
        header,
        Style::default().add_modifier(Modifier::DIM),
    )])];

    if !expanded {
        return rows;
    }

    let Some(output) = output else {
        rows.push(tool_output_row(
            "output not carried in this transcript".to_string(),
        ));
        return rows;
    };

    let lines: Vec<&str> = output.split('\n').collect();
    for line in lines.iter().take(TOOL_OUTPUT_ROWS) {
        rows.push(tool_output_row((*line).to_string()));
    }
    let hidden = lines.len().saturating_sub(TOOL_OUTPUT_ROWS);
    if hidden > 0 {
        // The statement the I5/B22 rule requires, and the reason the block counts what it
        // dropped instead of just dropping it.
        rows.push(tool_output_row(format!(
            "… {hidden} more output lines (truncated at {TOOL_OUTPUT_ROWS})"
        )));
    }

    rows
}

/// One indented row of a tool block's body.
fn tool_output_row(text: String) -> Line<'static> {
    Line::styled(
        format!("    {text}"),
        Style::default().add_modifier(Modifier::DIM),
    )
}

/// What the block prints where a duration would go and none was reported.
///
/// A constant, and public, so the frame's test and the block's own tests name the same
/// string: two spellings of "we do not know" is how one of them becomes a zero.
pub const NO_DURATION: &str = "duration not reported";

/// A duration as the short text a transcript row can hold.
///
/// The two shapes `DESIGN.md` §21.2 draws (`120ms`, `3.1s`), plus a minutes form for a
/// command that really did run long. `None` is [`NO_DURATION`], never `0ms`.
fn duration_text(duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return NO_DURATION.to_string();
    };
    let millis = duration.as_millis();
    if millis < 1_000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{}.{:01}s", millis / 1_000, (millis % 1_000) / 100)
    } else {
        let seconds = duration.as_secs();
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

/// The composer: a bordered box, fixed at the bottom, holding the prompt.
///
/// `DESIGN.md` §21.2: *"the composer's border carries status"* — `┌─ composer ──[border = status]──┐`
/// is the sketch, so the *border* is where the status is drawn and its title says what the box
/// is. The border's style comes from [`status_style`], the same style the footer's status word
/// uses, so the border cannot say `running` in the colour of a failure — which is what a second
/// `match` here would eventually do.
///
/// The reason the title is not the status: `Status::Failed` carries a vendor's reason, and a
/// reason long enough to reach the footer's own row would then be on two rows at once —
/// `a_failed_status_is_collapsed_onto_one_row` asserts it appears on exactly one, and the row
/// that has to keep it is the one that can wrap nothing (the footer truncates, the border
/// title clips silently).
///
/// The prompt is drawn into the box's interior rather than into the box's whole rect, which is
/// what keeps the caret off the border. The interior is one row (see [`COMPOSER_ROWS`]), so the
/// paragraph is scrolled to the row the caret is on: a prompt whose caret is off-screen is a
/// prompt that cannot be edited, which is the failure this scroll exists to prevent.
fn composer_widget(frame: &mut Frame, app: &App, area: Rect) {
    let style = status_style(&app.status);
    let block = Block::bordered().border_style(style).title(Span::styled(
        " composer ",
        Style::default().add_modifier(Modifier::DIM),
    ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width < MIN_WRAPPED_COLUMNS {
        // A one-row box, or one too narrow to wrap text in safely (`MIN_WRAPPED_COLUMNS`):
        // the border is still drawn, because it is where the status lives.
        return;
    }

    let offset = prompt_scroll_offset(app, inner.height);
    frame.render_widget(prompt_widget(app).scroll((offset, 0)), inner);
}

/// The row of the prompt the caret sits on, measured in the prompt's own `\n` breaks.
///
/// Characters are counted, never bytes: `App::cursor` is a character offset, and
/// `&app.prompt[..cursor]` is the byte slice that panics on `日本語`. The count matches
/// [`prompt_lines`] row for row, because that is the number of rows it has emitted so far.
///
/// This counts the prompt's explicit line breaks, which is what a pasted multi-line prompt
/// has. A single logical line long enough to *wrap* inside the composer is the one case where
/// the row count here is lower than the row ratatui draws, so the tail of such a line clips
/// with the caret — the accepted limit of a three-row composer (`DESIGN.md` §21.2 draws one
/// prompt row inside the border), and stated rather than discovered.
fn caret_row(app: &App) -> u16 {
    let before: String = app.prompt.chars().take(app.cursor).collect();
    u16::try_from(before.matches('\n').count()).unwrap_or(u16::MAX)
}

/// The offset that keeps [`caret_row`] inside an `interior`-row-tall composer.
///
/// Clamped against the pane as well as the arithmetic, for the same reason
/// [`max_scroll_offset`] exists: `Paragraph` adds the area height to the offset, and an
/// offset that leaves `u16` panics inside ratatui rather than drawing nothing (B29).
fn prompt_scroll_offset(app: &App, interior: u16) -> u16 {
    let rows_above = interior.saturating_sub(1);
    caret_row(app)
        .saturating_sub(rows_above)
        .min(max_scroll_offset(interior))
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
    // `trim: false` here, unlike the transcript: leading spaces in a prompt are
    // the user's, and a prompt that silently eats them looks like a bug in edit
    // handling.
    Paragraph::new(prompt_lines(app)).wrap(Wrap { trim: false })
}

/// The prompt as the rows it occupies, the caret inverted on the one it is on.
///
/// Split on the prompt's own `\n` breaks first, for the reason every other multi-line
/// string in this module is split: ratatui's reflow **drops** zero-width symbols, so a
/// pasted two-line prompt left as one `Line` arrives as a single run-on row — and
/// [`caret_row`], which counts those breaks, would then be counting rows that never reach
/// the screen. The rows below are rows this module has built, so the scroll offset and the
/// drawn output agree on what a row is.
///
/// Continuation rows are indented two columns so the text stays under the `> ` marker
/// rather than under the marker itself.
fn prompt_lines(app: &App) -> Vec<Line<'static>> {
    let caret = Style::default().add_modifier(Modifier::REVERSED);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::styled("> ".to_string(), dim)];
    let mut caret_drawn = false;

    for (index, ch) in app.prompt.chars().enumerate() {
        match (index == app.cursor, ch) {
            // The caret is on a line break, so there is no character to invert: it is a
            // space at the end of the row the break closes.
            (true, '\n') => {
                spans.push(Span::styled(" ".to_string(), caret));
                caret_drawn = true;
                rows.push(Line::from(std::mem::take(&mut spans)));
                spans.push(Span::styled("  ".to_string(), dim));
            }
            (true, at) => {
                spans.push(Span::styled(at.to_string(), caret));
                caret_drawn = true;
            }
            (false, '\n') => {
                rows.push(Line::from(std::mem::take(&mut spans)));
                spans.push(Span::styled("  ".to_string(), dim));
            }
            (false, at) => spans.push(Span::raw(at.to_string())),
        }
    }

    if !caret_drawn {
        // Past the end of the text — where the cursor sits most of the time — so there is
        // no character to invert.
        spans.push(Span::styled(" ".to_string(), caret));
    }
    rows.push(Line::from(spans));

    rows
}

/// The palette: the filtered registry rows, drawn over the transcript above the composer.
///
/// `DESIGN.md` §21.2 places it there (the position agy puts its model dropdown and its
/// announcements in), and §21.3 is what it draws: each row's **name**, its **argument hint**
/// and its **description**, with a marker on the highlighted row.
///
/// The rows come from [`App::palette_rows`] — that is, from [`slash::matching`] applied to
/// the prompt — **every frame**. The renderer keeps no list of its own, which is the rule
/// §21.3 inherits from §12's keymap: a cached copy is a second table free to disagree with
/// the registry, which is the B26 shape.
///
/// Two stated limits rather than two silent ones:
///
/// * **More rows than fit are counted.** `ACCEPTANCE.md` §14's S13 recorded the model picker
///   scrolling with no indicator that it was scrollable and called it a missing affordance;
///   the note row here says how many rows are not drawn, and `↑`/`↓` do move the highlight.
/// * **An empty match says so.** `/zzzz` leaves the overlay open with nothing to show, and an
///   empty box would leave a user unable to tell "nothing matched" from "the UI is stuck".
fn palette_overlay(frame: &mut Frame, app: &App, area: Rect) {
    if app.palette.is_none() {
        return;
    }

    let rows = app.palette_rows();
    let total = rows.len();
    let selected = app
        .palette
        .map(|palette| palette.selected.min(total.saturating_sub(1)))
        .unwrap_or(0);

    // Two rows of the panel are its border, and the panel is never taller than the room the
    // transcript can spare: an overlay that covered the composer would be a screen, and the
    // composer is the fixed half of the layout.
    let capacity = area.height.min(MAX_PALETTE_ROWS.saturating_add(2));
    if capacity < 3 {
        return;
    }
    let interior = usize::from(capacity - 2);

    // The window: every row when they all fit, otherwise the rows around the highlight plus
    // one row for the note that counts the rest.
    let (window, offset, hidden) = if total == 0 {
        (0usize, 0usize, 0usize)
    } else if total <= interior {
        (total, 0usize, 0usize)
    } else {
        let visible = interior.saturating_sub(1).max(1);
        let start = if selected >= visible {
            selected + 1 - visible
        } else {
            0
        };
        let start = start.min(total.saturating_sub(visible));
        (visible, start, total - visible)
    };

    let panel = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(capacity),
        width: area.width,
        height: capacity,
    };
    frame.render_widget(Clear, panel);

    let mut body: Vec<Line<'static>> = Vec::with_capacity(interior);
    for (index, row) in rows.iter().enumerate().skip(offset).take(window) {
        body.push(palette_line(row, index == selected));
    }
    if total == 0 {
        body.push(Line::styled(
            format!(
                "  nothing matches /{}",
                slash::filter_of(&app.prompt).unwrap_or_default()
            ),
            Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
    }
    if hidden > 0 {
        body.push(Line::styled(
            format!("  … {hidden} more of {total} rows — ↑/↓ moves the highlight"),
            Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
    }

    frame.render_widget(
        Paragraph::new(body).block(Block::bordered().title(Span::styled(
            palette_title(&app.prompt),
            Style::default().add_modifier(Modifier::DIM),
        ))),
        panel,
    );
}

/// One palette row: the marker, the invocation, and the description.
///
/// The invocation is built from the row's own [`SlashCommand`] fields
/// (`/{name} {argument}`), so the overlay cannot describe a command differently from the
/// table it came from — the same reason [`SlashCommand::menu_line`] exists.
fn palette_line(row: &SlashCommand, highlighted: bool) -> Line<'static> {
    let marker = if highlighted { "> " } else { "  " };
    let mut text = String::from(marker);
    text.push('/');
    text.push_str(row.name);
    if !row.argument.is_empty() {
        text.push(' ');
        text.push_str(row.argument);
    }
    text.push_str("  ·  ");
    text.push_str(row.description);

    // `REVERSED` as well as the marker, because a mark a screenshot cannot see is half an
    // affordance — the picker highlights its selected row the same way.
    let style = if highlighted {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    Line::styled(text, style)
}

/// The overlay's title: the filter the rows are being matched against.
fn palette_title(prompt: &str) -> String {
    match slash::filter_of(prompt) {
        Some(filter) if !filter.is_empty() => format!(" commands starting /{filter} "),
        _ => " commands ".to_string(),
    }
}

/// The footer: folder, session, model, last usage, cost, and the status.
///
/// `DESIGN.md` §21.2 names all five fields, and this row is the only one on screen at every
/// size — which is why the order is the order a *narrow* terminal can afford rather than the
/// order the sentence reads: the session and the model survive clipping, and the folder, the
/// least urgent of the five, is what a 20-column screen loses.
///
/// Usage is shown as `not reported` when the backend sent none, rather than as zeros: a turn
/// that reported nothing and a turn that really cost nothing are different facts, and
/// `Usage::merge` treats zero as "not reported" for the same reason. The cost follows the
/// same rule — `cost_usd_micros` is documented as zero-means-not-reported, and a permanent
/// `$0.00` implies pie measures cost when it does not (`TOKEN-ECONOMICS.md`).
fn footer_widget(app: &App) -> Paragraph<'static> {
    let spans = vec![
        Span::styled(
            "pie".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  session {}  ", one_row(&app.session))),
        Span::raw(format!("model {}  ", app.target())),
        Span::styled(status_label(&app.status), status_style(&app.status)),
        Span::raw(format!("  {}  ", usage_text(app))),
        Span::raw(format!("{}  ", cost_text(app))),
        Span::raw(format!("folder {}", folder_text())),
    ];

    Paragraph::new(Line::from(spans))
}

/// What the last turn cost, as tokens, or an honest blank.
///
/// A `Usage` whose every field is zero is a turn that reported nothing — the convention
/// `Usage::merge` follows — so it prints the blank rather than `tokens in 0 out 0 think 0`,
/// which is four invented figures wearing a real turn's clothes.
fn usage_text(app: &App) -> String {
    match &app.last_usage {
        Some(usage) if usage.computed_total() > 0 => format!(
            "tokens in {} out {} think {} total {}",
            usage.input_tokens,
            usage.output_tokens,
            usage.thinking_tokens,
            usage.computed_total()
        ),
        _ => "tokens not reported".to_string(),
    }
}

/// What the last turn cost, in dollars, or an honest blank.
///
/// Three cases, and the middle one is why this is a `match` rather than a format string:
/// zero is "not reported" (`Usage::cost_usd_micros`), a real cost too small to round to a
/// cent says `< $0.01` instead of the `$0.00` that would read as free, and everything else
/// prints to the cent. The `/status` sheet prints the same figure at four decimals
/// (`src/commands/tui.rs`), and the rule it shares with this row is the zero case.
fn cost_text(app: &App) -> String {
    match app.last_usage.as_ref().map(|usage| usage.cost_usd_micros) {
        Some(micros) if micros >= 10_000 => format!(
            "cost ${}.{:02}",
            micros / 1_000_000,
            (micros % 1_000_000) / 10_000
        ),
        Some(micros) if micros > 0 => "cost < $0.01".to_string(),
        _ => "cost not reported".to_string(),
    }
}

/// The folder the footer names: pie's own state directory.
///
/// The one value this module reads from outside [`App`], and it is [`paths::pie_home`] rather
/// than the working directory because that is what *folder* means everywhere else in pie:
/// `/status` prints `folder  {pie_home}` (`src/commands/tui.rs`), and `DESIGN.md` §21.2 names
/// `paths` as the field's owner. `App` has no folder field — the vendor's `Init` event carries
/// a `cwd` and `apply_event` does not record it — so the alternative to reading it is a lie.
/// It is **read**, never written: `render` still cannot change any state, which is the property
/// the `&App` signature exists to give.
///
/// The whole path, in the last field of the row, so a narrow terminal loses it first rather
/// than losing the session and the model to it.
fn folder_text() -> String {
    paths::pie_home().display().to_string()
}

/// The status as one word, because the footer is one row.
fn status_label(status: &Status) -> String {
    match status {
        Status::Idle => "idle".to_string(),
        Status::Running => "running".to_string(),
        Status::Done => "done".to_string(),
        Status::Failed(reason) => format!("failed: {}", one_row(reason)),
    }
}

/// A colour per state, so a failure is visible without reading the row.
///
/// Used twice: on the footer's status word and on the composer's border. Two callers, one
/// style, so the border cannot say `running` in the colour of a failure — which is what a
/// second `match` here would eventually do.
fn status_style(status: &Status) -> Style {
    match status {
        Status::Idle => Style::default().add_modifier(Modifier::DIM),
        Status::Running => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
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
///
/// `DESIGN.md` §21.5 names these states `running` / `ok` / `error` while
/// `provider::ToolState` names them `Active` / `Done` / `Error`, and the two vocabularies
/// meet here. The words below are the ones `tests/tui_render.rs` has pinned since M6
/// (`a_tool_step_carries_the_vendor_note` asserts `active`), so they stay until the frozen
/// witness and §21.5 are reconciled — a render that renamed them would turn a documented
/// preference into a red gate without the document being the thing that changed.
fn tool_state_label(state: ToolState) -> &'static str {
    match state {
        ToolState::Active => "active",
        ToolState::Done => "done",
        ToolState::Error => "error",
    }
}

/// Collapse every whitespace run in `text` to a single space.
///
/// The footer and the tool name are single rows, and a `\n` inside a `Span`
/// is not a line break in ratatui: it is dropped. A vendor error carrying a
/// newline would therefore weld two sentences together mid-row. Collapsing first
/// keeps it one honest row.
fn one_row(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
