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

use crate::provider::{Model, StreamEvent, ToolState, Usage};

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

impl Action {
    /// The two-or-three-word name the keymap help prints for this action.
    ///
    /// Derived from the variant rather than written into the help table beside it. A
    /// separate display string is a second copy of the same fact, and the failure it
    /// produces is silent: the help would describe an action the reducer does not
    /// produce, which is exactly the drift `ACCEPTANCE.md` §9 forbids ("the keybinding
    /// help and the reducer read the **same** table"). The match has no wildcard arm
    /// on purpose — a new `Action` variant cannot compile without a name, so it cannot
    /// reach the footer as a blank.
    pub fn label(self) -> &'static str {
        match self {
            Action::None => "no action",
            Action::Quit => "quit",
            Action::Submit => "submit",
            Action::Cancel => "cancel",
            Action::ScrollUp => "scroll up",
            Action::ScrollDown => "scroll down",
            Action::OpenPicker => "pick a model",
            Action::PickerNext => "next model",
            Action::PickerPrev => "previous model",
            Action::PickerAccept => "take the model",
        }
    }
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

impl Status {
    /// Whether a turn is in flight.
    ///
    /// A predicate rather than the comparison it replaces, because the comparison
    /// *was* a defect: `Enter` asked `status == Idle`, `apply_event(Done)` set `Done`,
    /// nothing returned to `Idle`, and the TUI sent one turn and then went inert
    /// forever (B26, `ACCEPTANCE.md` §13). The lifecycle is stated in terms of a turn
    /// running (`DESIGN.md` §12), so the code asks the question in the same terms
    /// rather than restating the lifecycle as a chain of equality tests.
    pub fn is_running(&self) -> bool {
        matches!(self, Status::Running)
    }
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

    /// The rows for one backend's catalog, in the catalog's own order.
    ///
    /// Pure on purpose: no catalog read, no clock, no `provider::list_models` call.
    /// The catalog itself costs a vendor CLI on a cache miss (`src/tui.rs`'s
    /// `fill_picker`), so a picker that could only be filled *through* a backend would
    /// be a picker no offline test could fill — and a test that reached a backend
    /// would spend subscription quota to prove a layout.
    ///
    /// The label leads with the `backend/model` spelling `--model` accepts, because a
    /// row sets **both** `App::backend` and `App::model` (`DESIGN.md` §12) and a bare
    /// model id would not say which subscription the row switches to — the loop merges
    /// every backend's catalog into one list, so two rows can otherwise read alike.
    pub fn from_models(backend: &str, models: &[Model]) -> Self {
        let items = models
            .iter()
            .map(|model| {
                // An empty display name must not be rendered as an empty bracket, and a
                // display name equal to the id adds nothing the label does not already
                // say. Same rule as the loop's own rows, so a `display`-less catalog
                // does not look different depending on which path built it.
                let display = model.display.trim();
                let label = if display.is_empty() || display == model.id {
                    format!("{}/{}", backend, model.id)
                } else {
                    format!("{}/{} ({})", backend, model.id, display)
                };
                PickerItem {
                    backend: backend.to_string(),
                    model: model.id.clone(),
                    label,
                }
            })
            .collect();

        Self {
            items,
            // The first row, not a remembered one: the catalog is new, so any previous
            // highlight points at a row that no longer exists.
            selected: 0,
        }
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

/// The state a keymap row is documented in.
///
/// The keymap is modal — `Enter`, `Tab`, `Up`, `Down` and `Esc` each mean two things —
/// so a row is only a contract together with the state it holds in. Naming that state
/// as data rather than in prose is what lets the keymap test *drive* every row through
/// [`reduce`] instead of reasoning about it, which is the difference between a checked
/// table and a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingState {
    /// Nothing running, no picker, and text in the prompt.
    PromptReady,
    /// A turn is in flight.
    Running,
    /// The model picker is open.
    PickerOpen,
}

/// One row of the frozen keymap (`DESIGN.md` §12).
///
/// Data, not an overlay's code path: `Input` has no help-toggle variant and must not
/// get one, so the help is a footer rendered from this table rather than a screen a key
/// opens. `action` is the [`Action`] variant the row must produce, not a description of
/// it — the test drives the row through [`reduce`] and compares against this field, so a
/// row that lies about its key or its action fails rather than reading plausibly.
#[derive(Debug, Clone, Copy)]
pub struct KeyBinding {
    /// The key, as the help prints it.
    pub keys: &'static str,
    /// What the key does, printed after [`Self::keys`].
    pub description: &'static str,
    /// The [`Input`] the row is driven with.
    pub input: Input,
    /// The state the row holds in.
    pub state: BindingState,
    /// The [`Action`] [`reduce`] must return for this row.
    pub action: Action,
}

/// The whole keymap, as the one table the help and the reducer both read.
///
/// A `const` rather than a function returning a slice, because every field is a
/// `&'static str`, an [`Input`] or an [`Action`] and all of them are const-constructible
/// — so the table is a value with no call to make and nothing to allocate per frame. A
/// `fn keybindings()` would add an indirection that buys nothing.
///
/// Rows repeat their key where the keymap is modal (`Enter` submits and accepts, `Tab`
/// opens and then no-ops, `Esc` cancels and then quits), because folding those together
/// is how the second half of a modal row goes missing from the help.
///
/// The descriptions are deliberately terse: sixteen rows have to fit a two-row footer
/// (`render::HELP_ROWS`), so a phrase like "submit the prompt unless a turn is running"
/// would push two other rows off the screen. The state lives in [`Self::state`], which
/// is what the test uses.
pub const KEYBINDINGS: &[KeyBinding] = &[
    KeyBinding {
        keys: "char",
        description: "type at the cursor",
        input: Input::Char('x'),
        state: BindingState::PromptReady,
        action: Action::None,
    },
    KeyBinding {
        keys: "backspace",
        description: "delete a character",
        input: Input::Backspace,
        state: BindingState::PromptReady,
        action: Action::None,
    },
    KeyBinding {
        keys: "left",
        description: "cursor left",
        input: Input::Left,
        state: BindingState::PromptReady,
        action: Action::None,
    },
    KeyBinding {
        keys: "right",
        description: "cursor right",
        input: Input::Right,
        state: BindingState::PromptReady,
        action: Action::None,
    },
    KeyBinding {
        keys: "ctrl-x",
        description: "ignored",
        input: Input::Ctrl('x'),
        state: BindingState::PromptReady,
        action: Action::None,
    },
    KeyBinding {
        keys: "ctrl-c",
        description: "quit always",
        input: Input::Ctrl('c'),
        state: BindingState::PromptReady,
        action: Action::Quit,
    },
    KeyBinding {
        keys: "enter",
        description: "send prompt",
        input: Input::Enter,
        state: BindingState::PromptReady,
        action: Action::Submit,
    },
    KeyBinding {
        keys: "enter",
        description: "take model, picker open",
        input: Input::Enter,
        state: BindingState::PickerOpen,
        action: Action::PickerAccept,
    },
    KeyBinding {
        keys: "tab",
        description: "open picker",
        input: Input::Tab,
        state: BindingState::PromptReady,
        action: Action::OpenPicker,
    },
    KeyBinding {
        keys: "tab",
        description: "no-op, picker open",
        input: Input::Tab,
        state: BindingState::PickerOpen,
        action: Action::None,
    },
    KeyBinding {
        keys: "up",
        description: "scroll up",
        input: Input::Up,
        state: BindingState::PromptReady,
        action: Action::ScrollUp,
    },
    KeyBinding {
        keys: "down",
        description: "scroll down",
        input: Input::Down,
        state: BindingState::PromptReady,
        action: Action::ScrollDown,
    },
    KeyBinding {
        keys: "up",
        description: "previous model, picker open",
        input: Input::Up,
        state: BindingState::PickerOpen,
        action: Action::PickerPrev,
    },
    KeyBinding {
        keys: "down",
        description: "next model, picker open",
        input: Input::Down,
        state: BindingState::PickerOpen,
        action: Action::PickerNext,
    },
    KeyBinding {
        keys: "esc",
        description: "cancel turn, close picker",
        input: Input::Esc,
        state: BindingState::Running,
        action: Action::Cancel,
    },
    KeyBinding {
        keys: "esc",
        description: "quit when nothing to cancel",
        input: Input::Esc,
        state: BindingState::PromptReady,
        action: Action::Quit,
    },
];

/// The keymap as the single line of help the footer draws, derived from
/// [`KEYBINDINGS`].
///
/// The words have exactly one source. A help string written here instead would be a
/// second copy of the keymap that nothing could contradict — which is the shape of the
/// claim `ACCEPTANCE.md` §9 rejects: the assertion has to be that every documented row
/// *produces* its documented action through [`reduce`], not that a help string exists.
///
/// Returns a `String` rather than a `&'static str` because a `const` cannot concatenate
/// the table's fields; the footer is redrawn per frame either way, and the allocation is
/// one line of text.
pub fn help_text() -> String {
    KEYBINDINGS
        .iter()
        .map(|binding| format!("{} {}", binding.keys, binding.description))
        .collect::<Vec<_>>()
        .join(" · ")
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
/// | `Enter`, picker open | `PickerAccept` | `backend` **and** `model` taken from the highlight; picker closed |
/// | `Enter`, prompt submittable, status **not `Running`** | `Submit` | `Entry::User` pushed; prompt cleared; `Status::Running` |
/// | `Enter`, otherwise | `None` | nothing |
/// | `Tab`, picker closed | `OpenPicker` | picker opened **empty** for the caller to fill |
/// | `Tab`, picker already open | `None` | nothing — a re-open would discard the catalog the caller filled in |
/// | `Up` / `Down`, picker open | `PickerPrev` / `PickerNext` | highlight moved and clamped |
/// | `Up` / `Down`, otherwise | `ScrollUp` / `ScrollDown` | scroll moved, never below zero |
/// | `Esc`, a turn is running or a picker is open | `Cancel` | picker closed; `Idle` **only if the turn was running** |
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
///
/// Three rows above were wrong in the first freeze, and each is worth a sentence
/// because the reason is not visible from the code (B26, `ACCEPTANCE.md` §13).
///
/// * **`Enter` submits whenever the status is not `Running`, not only from `Idle`.**
///   The original table read `Idle` and `apply_event(Done)` set `Done`, with nothing
///   anywhere returning to `Idle` — so after the first answer `Enter` was inert
///   forever and the TUI could hold exactly one turn. [`Status::is_running`] exists so
///   the question is asked once, in the lifecycle's own terms.
/// * **`Tab` while the picker is open is a no-op.** `Tab` opens the picker empty for
///   the caller to fill from a catalog that is read asynchronously, so a second `Tab`
///   that reset it to [`Picker::default`] threw that catalog away — a picker that
///   emptied itself under a repeated keypress, with nothing on screen to explain it.
/// * **`Esc` returns to `Idle` only when a turn was running.** Closing an overlay is
///   not a status change, and the status it used to clear was a `Failed` reason the
///   user had not read yet.
///
/// `Submit` also pushes the submitted text as an [`Entry::User`] **before** it clears
/// the prompt: `Action::Submit` carries no payload, and the prompt is gone by the time
/// `reduce` returns — so the caller cannot record the question without re-deriving
/// state the reducer already decided, and the TUI would show answers with no questions
/// (`ACCEPTANCE.md` §9, stage B's checklist: "both `User` and `Assistant` entries
/// appear"). `src/tui.rs` must not also append a `User` entry, or the turn is recorded
/// twice.
pub fn reduce(app: &mut App, input: Input) -> Action {
    // `Ctrl('c')` is answered first and unconditionally: it is the one key that
    // must work while a turn is running and while the picker is modal, because it
    // is the user's way out when nothing else responds. `Esc` cannot serve that
    // role — `Esc` is `Cancel` in exactly those two states.
    if matches!(input, Input::Ctrl('c')) {
        app.should_quit = true;
        return Action::Quit;
    }

    match input {
        Input::Ctrl(_) => Action::None,
        Input::Char(c) => {
            app.prompt = insert_char(&app.prompt, app.cursor, c);
            // Clamped, not `+= 1`: a prompt assigned from outside (a test, a
            // restored draft) can leave the cursor past the end, and an
            // unclamped cursor is the offset the multi-byte editing bug rides in
            // on.
            app.cursor = (app.cursor + 1).min(char_count(&app.prompt));
            Action::None
        }
        Input::Backspace => {
            if app.cursor > 0 {
                app.prompt = remove_char_before(&app.prompt, app.cursor);
                app.cursor -= 1;
            }
            Action::None
        }
        Input::Left => {
            app.cursor = app.cursor.saturating_sub(1).min(char_count(&app.prompt));
            Action::None
        }
        Input::Right => {
            app.cursor = (app.cursor + 1).min(char_count(&app.prompt));
            Action::None
        }
        Input::Enter => {
            // The picker is modal, so it takes `Enter` first: accepting a row and
            // submitting a prompt on the same key would send a turn the user was
            // still choosing a model for.
            if app.picker.is_some() {
                if let Some(item) = app.picker.as_ref().and_then(Picker::current).cloned() {
                    app.backend = item.backend;
                    app.model = Some(item.model);
                }
                app.picker = None;
                return Action::PickerAccept;
            }

            if !app.status.is_running() && app.prompt_is_submittable() {
                let submitted = std::mem::take(&mut app.prompt);
                app.cursor = 0;
                app.transcript.push(Entry::User(submitted));
                app.status = Status::Running;
                return Action::Submit;
            }
            Action::None
        }
        Input::Esc => {
            // Read both facts before either is changed: one `Cancel` covers the turn and
            // the overlay, but only a running turn may move the status.
            let running = app.status.is_running();
            let overlay_open = app.picker.is_some();
            if running || overlay_open {
                app.picker = None;
                // `Idle` only when a turn was running. Clearing a `Failed` reason as a
                // side effect of closing the picker destroyed the one thing the user
                // still had to read (B26).
                if running {
                    app.status = Status::Idle;
                }
                Action::Cancel
            } else {
                app.should_quit = true;
                Action::Quit
            }
        }
        Input::Tab => {
            if app.picker.is_some() {
                // A no-op, not a second `OpenPicker`: the picker is opened **empty** for
                // the caller to fill from a catalog read asynchronously, so resetting it
                // here discards the rows the loop has just fetched (B26).
                return Action::None;
            }
            // Frozen: opened **empty**. The caller fills it from the backend's
            // catalog, which needs I/O and therefore cannot happen here.
            app.picker = Some(Picker::default());
            Action::OpenPicker
        }
        Input::Up => {
            if let Some(picker) = app.picker.as_mut() {
                picker.move_by(-1);
                Action::PickerPrev
            } else {
                app.scroll = app.scroll.saturating_sub(1);
                Action::ScrollUp
            }
        }
        Input::Down => {
            if let Some(picker) = app.picker.as_mut() {
                picker.move_by(1);
                Action::PickerNext
            } else {
                // Saturating, not `+= 1`: `u16` overflow panics in debug, and
                // `lib.rs` denies indexing that can panic for the same reason.
                // The upper bound is render's, because only render knows how many
                // lines the transcript occupies.
                app.scroll = app.scroll.saturating_add(1);
                Action::ScrollDown
            }
        }
    }
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
///
/// Two details the table above states as prose, implemented as narrowly as
/// written. `Init` with no `conversation_id` **keeps** an id already recorded: the
/// field exists so a later `--resume` has something to take, and a resumed turn
/// that reported `None` must not erase it. And `Done` re-sends `text` only when
/// nothing in the current turn streamed any answer, where "the current turn" is
/// the transcript since the last [`Entry::User`] — not "the last entry", because a
/// turn that ends on a tool step would otherwise append the whole reply a second
/// time, and not "the transcript", because that would suppress turn 2's text
/// merely because turn 1 had streamed.
pub fn apply_event(app: &mut App, event: &StreamEvent) {
    match event {
        StreamEvent::Init {
            conversation_id,
            tools,
            ..
        } => {
            if let Some(id) = conversation_id {
                app.vendor_id = Some(id.clone());
            }
            // The id and the tool count are both shown here so a user can see
            // what `--json` would have told them; the count is the vendor's own
            // catalog, which is also how the UI can say the tools are the
            // vendor's (`VENDOR_TOOL_NOTE`).
            let conversation = conversation_id
                .clone()
                .unwrap_or_else(|| "(none reported)".to_string());
            let notice = format!(
                "{} · conversation {} · {} tools available",
                app.backend,
                conversation,
                tools.len()
            );
            app.transcript.push(Entry::Notice(notice));
        }
        StreamEvent::TextDelta(delta) => {
            let appended = match app.transcript.last_mut() {
                Some(Entry::Assistant(text)) => {
                    text.push_str(delta);
                    true
                }
                _ => false,
            };
            if !appended {
                app.transcript.push(Entry::Assistant(delta.clone()));
            }
        }
        StreamEvent::ToolStep { name, state, .. } => {
            app.tool_events.push((name.clone(), *state));
            app.transcript.push(Entry::Tool {
                name: name.clone(),
                state: *state,
            });
        }
        // A tool the **vendor is asking pie to run** (M9). It is displayed as a tool row
        // because that is what the user sees, but it is recorded as `Active`: a call is
        // the start of a step, and only a later `ToolStep` can say how it ended. The
        // arguments are deliberately not rendered — they are the vendor's payload, not a
        // sentence, and a transcript full of JSON is one nobody reads.
        StreamEvent::ToolUse { name, .. } => {
            app.tool_events.push((name.clone(), ToolState::Active));
            app.transcript.push(Entry::Tool {
                name: name.clone(),
                state: ToolState::Active,
            });
        }
        StreamEvent::Usage(usage) => match app.last_usage.as_mut() {
            Some(existing) => existing.merge(usage.clone()),
            None => app.last_usage = Some(usage.clone()),
        },
        StreamEvent::Done { text, .. } => {
            app.status = Status::Done;
            // An empty terminal text is not an answer, and pushing it would leave
            // a blank line in the transcript that no input produced.
            if !text.is_empty() && !this_turn_streamed_an_answer(app) {
                app.transcript.push(Entry::Assistant(text.clone()));
            }
        }
        // Deliberately ignored, and the reason is in the table above: a `Notice`
        // per unparsed banner line would fill the transcript with vendor chatter
        // during a normal turn, and `PIE_LOG` already holds them.
        StreamEvent::Other { .. } | StreamEvent::Unparsed(_) => {}
    }
}

/// Characters in `s`, which is **not** `s.len()`.
///
/// `App::cursor` is a character offset, so every edit has to be expressed in
/// characters. The byte version of this function is the classic bug here: on a
/// prompt of `é` or `日本語`, `s.len()` is larger than the number of characters,
/// and an offset taken from it lands inside a character — `String::insert` either
/// inserts in the wrong place or panics on the non-boundary. Measured in
/// characters, both are impossible.
fn char_count(s: &str) -> usize {
    s.chars().count()
}

/// `s` with `c` inserted before character `index`, rebuilt rather than spliced.
///
/// Rebuilding keeps this free of a byte offset that could be a non-boundary — the
/// reason `char_count` exists — and an out-of-range `index` inserts at the end
/// rather than panicking, because `lib.rs` denies indexing that can panic.
fn insert_char(s: &str, index: usize, c: char) -> String {
    let mut out = String::with_capacity(s.len() + c.len_utf8());
    out.extend(s.chars().take(index));
    out.push(c);
    out.extend(s.chars().skip(index));
    out
}

/// `s` without the character before `index`; `index == 0` removes nothing.
///
/// Removing by character position rather than by byte is what makes `Backspace`
/// after `Char('é')` remove the whole character and not half of it.
fn remove_char_before(s: &str, index: usize) -> String {
    s.chars()
        .enumerate()
        .filter(|(i, _)| *i + 1 != index)
        .map(|(_, c)| c)
        .collect()
}

/// Whether the turn in progress has already put answer text on screen.
///
/// Looks only at the entries since the last [`Entry::User`], which is what makes
/// the `Done` rule per-turn: scanning the whole transcript would stop suppressing
/// the duplicate after the first turn, and looking only at the last entry would
/// re-append the answer after a trailing tool step. A delta that carried no text
/// does not count — an empty `Assistant` entry would otherwise swallow the only
/// copy of the answer.
fn this_turn_streamed_an_answer(app: &App) -> bool {
    app.transcript
        .iter()
        .rev()
        .take_while(|entry| !matches!(entry, Entry::User(_)))
        .any(|entry| matches!(entry, Entry::Assistant(text) if !text.is_empty()))
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
