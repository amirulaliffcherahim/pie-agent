//! Gate G4 for M3: one persona across agy and codex, and no per-CLI config.
//!
//! `planning/ACCEPTANCE.md` §7 lists the checks; this file is where they are
//! enforced. Every one runs offline. That is deliberate: the roadmap's Done
//! condition ("same persona on agy and codex") is a statement about the bytes pie
//! sends, so it is provable at the payload level — argv, not a live turn. G5's
//! two live turns prove the model *honoured* those bytes; that costs quota and is
//! the user's call, not a test's.
//!
//! Nothing here touches the real `~/.pie`: every test builds its own home in a
//! `tempfile::tempdir()` and passes it explicitly (or to a child process through
//! `PIE_HOME`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use pie::config::Config;
use pie::error::PieError;
use pie::profile::{Profile, TASK_SEPARATOR};
use pie::provider::{agy, claude, codex, resolve_target};

/// Serialises the tests that touch process-global state.
///
/// `std::env::set_current_dir` changes the working directory for *every* thread
/// in the process, and the determinism check below deliberately assembles a
/// profile from two different directories. Every test here takes this lock, and
/// every child process is given an explicit `current_dir`, so a cwd change can
/// never be observed by a neighbour.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    // A poisoned lock means another assertion failed; the cwd itself is fine, so
    // recover rather than turn one failure into a cascade.
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The task text used everywhere, so the folded payloads are comparable.
const TASK: &str = "summarise this repo";

/// Deliberately distinctive and free of anything timestamp- or path-shaped.
const PROFILE_TOML: &str = r#"
name = "pie"
tone = "terse"
rules = [
  "Answer with the smallest correct change.",
  "Never guess a path: read it first.",
]
output = ["Plain text. No preamble, no summary of what you just did."]
"#;

const AGENTS_MD: &str = "AGENTS-MARKER: prefer an existing helper over a new abstraction.\n";

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// A home with all three sources configured.
fn write_full_home(home: &Path) {
    write(home, "profile.toml", PROFILE_TOML);
    write(home, "AGENTS.md", AGENTS_MD);
    write(
        home,
        "memory/01-first.md",
        "MEMORY-ONE: the release gate is `scripts/qc.sh`.\n",
    );
    write(
        home,
        "memory/02-second.md",
        "MEMORY-TWO: agy has no system role.\n",
    );
}

fn load(home: &Path) -> Profile {
    Profile::load(home).expect("a valid home must load")
}

fn strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Strip the task text off a folded payload and return the persona in front.
fn persona_in(payload: &str, task: &str) -> String {
    let suffix = format!("{TASK_SEPARATOR}{task}");
    payload
        .strip_suffix(&suffix)
        .unwrap_or_else(|| panic!("{payload:?} is not `persona + separator + {task:?}`"))
        .to_string()
}

// ---------------------------------------------------------------------------
// The Done condition, at payload level
// ---------------------------------------------------------------------------

/// *"same persona on agy and codex"* — asserted on the argv both backends would
/// receive, which is the strongest check that needs no quota.
#[test]
fn the_persona_agy_receives_is_byte_identical_to_the_persona_codex_receives() {
    let _guard = serial();
    let home = tempfile::tempdir().unwrap();
    write_full_home(home.path());

    let profile = load(home.path());
    let persona = profile.persona().expect("a configured home has a persona");

    let request = {
        let mut request = pie::provider::ChatRequest::new(TASK);
        request.cwd = PathBuf::from("C:/work");
        request.persona = Some(persona.to_string());
        request
    };

    let agy_args = strings(&agy::build_args(&Config::default(), &request).unwrap());
    let codex_args = strings(&codex::build_args(&Config::default(), &request).unwrap());

    // agy: `--print <prompt>` consumes the next token as the prompt.
    assert_eq!(agy_args[0], "--print");
    assert_eq!(
        agy_args[1],
        *codex_args.last().unwrap(),
        "identical payload"
    );
    assert!(!agy_args.contains(&"--append-system-prompt".to_string()));
    assert!(!codex_args.contains(&"--append-system-prompt".to_string()));

    // The persona inside that payload is the projection, byte for byte.
    assert_eq!(persona_in(&agy_args[1], TASK), persona);
    assert_eq!(persona_in(codex_args.last().unwrap(), TASK), persona);

    // And it is the projection `pie profile` prints, not a re-rendering of it.
    assert!(profile.projection().contains("AGENTS-MARKER"));
    assert!(profile.projection().contains("MEMORY-TWO"));
}

#[test]
fn claude_receives_the_persona_through_append_system_prompt_and_agy_and_codex_do_not() {
    let _guard = serial();
    let home = tempfile::tempdir().unwrap();
    write_full_home(home.path());
    let persona = load(home.path()).persona().unwrap().to_string();

    let mut request = pie::provider::ChatRequest::new(TASK);
    request.cwd = PathBuf::from("C:/work");
    request.persona = Some(persona.clone());

    let claude_args = strings(&claude::build_args(&Config::default(), &request).unwrap());
    let at = claude_args
        .iter()
        .position(|arg| arg == "--append-system-prompt")
        .expect("claude's mechanism is a system-prompt flag (knowledge/claude.md)");
    assert_eq!(claude_args[at + 1], persona, "the projection verbatim");
    assert_eq!(claude_args[1], TASK, "claude's task text is not folded");

    for (backend, args) in [
        (
            "agy",
            strings(&agy::build_args(&Config::default(), &request).unwrap()),
        ),
        (
            "codex",
            strings(&codex::build_args(&Config::default(), &request).unwrap()),
        ),
    ] {
        assert!(
            !args.contains(&"--append-system-prompt".to_string()),
            "{backend} has no system-prompt flag (DESIGN §10)"
        );
        assert!(
            args.iter().any(|arg| arg.contains(&persona)),
            "{backend} must still receive the persona"
        );
        assert!(
            !args.iter().any(|arg| arg == TASK),
            "{backend} must not receive a bare, unfolded task"
        );
    }

    // Capabilities route the projection, so they must be exact. M10 appended two backends
    // that **do** have a real system role — the OpenAI-compatible shape has a `system`
    // message, so the projection goes in whole and unmodified, exactly as claude's does.
    // That is why this is a labelled list rather than a "is it claude" check: the property
    // is "the persona arrives verbatim by a mechanism the backend actually has", and a
    // length assertion would stop being evidence the moment a backend was added.
    let mechanisms: Vec<(&str, bool)> = pie::provider::registry()
        .iter()
        .map(|provider| (provider.id(), provider.capabilities().system_prompt))
        .collect();
    assert_eq!(
        mechanisms,
        vec![
            ("agy", false),
            ("codex", false),
            ("claude", true),
            ("openrouter", true),
            ("deepseek", true),
        ],
        "capabilities are what route the projection, so they must be exact"
    );

    // The CLI backends specifically: `agy` and `codex` have no system role at all, which is
    // why they fold, and that is the pair `DESIGN.md` §10 describes. Asserted separately so
    // adding a backend never weakens the claim about *these* two.
    for provider in pie::provider::registry() {
        if matches!(provider.id(), "agy" | "codex") {
            assert!(
                !provider.capabilities().system_prompt,
                "`{}` folds because it has no system role (DESIGN section 10)",
                provider.id()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// The same inputs must produce the same bytes: from another working directory,
/// at another time, and from another root. Anything that fails here invalidates
/// whatever stable prefix the vendor caches, on every invocation.
#[test]
fn the_projection_is_byte_identical_from_another_directory_time_and_root() {
    let _guard = serial();
    let first_home = tempfile::tempdir().unwrap();
    let second_home = tempfile::tempdir().unwrap();
    write_full_home(first_home.path());
    write_full_home(second_home.path());

    let original = std::env::current_dir().unwrap();

    let before = load(first_home.path()).projection().to_string();
    std::thread::sleep(Duration::from_millis(30));
    let again = load(first_home.path()).projection().to_string();
    assert_eq!(before, again, "time must not enter the projection");

    let elsewhere = tempfile::tempdir().unwrap();
    std::env::set_current_dir(elsewhere.path()).unwrap();
    let from_other_cwd = load(first_home.path()).projection().to_string();
    std::env::set_current_dir(&original).unwrap();
    assert_eq!(
        before, from_other_cwd,
        "the cwd must not enter the projection"
    );

    // A different root with identical content: the *root* must not leak either.
    let from_other_root = load(second_home.path()).projection().to_string();
    assert_eq!(before, from_other_root, "the home path must not enter it");

    assert_no_volatile(
        &before,
        &[first_home.path(), second_home.path(), elsewhere.path()],
    );
}

/// No timestamp, session id, cwd, model id or quota figure may appear anywhere in
/// the projection (`DESIGN.md` §10, `TOKEN-ECONOMICS.md` §4).
fn assert_no_volatile(projection: &str, roots: &[&Path]) {
    for root in roots {
        let rendered = root.display().to_string();
        assert!(
            !projection.contains(&rendered),
            "a filesystem path leaked into the projection: {rendered}"
        );
    }

    for marker in [
        "gemini-3.8-flash",
        "gpt-6-luna",
        env!("CARGO_PKG_VERSION"),
        "quota",
    ] {
        assert!(
            !projection.contains(marker),
            "`{marker}` is volatile and must never reach the projection"
        );
    }

    // `yyyy-mm-dd`: the shape a timestamp takes. Cheap and direct, not a
    // general-purpose date parser.
    let bytes = projection.as_bytes();
    let dated = (0..bytes.len().saturating_sub(9)).any(|i| {
        bytes[i..i + 4].iter().all(u8::is_ascii_digit)
            && bytes[i + 4] == b'-'
            && bytes[i + 5..i + 7].iter().all(u8::is_ascii_digit)
            && bytes[i + 7] == b'-'
            && bytes[i + 8..i + 10].iter().all(u8::is_ascii_digit)
    });
    assert!(!dated, "the projection contains a date: {projection}");
}

#[test]
fn memory_files_concatenate_in_filename_order_not_creation_order() {
    let _guard = serial();
    let home = tempfile::tempdir().unwrap();
    // Written in the reverse of the sorted order, so a filesystem that returns
    // entries in creation order would fail this test.
    write(home.path(), "memory/zeta.md", "PROBE-ZETA");
    write(home.path(), "memory/alpha.md", "PROBE-ALPHA");
    write(home.path(), "memory/middle.md", "PROBE-MIDDLE");

    let profile = load(home.path());
    let projection = profile.projection();
    let alpha = projection.find("PROBE-ALPHA").unwrap();
    let middle = projection.find("PROBE-MIDDLE").unwrap();
    let zeta = projection.find("PROBE-ZETA").unwrap();
    assert!(alpha < middle && middle < zeta, "{projection}");

    let sources: Vec<String> = profile
        .contributions()
        .iter()
        .map(|c| c.source.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(sources, vec!["alpha.md", "middle.md", "zeta.md"]);

    // And the order is what `pie profile` reports, so provenance matches the
    // projection rather than being a second, independent guess.
    let stdout = run_pie(home.path(), &["profile"], &[]);
    let mut cursor = 0;
    for name in ["alpha.md", "middle.md", "zeta.md"] {
        let at = stdout
            .find(name)
            .unwrap_or_else(|| panic!("{name} missing from the provenance table of:\n{stdout}"));
        assert!(at > cursor, "{name} is out of order in:\n{stdout}");
        cursor = at;
    }
}

// ---------------------------------------------------------------------------
// Optionality
// ---------------------------------------------------------------------------

/// A fresh install has no configuration at all, and must still work — with no
/// invented persona reaching any backend.
#[test]
fn a_fresh_home_projects_nothing_and_no_backend_gets_an_invented_persona() {
    let _guard = serial();
    let home = tempfile::tempdir().unwrap();

    let profile = load(home.path());
    assert!(profile.is_empty());
    assert_eq!(profile.projection(), "");
    assert_eq!(profile.bytes(), 0);
    assert_eq!(profile.persona(), None, "None, never Some(\"\")");
    assert!(profile.contributions().is_empty());

    let request = pie::provider::ChatRequest::new(TASK);
    assert_eq!(
        strings(&agy::build_args(&Config::default(), &request).unwrap())[1],
        TASK
    );
    assert_eq!(
        strings(&codex::build_args(&Config::default(), &request).unwrap())
            .last()
            .unwrap(),
        TASK
    );
    let claude_args = strings(&claude::build_args(&Config::default(), &request).unwrap());
    assert!(!claude_args.contains(&"--append-system-prompt".to_string()));

    // Each source is optional on its own, too: one file at a time, with the
    // other two absent, must not error.
    for (only, content) in [
        ("profile.toml", "name = \"ONLY-MARKER\"\n"),
        ("AGENTS.md", "ONLY-MARKER\n"),
        ("memory/only.md", "ONLY-MARKER\n"),
    ] {
        let single = tempfile::tempdir().unwrap();
        write(single.path(), only, content);
        let profile = load(single.path());
        assert_eq!(
            profile.contributions().len(),
            1,
            "{only} must be accepted with the other two absent"
        );
        assert!(profile.projection().contains("ONLY-MARKER"));
    }

    // `memory/` without any `*.md` in it is empty, not an error.
    let empty_memory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(empty_memory.path().join("memory")).unwrap();
    assert!(load(empty_memory.path()).is_empty());
}

// ---------------------------------------------------------------------------
// The agy argv cap
// ---------------------------------------------------------------------------

/// A projection that does not fit is an error naming the projection. It is never
/// truncated: half a persona silently answers a different question than the one
/// that was configured.
#[test]
fn a_projection_that_does_not_fit_is_a_prompt_too_long_naming_the_projection() {
    let _guard = serial();
    let mut request = pie::provider::ChatRequest::new("hi");
    request.cwd = PathBuf::from("C:/work");
    request.persona = Some("p".repeat(30_000));

    // Built in full first: nothing dropped the persona on the way in.
    let args = agy::build_args(&Config::default(), &request).unwrap();
    let payload = args[1].to_string_lossy();
    assert_eq!(payload.len(), 30_000 + TASK_SEPARATOR.len() + 2);
    assert_eq!(payload.matches('p').count(), 30_000);
    assert!(payload.starts_with('p'), "the persona survives intact");

    let error = agy::build_plan(&Config::default(), &request).unwrap_err();
    match &error {
        PieError::PromptTooLong { backend, hint, .. } => {
            assert_eq!(backend, "agy");
            assert!(
                hint.contains("projection"),
                "the hint must name the projection: {hint}"
            );
        }
        other => panic!("expected PromptTooLong, got {other:?}"),
    }
    assert_eq!(error.exit_code(), 2);
    assert!(error.to_string().contains("prompt too long"));

    // With no persona the same argv is fine and the hint is the prompt's own.
    let plain = pie::provider::ChatRequest::new("hi");
    assert!(agy::build_plan(&Config::default(), &plain).is_ok());
}

// ---------------------------------------------------------------------------
// No vendor config
// ---------------------------------------------------------------------------

/// Vendor configuration files pie must never read or write (`PLAN.md` §8,
/// `DESIGN.md` §10). Teaching one CLI a persona outside pie is the other half of
/// "no per-CLI config", and it cannot be caught by any behaviour test — there is
/// nothing to observe if the file is simply opened.
const VENDOR_CONFIG: [&str; 3] = [
    ".codex/config.toml",
    ".claude/settings.json",
    ".gemini/config",
];

/// The one pre-existing exception, with its reason.
///
/// `src/auth.rs` probes `~/.gemini/config/config.json` for **presence** to
/// classify agy's credential (`ACCEPTANCE.md` §7, invariant I9; the alternative
/// is reporting "logged out" for a logged-in CLI). That is a credential store,
/// not a configuration surface, and pie reads no value from it. Reporting it
/// here rather than widening the rule silently: this literal appearing anywhere
/// else is the failure the rule exists to catch.
const CREDENTIAL_STORE_HEURISTIC: &[(&str, &str)] = &[("src/auth.rs", ".gemini/config")];

#[test]
fn nothing_under_src_reads_or_writes_vendor_config() {
    let mut sources = Vec::new();
    rust_sources(&repo_root().join("src"), &mut sources);
    assert!(sources.len() > 10, "the scan found no sources: {sources:?}");

    let mut offenders: Vec<String> = Vec::new();
    for path in &sources {
        let relative = relative(path);
        let text = std::fs::read_to_string(path).unwrap();

        for (index, line) in text.lines().enumerate() {
            // A comment that documents the rule is not a reader.
            if line.trim_start().starts_with("//") {
                continue;
            }
            for forbidden in VENDOR_CONFIG {
                if !line.contains(forbidden) {
                    continue;
                }
                if CREDENTIAL_STORE_HEURISTIC
                    .iter()
                    .any(|(file, literal)| *file == relative && *literal == forbidden)
                {
                    continue;
                }
                offenders.push(format!("{relative}:{} {forbidden}", index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "pie must never read a vendor's own config (PLAN §8): {offenders:?}"
    );

    // The projection's own module must not be able to reach one at all — not
    // even in a comment, so a future edit has to think about it.
    for file in ["src/profile.rs", "src/paths.rs"] {
        let text = std::fs::read_to_string(repo_root().join(file)).unwrap();
        for forbidden in VENDOR_CONFIG {
            assert!(
                !text.contains(forbidden),
                "{file} names a vendor config file ({forbidden})"
            );
        }
    }
}

fn relative(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            into.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// `pie profile`
// ---------------------------------------------------------------------------

/// A stand-in vendor CLI that records every invocation.
///
/// The counter is the point: "spent no quota" is otherwise unfalsifiable, since
/// a command that silently spawns a CLI produces output either way.
fn counting_stand_in(dir: &Path) -> PathBuf {
    let counter = dir.join("calls.txt");

    #[cfg(windows)]
    let script = dir.join("counting-cli.bat");
    #[cfg(not(windows))]
    let script = dir.join("counting-cli.sh");

    #[cfg(windows)]
    let body = format!("@echo off\r\necho call>> \"{}\"\r\n", counter.display());
    #[cfg(not(windows))]
    let body = format!("#!/bin/sh\necho call >> '{}'\n", counter.display());

    std::fs::write(&script, body).unwrap();

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    script
}

/// Every file under `root`, two levels deep, relative and sorted — for proving a
/// command wrote nothing at all.
fn listing(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        found.push(relative_to(root, &path));
        if path.is_dir() {
            for child in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                found.push(relative_to(root, &child.path()));
            }
        }
    }
    found.sort();
    found
}

fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn run_pie(home: &Path, args: &[&str], binaries: &[(&str, &Path)]) -> String {
    run_pie_both(home, args, binaries).0
}

/// `(stdout, stderr)` of a successful run.
fn run_pie_both(home: &Path, args: &[&str], binaries: &[(&str, &Path)]) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pie"));
    command
        .args(args)
        .env("PIE_HOME", home)
        // Immune to a concurrent cwd change in another test in this binary.
        .current_dir(repo_root());

    for (key, path) in binaries {
        command.env(*key, *path);
    }

    let output = command.output().expect("failed to run the pie binary");
    assert!(
        output.status.success(),
        "`pie {}` failed ({}): {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn pie_profile_spends_no_quota_and_writes_no_session_row() {
    let _guard = serial();
    let home = tempfile::tempdir().unwrap();
    write_full_home(home.path());

    let stand_in_dir = tempfile::tempdir().unwrap();
    let stand_in = counting_stand_in(stand_in_dir.path());

    let before = listing(home.path());
    let stdout = run_pie(
        home.path(),
        &["profile"],
        &[
            ("PIE_AGY_BIN", &stand_in),
            ("PIE_CODEX_BIN", &stand_in),
            ("PIE_CLAUDE_BIN", &stand_in),
        ],
    );

    assert!(
        !stand_in_dir.path().join("calls.txt").exists(),
        "`pie profile` must not spawn a vendor CLI (invariant I7)"
    );
    assert!(
        !home.path().join("usage.jsonl").exists(),
        "`pie profile` must not write a session row"
    );
    assert_eq!(
        listing(home.path()),
        before,
        "`pie profile` must leave PIE_HOME untouched"
    );

    // The projection itself is what it prints, verbatim.
    let projection = load(home.path()).projection().to_string();
    assert!(
        stdout.contains(&projection),
        "the projection must be printed verbatim:\n{stdout}"
    );

    // Its provenance is printed too, per contribution.
    for name in ["profile.toml", "AGENTS.md", "01-first.md", "02-second.md"] {
        assert!(stdout.contains(name), "{name} missing from:\n{stdout}");
    }

    // And the mechanism, so the M3 contract is inspectable without reading code.
    assert!(stdout.contains("folded before the prompt text"));
    assert!(stdout.contains("--append-system-prompt"));
}

#[test]
fn pie_profile_works_on_a_home_with_no_configuration_at_all() {
    let _guard = serial();
    let home = tempfile::tempdir().unwrap();
    let (stdout, stderr) = run_pie_both(home.path(), &["profile"], &[]);
    assert!(stdout.contains("empty"), "{stdout}");
    assert!(stdout.contains("projection"), "{stdout}");
    assert!(
        stderr.is_empty(),
        "an empty projection is not worth a warning: {stderr}"
    );
}

/// A projection larger than agy's whole argv budget makes every agy turn
/// impossible. pie cannot fix that by truncating, so it has to say so — at the
/// moment the user is looking at the projection (DESIGN §10).
#[test]
fn pie_profile_warns_when_the_projection_cannot_fit_agy_at_all() {
    let _guard = serial();
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "memory/huge.md", &"m".repeat(25_000));

    let (stdout, stderr) = run_pie_both(home.path(), &["profile"], &[]);
    assert!(stdout.contains("25000 bytes"), "{stdout}");
    assert!(
        stderr.contains("WARNING") && stderr.contains("never truncates a persona"),
        "expected a warning on stderr, got: {stderr:?}"
    );

    // And the same projection really is refused by agy rather than cut down.
    let request = {
        let mut request = pie::provider::ChatRequest::new(TASK);
        request.persona = load(home.path()).persona().map(str::to_string);
        request
    };
    let error = agy::build_plan(&Config::default(), &request).unwrap_err();
    assert!(matches!(error, PieError::PromptTooLong { .. }));
}

/// `pie profile` reads files and prints them. Nothing in it may ever spawn, and
/// that is a property of the file rather than of the run, so it is checked
/// statically too — the counting stand-in above only proves it for the paths
/// this test happens to exercise.
///
/// Comment lines are skipped: naming the process layer in a doc comment is not a
/// call into it.
#[test]
fn the_profile_command_has_no_way_to_spawn() {
    let text = std::fs::read_to_string(repo_root().join("src/commands/profile.rs")).unwrap();

    let mut offenders = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for forbidden in [
            "crate::process",
            "process::",
            ".stream(",
            "session_log",
            "run_streaming",
            "Command::new",
        ] {
            if line.contains(forbidden) {
                offenders.push(format!("{}:{} {forbidden}", index + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "`pie profile` must be unable to spend quota, but it names: {offenders:?}"
    );
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

#[test]
fn routing_uses_the_default_backend_and_an_explicit_model_always_wins() {
    let _guard = serial();

    // The built-in default (DESIGN §10: codex, because it answers a turn in
    // ~3.6 s against agy's ~40 s), with no model invented for it.
    let cfg = Config::default();
    assert_eq!(
        cfg.general.default_backend.as_deref(),
        Some("codex"),
        "codex is the default backend"
    );
    assert_eq!(resolve_target(&cfg, None).unwrap(), ("codex".into(), None));

    // A configured default applies...
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    write(
        home.path(),
        "config.toml",
        "[general]\ndefault_backend = \"agy\"\n",
    );
    let agy_first = Config::load_from(&config).unwrap();
    assert_eq!(
        resolve_target(&agy_first, None).unwrap(),
        ("agy".into(), None)
    );

    // ...and an explicit `--model` beats it, in the strongest form: the
    // configured default is not even validated when it is not used.
    assert_eq!(
        resolve_target(&agy_first, Some("codex/gpt-6-luna")).unwrap(),
        ("codex".into(), Some("gpt-6-luna".into()))
    );

    // An unrecognised backend is a config error (exit 2), never a fallback.
    write(
        home.path(),
        "config.toml",
        "[general]\ndefault_backend = \"gemini\"\n",
    );
    let broken = Config::load_from(&config).unwrap();
    let error = resolve_target(&broken, None).unwrap_err();
    assert!(
        matches!(error, PieError::UnknownBackend { .. }),
        "got {error:?}"
    );
    assert_eq!(error.exit_code(), 2);
    assert!(error.to_string().contains("agy"), "the known list is shown");

    // Turning the default off restores the "no model selected" error, and only
    // then: that is the single state with neither a default nor a flag.
    write(
        home.path(),
        "config.toml",
        "[general]\ndefault_backend = \"\"\n",
    );
    let off = Config::load_from(&config).unwrap();
    let error = resolve_target(&off, None).unwrap_err();
    assert!(matches!(error, PieError::Config(_)));
    assert_eq!(error.exit_code(), 2);
    assert!(error.to_string().contains("no model selected"), "{error}");
}
