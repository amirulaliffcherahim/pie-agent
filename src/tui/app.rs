//! TUI state, and the two pure functions that change it.
//!
//! `DESIGN.md` §12. The one rule that makes M6 testable at all: **no crossterm
//! type appears in this file.** A terminal key becomes an [`Input`], the reducer
//! turns `key → Action → state`, and every step is asserted without a terminal.
//! A `KeyEvent` parameter would drag the terminal into every test, which is why
//! [`Input`] exists rather than `ratatui`/`crossterm` re-exports.
//!
//! Nothing here spawns, reads a file, or is `async`. [`apply_event`] folds a
//! backend's [`StreamEvent`] into the transcript; the loop that actually drives a
//! turn lives in [`super`].

use crate::provider::{StreamEvent, ToolState, Usage};

/// One key, with no terminal behind it.
///
/// The crate's own type on purpose (`DESIGN.md` §12). The terminal loop maps
/// crossterm's `KeyEvent` into this and nothing else ever sees the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A printable character.
    Char(char),
    /// Return.
    Enter,
    /// Escape.
    Esc,
    /// Backspace.
    Backspace,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Tab.
    Tab,
    /// Ctrl + a character, lower-cased by the caller.
    Ctrl(char),
}

/// What the reducer decided the *caller* should do about an input.
///
/// State changes happen inside [`reduce`]; the action is only what the loop
/// cannot do for itself — spawn a turn, quit, or ask a backend for its catalog.
/// Splitting it this way is what keeps the reducer free of I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing for the caller to do; `reduce` may still have changed state.
    None,
    /// Leave the TUI.
    Quit,
    /// Send the prompt as a turn.
    Submit,
    /// Abort the in-flight turn, or close the picker.
    Cancel,
    /// Scroll the transcript up one line.
    ScrollUp,
    /// Scroll the transcript down one line.
    ScrollDown,
    /// Open the model picker. `reduce` opens it **empty**; the caller fills it
    /// from the backend's catalog, because that needs I/O.
    OpenPicker,
    /// Move the picker's highlight down.
    PickerNext,
    /// Move the picker's highlight up.
    PickerPrev,
    /// Take the highlighted picker row as the new model, and close the picker.
    PickerAccept,
}

/// What the conversation is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Waiting for input.
    Idle,
    /// A turn is in flight.
    Running,
    /// The last turn finished.
    Done,
    /// The last turn failed, with a short reason.
    Failed(String),
}

/// One line of the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Something the user typed and submitted.
    User(String),
    /// Answer text. Grows in place while a turn streams.
    Assistant(String),
    /// A tool step **the vendor CLI ran**. See [`VENDOR_TOOL_NOTE`].
    Tool {
        /// Tool name as the vendor reported it.
        name: String,
        /// Lifecycle state.
        state: ToolState,
    },
    /// pie talking about itself: a switch, a compaction, a failure.
    Notice(String),
}

/// The line the UI must show beside tool steps, because delegate mode means the
/// vendor runs its own tools and pie injects none (`RISKS.md` §1).
///
/// Frozen here rather than in `render.rs` so the transcript label, the picker
/// help and the M6 acceptance check all quote the same sentence: a UI that
/// implied pie has tool cards would be claiming a capability that does not
/// exist, and two different wordings would make that harder to notice.
pub const VENDOR_TOOL_NOTE: &str =
    "tool steps are the vendor's own — pie delegates, it runs none of its own";

/// One selectable row in the model picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    /// Backend id the model belongs to.
    pub backend: String,
    /// Bare model id to pass to the backend.
    pub model: String,
    /// Human-readable label for the row.
    pub label: String,
}

/// The model picker overlay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picker {
    /// Selectable rows, in display order.
    pub items: Vec<PickerItem>,
    /// Index of the highlighted row. Meaningless while `items` is empty, which
    /// is why `reduce` never moves it without checking.
    pub selected: usize,
}

impl Picker {
    /// The highlighted row, if there is one.
    pub fn current(&self) -> Option<&PickerItem> {
        self.items.get(self.selected)
    }

    /// Move the highlight by `delta`, clamped to the available rows.
    pub fn move_by(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }
}

/// Everything the UI shows and everything a key can change.
///
/// `tool_events` exists beside `transcript` rather than being derived from it:
/// the transcript is bounded and scrollable, while the tool list is a summary the
/// status line keeps, and deriving one from the other would tie the summary's
/// lifetime to the scrollback's.
#[derive(Debug, Clone)]
pub struct App {
    /// What the user has typed but not submitted.
    pub prompt: String,
    /// Cursor position within `prompt`, counted in **characters** rather than
    /// bytes, so a multi-byte character is edited as one unit.
    pub cursor: usize,
    /// Backend id, e.g. `codex`.
    pub backend: String,
    /// Bare model id, or `None` when the backend picks its own.
    pub model: Option<String>,
    /// pie's own session id for this conversation.
    pub session: String,
    /// The vendor's conversation id, once the backend has issued one. This is
    /// what a later `--resume` would take, not what `--session` takes.
    pub vendor_id: Option<String>,
    /// What the conversation is doing.
    pub status: Status,
    /// Everything shown, in order.
    pub transcript: Vec<Entry>,
    /// Tool steps seen this session, most recent last.
    pub tool_events: Vec<(String, ToolState)>,
    /// Scroll offset, in lines from the top.
    pub scroll: u16,
    /// Token accounting from the most recent turn, when the backend reported it.
    pub last_usage: Option<Usage>,
    /// The model picker, when it is open.
    pub picker: Option<Picker>,
    /// Set when the user asked to leave.
    pub should_quit: bool,
}

impl App {
    /// A fresh app for one conversation.
    pub fn new(
        backend: impl Into<String>,
        model: Option<String>,
        session: impl Into<String>,
    ) -> Self {
        Self {
            prompt: String::new(),
            cursor: 0,
            backend: backend.into(),
            model,
            session: session.into(),
            vendor_id: None,
            status: Status::Idle,
            transcript: Vec::new(),
            tool_events: Vec::new(),
            scroll: 0,
            last_usage: None,
            picker: None,
            should_quit: false,
        }
    }

    /// Whether the prompt has anything to send, ignoring surrounding space.
    pub fn prompt_is_submittable(&self) -> bool {
        !self.prompt.trim().is_empty()
    }

    /// The backend and model currently selected, as `provider/model` when a model
    /// is pinned.
    pub fn target(&self) -> String {
        match &self.model {
            Some(model) => format!("{}/{}", self.backend, model),
            None => self.backend.clone(),
        }
    }
}

/// Turn one key into a state change plus, when the caller must act, an [`Action`].
///
/// The frozen keybinging table (`DESIGN.md` §12). Every row is a contract with
/// stage D's acceptance check, so the table is the specification rather than a
/// description of the code:
///
/// | Input | Action | State change |
/// |---|---|---|
/// | `Char(c)` | `None` | insert `c` at the cursor |
/// | `Backspace` | `None` | delete the character before the cursor |
/// | `Left` / `Right` | `None` | move the cursor, clamped to the prompt |
/// | `Enter`, picker open | `PickerAccept` | highlight taken as the model; picker closed |
/// | `Enter`, prompt submittable, `Idle` | `Submit` | prompt cleared, `Status::Running` |
/// | `Enter`, otherwise | `None` | nothing |
/// | `Tab` | `OpenPicker` | picker opened **empty** for the caller to fill |
/// | `Up` / `Down`, picker open | `PickerPrev` / `PickerNext` | highlight moved and clamped |
/// | `Up` / `Down`, otherwise | `ScrollUp` / `ScrollDown` | scroll moved, never below zero |
/// | `Esc`, a turn is running or a picker is open | `Cancel` | picker closed, status back to `Idle` |
/// | `Esc`, nothing to cancel | `Quit` | `should_quit` |
/// | `Ctrl('c')` | `Quit` | `should_quit` |
/// | any other `Ctrl(..)` | `None` | nothing |
///
/// Two deliberate choices worth keeping. **`Enter` is refused unless the prompt
/// has non-whitespace text**, so a turn is never spent on an empty prompt — the
/// one input a user can hit by accident that costs real subscription quota. And
/// **`Up`/`Down` mean different things depending on the picker**, because the
/// picker is modal; overloading them is what keeps the keymap small enough to
/// document in full.
pub fn reduce(_app: &mut App, _input: Input) -> Action {
    // SCAFFOLD — stage A replaces this body with the table above and asserts
    // every row of it in `tests/tui_app.rs`. It returns `None` rather than
    // panicking because `lib.rs` denies `panic`/`unwrap`/`expect` outside tests,
    // and because a stub that cannot compile is worse than one that does nothing.
    Action::None
}

/// Fold one backend event into the app.
///
/// Frozen behaviour (`DESIGN.md` §12). The events that matter to a user are the
/// answer, the vendor's tool steps, and the terminal state; the rest is protocol.
///
/// * `Init` — records `conversation_id` in [`App::vendor_id`] and pushes a
///   [`Entry::Notice`] naming the backend's conversation and its tool count, so
///   the vendor id is visible without `--json`.
/// * `TextDelta` — appended to the **last** entry when it is already an
///   [`Entry::Assistant`], and otherwise to a new one. Appending rather than
///   replacing is what makes streaming visible; a transcript of one entry per
///   delta would grow without bound on a long answer.
/// * `ToolStep` — pushed to both [`App::tool_events`] and the transcript as an
///   [`Entry::Tool`]. The transcript row is what carries [`VENDOR_TOOL_NOTE`].
/// * `Usage` — recorded in [`App::last_usage`] via [`Usage::merge`], because a
///   later event may carry only some fields.
/// * `Done` — sets [`Status::Done`], and pushes `text` as an assistant entry
///   **only when nothing was streamed**. Answering with the terminal text after
///   deltas have already landed would duplicate the whole reply.
/// * `Other` and `Unparsed` — **ignored**, deliberately. They are protocol noise
///   (unknown vendor events, banner lines) and `PIE_LOG` already records them; a
///   `Notice` per unparsed line would fill the transcript with vendor chatter
///   during a normal turn.
pub fn apply_event(_app: &mut App, _event: &StreamEvent) {
    // SCAFFOLD — stage A replaces this body with the behaviour above and asserts
    // it in `tests/tui_app.rs`.
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The scaffold's own smoke test: the types exist and the frozen invariant
    /// holds for them. Stage A adds the behaviour.
    #[test]
    fn a_fresh_app_starts_idle_and_unsubmittable() {
        let app = App::new("codex", None, "s-1-2");
        assert_eq!(app.status, Status::Idle);
        assert!(!app.prompt_is_submittable());
        assert!(app.picker.is_none());
        assert_eq!(app.target(), "codex");
    }

    /// A whitespace-only prompt must not be submittable: it is the cheapest way
    /// to spend subscription quota on nothing.
    #[test]
    fn a_whitespace_only_prompt_is_not_submittable() {
        let mut app = App::new("codex", None, "s-1-2");
        app.prompt = "   \n\t ".to_string();
        assert!(!app.prompt_is_submittable());
    }

    #[test]
    fn the_target_names_the_model_when_one_is_pinned() {
        let app = App::new("agy", Some("gemini-3.8-flash".to_string()), "s-1-2");
        assert_eq!(app.target(), "agy/gemini-3.8-flash");
    }

    /// The picker clamps rather than wrapping or panicking, including while it is
    /// empty — the state `reduce` creates before the caller has filled it.
    #[test]
    fn the_picker_highlight_clamps_and_tolerates_being_empty() {
        let mut picker = Picker::default();
        picker.move_by(1);
        assert_eq!(picker.selected, 0, "an empty picker must not move or panic");
        assert!(picker.current().is_none());

        picker.items = vec![
            PickerItem {
                backend: "codex".into(),
                model: "gpt-6-luna".into(),
                label: "codex/gpt-6-luna".into(),
            },
            PickerItem {
                backend: "agy".into(),
                model: "gemini-3.8-flash".into(),
                label: "agy/gemini-3.8-flash".into(),
            },
        ];
        picker.move_by(1);
        assert_eq!(picker.selected, 1);
        picker.move_by(1);
        assert_eq!(picker.selected, 1, "the last row is the end of the list");
        picker.move_by(-5);
        assert_eq!(picker.selected, 0, "the first row is the start");
    }

    /// The `Usage::merge` rule (a zero field means "not reported") has to hold on
    /// the TUI path too, or a partial terminal event blanks a real figure.
    #[test]
    fn a_partial_usage_event_does_not_zero_what_was_already_reported() {
        let mut app = App::new("agy", None, "s-1-2");
        app.last_usage = Some(Usage {
            input_tokens: 100,
            thinking_tokens: 7,
            ..Usage::default()
        });
        let mut later = app.last_usage.clone().unwrap_or_default();
        later.merge(Usage {
            input_tokens: 120,
            ..Usage::default()
        });
        app.last_usage = Some(later);

        let usage = app.last_usage.unwrap_or_default();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.thinking_tokens, 7, "the omitted field survives");
    }
}
