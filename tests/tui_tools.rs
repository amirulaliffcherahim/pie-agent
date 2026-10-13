//! M12 **stage D** gate: `/tools` (`planning/ACCEPTANCE.md` §20, row D; contract
//! `planning/DESIGN.md` §21.4).
//!
//! The stage row, verbatim:
//!
//! > `list` reads the **resolved** `Tools` and reports enabled / disabled /
//! > declared-but-unbuilt; `run read {"file_path":…}` works against the same fixture
//! > workspace M11's tier uses; `run bash` with no workspace is refused **naming
//! > `tools.workspace`**; `run write` is refused as declared-but-unbuilt; and a `run`
//! > spawns **no vendor CLI** and writes no session row
//!
//! Four things shape what the assertions below are worth:
//!
//! * **Every answer is driven through the loop's own dispatch.** A test that called
//!   `crate::tui::tools::lines` directly would not notice a `/tools` row that stopped
//!   reaching it, so each one goes text → `effect_for` → `Effect::Local` →
//!   `answer_local`, which is the path a typed command takes and the path the palette's
//!   `Enter` takes.
//! * **The refusals are compared against M11's own sentences**, not against a substring
//!   picked to match whatever this stage wrote. `run` must *render* the tier's refusal;
//!   a copy of the check here would agree with a substring and disagree with the tier the
//!   moment either moved.
//! * **"No vendor CLI" is a counting stand-in**, not the absence of an error: stand-ins
//!   named `agy`, `codex` and `claude` sit first on `PATH` and record the fact they ran,
//!   with a control that proves the same mechanism catches a spawn that *does* happen.
//! * **Truncation is compared verbatim** against what the tier returned, so a renderer
//!   that quietly dropped the tier's own cap note fails here.
//!
//! Offline: no vendor CLI, no socket, no credential, no model turn. The mutating-tier
//! fixture writes nothing outside a `tempfile` directory, and `PIE_HOME` is pinned
//! whenever a test asserts that nothing was written (G3d runs the suite with `HOME`
//! redirected to an empty directory, so a test that inherited machine state would pass
//! here and fail there).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use pie::config::{Config, Tools};
use pie::session_log;
use pie::tool_loop::{ToolCall, ToolResult, ToolRunner};
use pie::tools::{ToolSet, ANSWERED_TOOLS};
use pie::tui::app::{Action, App};
use pie::tui::{answer_local, effect_for, slash, Effect};

// ---------------------------------------------------------------------------------------
// Driving the row
// ---------------------------------------------------------------------------------------

/// The lines `/tools …` answers with, through the dispatch the loop uses.
///
/// `Effect::Run` is a panic rather than a value: every case below is a `/tools`
/// invocation, and "starts no turn" is one of the criteria — a dispatch that sent this
/// text to a backend would spend real quota, so the assertion is that the effect is
/// `Local` at all, made here for every single call in this file.
fn ask(cfg: &Config, text: &str) -> Vec<String> {
    ask_with(cfg, text, Action::Submit)
}

/// [`ask`] with the front-end chosen: a typed command ([`Action::Submit`]) or the
/// palette's `Enter` on the settled row ([`Action::PaletteRun`]).
fn ask_with(cfg: &Config, text: &str, action: Action) -> Vec<String> {
    let command = match effect_for(action, text.to_string()) {
        Effect::Local(command) => command,
        other => panic!("`{text}` must answer in-process, not {other:?}"),
    };
    let mut app = App::new("codex", None, "s-1-2");
    let lines = answer_local(cfg, &mut app, &command);
    assert!(
        lines.iter().any(|line| !line.trim().is_empty()),
        "`{text}` answered with nothing, which looks exactly like a hang (M7's rule)"
    );
    lines
}

/// The line `list` prints for one tool, found by the name in its first column.
fn state_line(lines: &[String], name: &str) -> String {
    lines
        .iter()
        .find(|line| line.split_whitespace().next() == Some(name))
        .unwrap_or_else(|| panic!("`{name}` has no line in:\n{}", lines.join("\n")))
        .clone()
}

/// The config key a refusal names first, read out of the tier's own sentence.
///
/// The oracle for [`the_key_list_names_is_the_key_the_tier_refuses_on`]: the `tools.*`
/// word that appears first is the key whose change a user has to make to move that tool.
fn first_key(refusal: &str) -> String {
    let at = refusal
        .find("tools.")
        .unwrap_or_else(|| panic!("no config key in this refusal: {refusal}"));
    let rest = &refusal[at..];
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '.' || c == '_'))
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

/// One call straight through M11's router, so a refusal this gate compares with can be
/// obtained from the tier itself rather than from this stage's prose.
fn tier(cfg: &Config, name: &str, input: serde_json::Value) -> ToolResult {
    ToolSet::new(cfg.tools.clone()).run(&ToolCall {
        id: "tools-run".into(),
        name: name.into(),
        input,
    })
}

// ---------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------

/// How many lines the over-the-cap fixture has, and how wide each one is.
///
/// Multi-line on purpose: the tier appends its cap note at the **end** of the output, so
/// a single-line fixture would let a renderer that kept only the first line still look
/// verbatim. The break that proved it: `.lines().take(1)` passed the assertion below
/// until this fixture grew line breaks.
const LONG_FILE_LINES: usize = 100;
const LONG_FILE_WIDTH: usize = 10;

/// The cap the truncation test configures, big enough for the tier's own note to fit
/// inside it — a cap smaller than the note would truncate the note, which is a different
/// (and already documented) behaviour of the tier rather than of this row.
const SMALL_CAP: usize = 256;

/// ```
/// <base>/ws/notes.txt          the file a `read` call returns
/// <base>/ws/long.txt           LONG_FILE_BYTES of text, for the cap note
/// <base>/bin/<stand-in>        the executable a `bash` call runs, `.bat`/`#!/bin/sh`
/// <base>/marks/<name>-ran.txt  what it recorded — the side effect a refusal must not have
/// ```
struct Base {
    dir: tempfile::TempDir,
}

impl Base {
    fn new() -> Self {
        let base = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(base.ws()).unwrap();
        fs::create_dir_all(base.bin()).unwrap();
        fs::create_dir_all(base.marks()).unwrap();
        fs::write(base.ws().join("notes.txt"), "workspace-file\n").unwrap();
        fs::write(
            base.ws().join("long.txt"),
            format!("{}\n", "x".repeat(LONG_FILE_WIDTH)).repeat(LONG_FILE_LINES),
        )
        .unwrap();
        base
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn ws(&self) -> PathBuf {
        self.path().join("ws")
    }

    fn bin(&self) -> PathBuf {
        self.path().join("bin")
    }

    fn marks(&self) -> PathBuf {
        self.path().join("marks")
    }

    fn mark(&self, name: &str) -> PathBuf {
        self.marks().join(format!("{name}-ran.txt"))
    }

    /// A configuration whose `[tools]` section enables the read tier over [`Self::ws`].
    fn cfg(&self) -> Config {
        self.cfg_with(Tools {
            workspace: Some(self.ws()),
            ..Tools::default()
        })
    }

    fn cfg_with(&self, tools: Tools) -> Config {
        Config {
            tools,
            ..Config::default()
        }
    }
}

/// Write one stand-in executable: `.bat` on Windows, `#!/bin/sh` elsewhere, `+x` elsewhere.
///
/// One body with two `#[cfg]`-gated bindings rather than two gated functions: M11's stage
/// C shipped an outer `#[cfg(windows)]` on a function that also had a
/// `#[cfg(not(windows))]` arm, and the result was a function that did not exist on unix
/// while every Windows gate was green (gate G6 caught it). One body cannot have that
/// shape.
fn script(base: &Base, name: &str, windows_body: &str, unix_body: &str) -> PathBuf {
    let _ = (windows_body, unix_body);

    #[cfg(windows)]
    let path = base.bin().join(format!("{name}.bat"));
    #[cfg(not(windows))]
    let path = base.bin().join(name);

    #[cfg(windows)]
    let body = format!("@echo off\r\n{windows_body}");
    #[cfg(not(windows))]
    let body = format!("#!/bin/sh\n{unix_body}");

    fs::write(&path, body).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// A stand-in that records that it ran and prints one line, so a spawn has a side effect
/// to be counted by rather than an error to be argued from.
fn recorder(base: &Base, name: &str) -> PathBuf {
    let mark = base.mark(name);
    script(
        base,
        name,
        &format!(
            "echo ran>>\"{mark}\"\r\necho hello-from-the-stand-in\r\n",
            mark = mark.display()
        ),
        &format!(
            "echo ran >> \"{mark}\"\necho hello-from-the-stand-in\n",
            mark = mark.display()
        ),
    )
}

/// The stand-in an allow-listed `bash` call runs.
const STAND_IN: &str = "pie-stage-d-stand-in";

/// `PIE_HOME` and `PATH` pinned for one test, with a process-wide lock: `std::env` is
/// shared and the harness is threaded, so an unpinned mutation would let two tests in this
/// binary see each other's environment. Restored on drop, including on a panic.
struct EnvPin {
    home: Option<String>,
    path: Option<String>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl EnvPin {
    /// `pie_home` pinned when given; `prepend` placed first on `PATH` when given.
    fn new(pie_home: Option<&Path>, prepend: Option<&Path>) -> Self {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let home = std::env::var("PIE_HOME").ok();
        if let Some(pie_home) = pie_home {
            std::env::set_var("PIE_HOME", pie_home);
        }

        let path = std::env::var("PATH").ok();
        if let Some(prepend) = prepend {
            let mut dirs = vec![prepend.to_path_buf()];
            dirs.extend(std::env::split_paths(&path.clone().unwrap_or_default()));
            std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());
        }

        Self {
            home,
            path,
            _guard: guard,
        }
    }
}

impl Drop for EnvPin {
    fn drop(&mut self) {
        match self.home.take() {
            Some(value) => std::env::set_var("PIE_HOME", value),
            None => std::env::remove_var("PIE_HOME"),
        }
        match self.path.take() {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// `list` — the effective state, read from the resolved table
// ---------------------------------------------------------------------------------------

#[test]
fn list_reports_the_resolved_state_of_every_tool_the_tier_answers_for() {
    let base = Base::new();
    let lines = ask(&base.cfg(), "/tools list");

    // The names are walked from M11's own constant — the list the router matches on — so
    // a tool M11 starts answering for has to appear in this row without an edit in either
    // file, and dropping one of them fails here rather than passing quietly.
    assert!(
        ANSWERED_TOOLS.len() >= 5,
        "the oracle is empty, so the walk below proves nothing"
    );
    for name in ANSWERED_TOOLS {
        let line = state_line(&lines, name);
        assert!(!line.contains("unclassified"), "`{name}`: {line}");
    }

    // Enabled: the read tier over a configured workspace.
    for name in ["read", "ls", "grep", "find"] {
        let line = state_line(&lines, name);
        assert!(line.contains("enabled"), "`{name}`: {line}");
    }

    // Disabled by the tier's own key, and the key is named.
    let mut read_off = base.cfg();
    read_off.tools.read = false;
    let line = state_line(&ask(&read_off, "/tools list"), "read");
    assert!(
        line.contains("disabled") && line.contains("tools.read"),
        "the refusal must name the key that would enable it: {line}"
    );

    // Disabled by the suite's own decision, which is not a tier key: no root, nothing to
    // scope a read to. This is the read tier's first check, and it is what M11's own
    // refusal names for these four. `bash` is the case where the order differs — it gates
    // on `tools.bash` before the workspace, exactly as its tier does — and
    // `the_key_list_names_is_the_key_the_tier_refuses_on` is where that is asserted.
    let no_root = base.cfg_with(Tools::default());
    let lines = ask(&no_root, "/tools list");
    for name in ["read", "ls", "grep", "find"] {
        let line = state_line(&lines, name);
        assert!(
            line.contains("disabled") && line.contains("tools.workspace"),
            "`{name}` without a workspace: {line}"
        );
    }
    assert!(state_line(&lines, "bash").contains("tools.bash"));

    // ...and the same config flips back the moment the table does, which is what makes
    // "read from the resolved `Tools`" a behaviour rather than a sentence.
    assert!(state_line(&ask(&base.cfg(), "/tools list"), "read").contains("enabled"));
}

#[test]
fn the_declared_but_unbuilt_tier_is_not_reported_as_disabled() {
    let base = Base::new();
    let lines = ask(&base.cfg(), "/tools list");

    for name in ["write", "edit"] {
        let line = state_line(&lines, name);
        assert!(
            line.contains("not built") && line.contains(&format!("tools.{name}")),
            "`{name}` must be reported as declared and unbuilt, naming its key: {line}"
        );
        assert!(
            !line.contains("disabled"),
            "`{name}` is not merely off — setting its key enables nothing, and a user who \
             read `disabled` would go and set it: {line}"
        );
    }

    // The positive control: a tool that really is one key away from running reads as
    // `disabled`, so the two sentences are different sentences and not one sentence with a
    // different prefix.
    let mut read_off = base.cfg();
    read_off.tools.read = false;
    let off = state_line(&ask(&read_off, "/tools list"), "read");
    assert!(off.contains("disabled"), "{off}");

    // And setting the declared key changes nothing, because there is nothing behind it
    // (`ACCEPTANCE.md` §19, S18).
    let mut declared_set = base.cfg();
    declared_set.tools.write = true;
    declared_set.tools.edit = true;
    assert_eq!(
        state_line(&ask(&declared_set, "/tools list"), "write"),
        state_line(&lines, "write"),
        "`tools.write = true` must not change what `/tools list` says"
    );
    assert_eq!(
        state_line(&ask(&declared_set, "/tools list"), "edit"),
        state_line(&lines, "edit")
    );
}

#[test]
fn the_key_list_names_is_the_key_the_tier_refuses_on() {
    // The strongest form of "`list` reads the resolved `Tools`" that does not restate this
    // stage's own prose: for every gate on a tool, the key `/tools list` prints beside it
    // is the key the tier's refusal for that tool names first. A list written from a
    // document, or one that checked the keys in a different order from the tier, fails
    // here — and the second case is not hypothetical: `bash` gates on its own key before
    // the workspace while the read tier gates on the workspace first.
    let base = Base::new();
    let root = base.ws();
    let cases: [(&str, Tools, &str, serde_json::Value); 6] = [
        (
            "read, no workspace",
            Tools::default(),
            "read",
            serde_json::json!({"file_path": "notes.txt"}),
        ),
        (
            "read, tier off",
            Tools {
                workspace: Some(root.clone()),
                read: false,
                ..Tools::default()
            },
            "read",
            serde_json::json!({"file_path": "notes.txt"}),
        ),
        (
            "bash, no workspace",
            Tools {
                bash: true,
                bash_allow: vec!["echo".into()],
                ..Tools::default()
            },
            "bash",
            serde_json::json!({"command": "echo hi"}),
        ),
        (
            "bash, tier off",
            Tools {
                workspace: Some(root.clone()),
                ..Tools::default()
            },
            "bash",
            serde_json::json!({"command": "echo hi"}),
        ),
        (
            "bash, nothing allowed",
            Tools {
                workspace: Some(root.clone()),
                bash: true,
                ..Tools::default()
            },
            "bash",
            serde_json::json!({"command": "echo hi"}),
        ),
        (
            "write, no workspace",
            Tools::default(),
            "write",
            serde_json::json!({"file_path": "notes.txt"}),
        ),
    ];

    for (case, tools, name, input) in cases {
        let cfg = base.cfg_with(tools);
        let refusal = tier(&cfg, name, input);
        assert!(
            refusal.is_error,
            "{case}: this case must refuse, or it is not a gate: {}",
            refusal.output
        );
        let expected = first_key(&refusal.output);
        let line = state_line(&ask(&cfg, "/tools list"), name);
        assert!(
            line.contains(&expected),
            "{case}: `{name}` is gated on `{expected}` — the key its own refusal names — \
             and `/tools list` says: {line}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// `run` — one tool, no turn, and M11's refusals rather than a copy of them
// ---------------------------------------------------------------------------------------

#[test]
fn run_read_reads_the_fixture_workspace_and_names_the_tool_and_its_state() {
    let base = Base::new();
    let lines = ask(&base.cfg(), "/tools run read {\"file_path\":\"notes.txt\"}");

    assert_eq!(lines[0], "`read` — ok", "the tool and its state come first");
    assert!(
        lines.iter().any(|line| line.contains("workspace-file")),
        "the fixture's own bytes must come back, which is what proves the call went \
         through M11's tier rather than through a list of paths:\n{}",
        lines.join("\n")
    );

    // Relative to the configured root, not to pie's working directory: the same call with
    // a path that only exists relative to *this* process fails.
    let outside = ask(
        &base.cfg(),
        "/tools run read {\"file_path\":\"Cargo.toml\"}",
    );
    assert!(
        outside[0].contains("error"),
        "a path outside the workspace must not read: {outside:?}"
    );
}

#[test]
fn run_with_no_workspace_is_refused_in_m11s_own_words_naming_tools_workspace() {
    let base = Base::new();
    // `bash` is on and scoped here, and *still* nothing can run: without a root there is
    // nothing to scope it to, which is the suite's decision rather than the tier's. With
    // `bash` off the tier answers with its own key first, and that case is below.
    let no_root = base.cfg_with(Tools {
        bash: true,
        bash_allow: vec!["echo".into()],
        ..Tools::default()
    });

    for (name, input) in [
        ("read", serde_json::json!({"file_path": "notes.txt"})),
        ("bash", serde_json::json!({"command": "echo hi"})),
    ] {
        let rendered = ask(&no_root, &format!("/tools run {name} {input}")).join("\n");
        assert!(
            rendered.contains("tools.workspace"),
            "`{name}` without a workspace must name the key: {rendered}"
        );

        // The tier's sentence, not a paraphrase of it. A copy of the check here would
        // satisfy a substring assertion and still be a second implementation of it.
        let refusal = tier(&no_root, name, input.clone());
        assert!(refusal.is_error);
        assert!(
            rendered.contains(&refusal.output),
            "`/tools run {name}` must render M11's refusal rather than write its own:\n\
             rendered: {rendered}\n\
             tier:     {}",
            refusal.output
        );
    }
}

#[test]
fn run_write_is_refused_as_declared_but_unbuilt_with_m11s_sentence() {
    let base = Base::new();
    let cfg = base.cfg();
    let input = serde_json::json!({"file_path": "notes.txt"});
    let rendered = ask(&cfg, &format!("/tools run write {input}")).join("\n");

    assert!(
        rendered.contains("`write` — error"),
        "the mutating tier has no implementation to succeed with: {rendered}"
    );
    assert!(
        rendered.contains("tools.write") && rendered.contains("not implemented"),
        "the refusal must name the key and say the key enables nothing: {rendered}"
    );
    assert!(
        !rendered.contains("disabled"),
        "`write` is not disabled: it is unbuilt, which is a different sentence: {rendered}"
    );

    let refusal = tier(&cfg, "write", input);
    assert!(
        rendered.contains(&refusal.output),
        "the tier's own refusal must be the one rendered:\n{rendered}"
    );
}

#[test]
fn a_disabled_tier_is_refused_with_its_own_sentence_and_names_the_key() {
    let base = Base::new();
    let cfg = base.cfg(); // `bash` is off by default
    let input = serde_json::json!({"command": "echo hi"});

    let rendered = ask(&cfg, &format!("/tools run bash {input}")).join("\n");
    assert!(rendered.contains("tools.bash"), "{rendered}");
    assert!(
        rendered.contains(&tier(&cfg, "bash", input).output),
        "{rendered}"
    );
}

#[test]
fn a_malformed_or_missing_argument_is_a_stated_refusal_that_starts_no_turn() {
    let base = Base::new();
    let cfg = base.cfg();

    // The argument is named in each case, because "the argument `args` is missing" is
    // actionable and "could not run the tool" is not.
    let cases = [
        ("/tools run", "name"),
        ("/tools run read", "args"),
        ("/tools run read file_path=notes.txt", "args"),
        ("/tools run read {\"file_path\":", "args"),
        ("/tools frobnicate", "subcommand"),
    ];
    for (text, argument) in cases {
        let joined = ask(&cfg, text).join("\n");
        assert!(
            joined.contains(argument),
            "`{text}` must refuse by naming `{argument}`: {joined}"
        );
        assert!(
            !joined.contains("`read` — ok"),
            "`{text}` must not reach a tool: {joined}"
        );
    }

    // "Starts no turn" asserted where a turn would be started: the effect for every one of
    // those texts is in-process, and the control shows the same call does *not* swallow an
    // ordinary prompt.
    for (text, _) in cases {
        assert!(
            matches!(
                effect_for(Action::Submit, text.to_string()),
                Effect::Local(_)
            ),
            "`{text}` must not be dispatched to a backend"
        );
    }
    assert_eq!(
        effect_for(Action::Submit, "what I typed".to_string()),
        Effect::Run("what I typed".to_string()),
        "an ordinary prompt is still a turn"
    );
}

// ---------------------------------------------------------------------------------------
// Truncation is stated, never silent
// ---------------------------------------------------------------------------------------

#[test]
fn the_tiers_own_truncation_note_reaches_the_transcript_word_for_word() {
    let base = Base::new();
    let cfg = base.cfg_with(Tools {
        workspace: Some(base.ws()),
        max_output_bytes: SMALL_CAP,
        ..Tools::default()
    });
    let lines = ask(&cfg, "/tools run read {\"file_path\":\"long.txt\"}");

    // First that the fixture really is over the cap, so the rest is not vacuous.
    let result = tier(&cfg, "read", serde_json::json!({"file_path": "long.txt"}));
    assert!(
        result.output.contains("capped at"),
        "the fixture must exercise the tier's cap: {}",
        result.output
    );

    assert!(
        lines
            .iter()
            .any(|line| line.contains("capped at 256 bytes by `tools.max_output_bytes`")),
        "the sentence that says the output is incomplete must survive rendering:\n{}",
        lines.join("\n")
    );

    // Verbatim: every byte the tier returned is in the transcript. A renderer that cut
    // the output, re-wrapped it, or dropped the tail would fail here — which is the shape
    // of "truncation is stated, never silent" that this row can be held to, since the cap
    // in this path is the tier's and its note is *inside* the text.
    assert_eq!(
        lines[1..].join("\n"),
        result.output.trim_end_matches('\n'),
        "the transcript must show the tier's output unchanged"
    );
}

// ---------------------------------------------------------------------------------------
// The two properties that are the point of this stage
// ---------------------------------------------------------------------------------------

#[test]
fn a_run_spawns_no_vendor_cli_and_writes_no_session_row() {
    let base = Base::new();
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("pie-home");
    fs::create_dir_all(&home).unwrap();

    // Stand-ins named after the backends, first on `PATH`: a spawn resolves a bare name
    // through `PATH` (`process::resolve_binary`), so a vendor CLI this row never reaches
    // is a mark file that never appears.
    for backend in ["agy", "codex", "claude"] {
        let _ = recorder(&base, backend);
    }
    let _env = EnvPin::new(Some(&home), Some(&base.bin()));

    // The control first, because "no mark file" is also what a broken recorder produces:
    // the same mechanism catches a spawn that really happens.
    let mut bash_on = base.cfg();
    bash_on.tools.bash = true;
    bash_on.tools.bash_allow = vec![STAND_IN.to_string()];
    let _ = recorder(&base, STAND_IN);
    let ran = ask(
        &bash_on,
        &format!("/tools run bash {{\"argv\":[\"{}\"]}}", STAND_IN),
    );
    assert!(ran[0].contains("ok"), "{ran:?}");
    assert!(
        base.mark(STAND_IN).exists(),
        "the control must spawn, or the assertions below are about a mechanism that \
         never worked: {ran:?}"
    );

    // Now the row's own surface: the list, a successful call, and every refusal.
    let cfg = base.cfg();
    let _ = ask(&cfg, "/tools list");
    let _ = ask(&cfg, "/tools run read {\"file_path\":\"notes.txt\"}");
    let _ = ask(&cfg, "/tools run write {\"file_path\":\"notes.txt\"}");
    let _ = ask(&cfg, "/tools run read");
    let _ = ask(&base.cfg_with(Tools::default()), "/tools list");

    for backend in ["agy", "codex", "claude"] {
        assert!(
            !base.mark(backend).exists(),
            "`/tools` spawned the `{backend}` CLI; the whole point of this row is that it \
             does not (no quota is spent, and on a machine with no API key it is the only \
             interactive path to M11's tier)"
        );
    }

    // And no session row: a row is written when a turn ends, and no turn ran. Read from
    // the store rather than asserted by absence of a file, so an empty log and a missing
    // log are the same evidence.
    let rows = session_log::read_all(&pie::paths::usage_log()).unwrap();
    assert!(
        rows.is_empty(),
        "`/tools` wrote {} session row(s); it runs no turn",
        rows.len()
    );
    assert!(
        pie::session::list().unwrap().is_empty(),
        "`/tools` created a session"
    );
}

#[test]
fn list_run_and_every_refusal_leave_config_toml_byte_identical() {
    // `DESIGN.md` §21.4's boundary: `/tools` reads state and runs tools, and never makes
    // the enable-a-tool decision on the user's behalf. The decision is made once, in
    // `config.toml`, by a human — so this row must not touch that file at all.
    let base = Base::new();
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("pie-home");
    fs::create_dir_all(&home).unwrap();
    let config = home.join("config.toml");
    fs::write(
        &config,
        format!(
            "[general]\ndefault_backend = \"codex\"\n\n[tools]\nworkspace = \"{}\"\n",
            base.ws().display()
        ),
    )
    .unwrap();
    let before = fs::read(&config).unwrap();
    let _env = EnvPin::new(Some(&home), None);

    let mut bash_on = base.cfg();
    bash_on.tools.bash = true;
    bash_on.tools.bash_allow = vec![STAND_IN.to_string()];
    let _ = recorder(&base, STAND_IN);

    for (cfg, text) in [
        (base.cfg(), "/tools list"),
        (base.cfg(), "/tools"),
        (base.cfg(), "/tools run read {\"file_path\":\"notes.txt\"}"),
        (base.cfg(), "/tools run write {\"file_path\":\"notes.txt\"}"),
        (base.cfg(), "/tools run"),
        (base.cfg_with(Tools::default()), "/tools list"),
        (bash_on, "/tools run bash {\"argv\":[\"echo\"]}"),
    ] {
        let lines = ask(&cfg, text);
        assert!(!lines.is_empty(), "`{text}` must answer");
    }

    assert_eq!(
        fs::read(&config).unwrap(),
        before,
        "a `/tools` session must leave config.toml byte-identical"
    );
}

// ---------------------------------------------------------------------------------------
// One row, one dispatch, two front-ends
// ---------------------------------------------------------------------------------------

#[test]
fn the_tools_row_is_answered_once_through_the_registry_on_both_front_ends() {
    let base = Base::new();
    let cfg = base.cfg();

    let row = slash::find(slash::TOOLS).expect("`/tools` must be a registry row");
    assert_eq!(row.name, "tools");

    for args in ["list", "run read {\"file_path\":\"notes.txt\"}"] {
        let settled = slash::settled(row, args);
        let typed = ask_with(&cfg, &settled, Action::Submit);
        let palette = ask_with(&cfg, &settled, Action::PaletteRun);
        assert_eq!(
            typed, palette,
            "the palette and a typed command must reach one answer for `{settled}`"
        );
        assert!(
            !typed.join("\n").contains("not wired yet"),
            "`{settled}` is answered by a stage-D stub rather than by the tool tier"
        );
    }

    // The row is the real one: `list` shows the workspace and a state, not a sentence
    // about a later stage.
    let listed = ask(&cfg, "/tools list").join("\n");
    assert!(listed.contains("workspace"), "{listed}");
    assert!(listed.contains("enabled"), "{listed}");

    // And the route did not swallow the other rows: `/help` still comes from the registry
    // table through the same dispatch.
    let help = ask(&cfg, "/help").join("\n");
    assert!(
        help.contains(&row.menu_line()),
        "`/help` must still be generated from the table:\n{help}"
    );
}

// ---------------------------------------------------------------------------------------
// What the surface does not reach for
// ---------------------------------------------------------------------------------------

#[test]
fn the_tools_surface_reaches_no_vendor_no_socket_and_no_file_of_its_own() {
    // Static, and therefore partial: it proves the module reaches for no process, no
    // socket and no session store *by name*. The behavioural half is the rest of this
    // file, where the only side effect is a spawn the tier was asked for.
    let source =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/tools.rs")).unwrap();
    assert!(
        source.contains("ToolSet"),
        "the gate is reading something other than the `/tools` implementation"
    );

    for forbidden in [
        "std::fs",
        "OpenOptions",
        "File::create",
        "std::process",
        "Command::new",
        "reqwest",
        "TcpStream",
        "crate::provider",
        "session_log",
    ] {
        assert!(
            !source.contains(forbidden),
            "`/tools` names `{forbidden}`; it reads state and runs one tool, and reaches \
             nothing else"
        );
    }
}
