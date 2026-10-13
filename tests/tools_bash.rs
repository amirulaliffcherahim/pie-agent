//! Gate for **stage B of M11** (`ACCEPTANCE.md` §19): `bash`, off by default and scoped.
//!
//! `cargo test --test tools_bash` is the stage's command, and every assertion here is on
//! either what a *model* reads or what a **stand-in executable actually did**. The second
//! half is the point: "the call returned an error" is what a decorative gate returns,
//! because a `bash` tool that never works at all returns an error too. So the evidence is
//! a **side effect** — a counter file that only a real spawn can create — and every
//! refusal is paired with the control where the same command *does* run. Without the pair,
//! "the counter file does not exist" passes for a tool that is simply broken.
//!
//! The stand-ins are `.bat` on Windows and extension-less `#!/bin/sh` scripts elsewhere,
//! because the child is exec'd directly: there is no shell for pie to hand the line to (see
//! `src/tools/bash.rs`), so the stand-in must be a program in its own right.
//!
//! Runs no vendor CLI, opens no socket, and needs no credential.
//!
//! **One thing on this platform cannot be observed, and it is stated rather than implied.**
//! A `.bat` *is* a `cmd.exe` script, so `cmd` parses the arguments at that boundary no
//! matter how pie spawned it — which means a Windows `.bat` stand-in cannot tell "pie ran a
//! shell" from "the executable you allow-listed is itself a script". `a_shell_line_is_not_
//! interpreted` is therefore unix-only (CI's ubuntu job is where it runs, and `G6` only
//! type-checks it), and the Windows evidence for the same property is the source-text
//! assertion in `the_tier_reads_argv_zero_directly_and_names_no_shell`. This is `S20`'s
//! shape — a source-text assertion where the platform cannot see the behaviour — and it is
//! recorded as a residual rather than dressed up as a behavioural test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pie::config::Tools;
use pie::tool_loop::{ToolCall, ToolResult, ToolRunner};
use pie::tools::ToolSet;

// ---------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------

/// One workspace, one directory of stand-in executables, one directory of marks.
///
/// ```
/// <base>/ws/hello.txt          "workspace-file" — the child reads it *relatively*, which is
///                              how the working directory is proven
/// <base>/bin/<stand-in>        the executables the calls run
/// <base>/marks/<stand-in>-*.txt  what they recorded: the side effect a refusal must not have
/// ```
///
/// The marks live outside the workspace on purpose: they record what *the model's command*
/// did, and a command that runs is allowed to write outside the workspace — the workspace is
/// the child's starting directory, not a sandbox (`src/tools/bash.rs`, "What this does not
/// claim"). Nothing here asserts otherwise.
struct Base {
    dir: tempfile::TempDir,
}

impl Base {
    fn new() -> Self {
        let base = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(base.ws()).unwrap();
        fs::create_dir_all(base.marks()).unwrap();
        fs::create_dir_all(base.bin()).unwrap();
        fs::write(base.ws().join("hello.txt"), "workspace-file\n").unwrap();
        base
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn ws(&self) -> PathBuf {
        self.path().join("ws")
    }

    fn marks(&self) -> PathBuf {
        self.path().join("marks")
    }

    fn bin(&self) -> PathBuf {
        self.path().join("bin")
    }

    fn mark(&self, name: &str) -> PathBuf {
        self.marks().join(name)
    }

    /// The configuration a working `bash` tool needs, and nothing more.
    fn tools(&self, allow: &[&str]) -> Tools {
        Tools {
            workspace: Some(self.ws()),
            bash: true,
            bash_allow: allow.iter().map(|name| (*name).to_string()).collect(),
            ..Tools::default()
        }
    }
}

/// One call through the real router: `ToolSet` is what routes `"bash"`, and a test that
/// called `bash::run` directly would not notice a router that stopped doing so.
fn bash(tools: &Tools, input: Value) -> ToolResult {
    ToolSet::new(tools.clone()).run(&ToolCall {
        id: "toolu_bash".into(),
        name: "bash".into(),
        input,
    })
}

/// The same call, with the wall clock around it: the deadline and the output bound are both
/// claims about *when* the call returns, and neither can be checked without one.
fn timed(tools: &Tools, input: Value) -> (ToolResult, Duration) {
    let started = Instant::now();
    let result = bash(tools, input);
    (result, started.elapsed())
}

/// A stand-in's file content, with CRLF folded so an assertion reads the same on both
/// platforms.
fn content(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .replace("\r\n", "\n")
}

/// The non-empty lines of a stand-in's record.
fn lines(path: &Path) -> Vec<String> {
    content(path)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------------------
// Stand-ins
// ---------------------------------------------------------------------------------------

/// Write one stand-in executable: `.bat` on Windows, `#!/bin/sh` elsewhere, `+x` elsewhere.
///
/// The platform split is two `#[cfg]`-gated `let` bindings inside **one** body rather than
/// two `#[cfg]`-gated functions, and that is not style. Stage C shipped an outer
/// `#[cfg(windows)]` on a function that also had `#[cfg(not(windows))]` arms: Windows
/// compiled it, unix had no function at all while both callers referenced it, every local
/// gate was green, and `G6` went red the first time it ran. One body with a gated binding
/// cannot have that shape.
fn script(base: &Base, name: &str, windows_body: &str, unix_body: &str) -> PathBuf {
    // Referenced on both platforms so that neither parameter is unused on the platform that
    // does not take its branch: with `-D warnings` that is a build failure, not a note.
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

/// The stand-in that records the fact it ran, the arguments it was handed, and their count —
/// then prints one line.
///
/// Every mark is named after the stand-in, so two of them in one fixture are two independent
/// pieces of evidence rather than one file two tests could both be reading.
fn counting(base: &Base, name: &str) -> PathBuf {
    let calls = base.mark(&format!("{name}-calls.txt"));
    let args = base.mark(&format!("{name}-args.txt"));
    let argc = base.mark(&format!("{name}-argc.txt"));

    script(
        base,
        name,
        &format!(
            "echo ran>>\"{calls}\"\r\n\
             set n=0\r\n\
             :count\r\n\
             if \"%~1\"==\"\" goto counted\r\n\
             set /a n+=1\r\n\
             >>\"{args}\" echo %~1\r\n\
             shift\r\n\
             goto count\r\n\
             :counted\r\n\
             >>\"{argc}\" echo %n%\r\n\
             echo hello-from-the-counter\r\n",
            calls = calls.display(),
            args = args.display(),
            argc = argc.display()
        ),
        &format!(
            "echo ran >> \"{calls}\"\n\
             printf '%s\\n' \"$#\" > \"{argc}\"\n\
             for one in \"$@\"; do printf '%s\\n' \"$one\" >> \"{args}\"; done\n\
             echo hello-from-the-counter\n",
            calls = calls.display(),
            args = args.display(),
            argc = argc.display()
        ),
    )
}

/// A stand-in that records that it started, sleeps, and records that it finished — so the
/// deadline has two things to prove: the call returned at the deadline, **and** the child it
/// killed never reached its final line.
fn sleeper(base: &Base, name: &str) -> PathBuf {
    let started = base.mark(&format!("{name}-started.txt"));
    let finished = base.mark(&format!("{name}-finished.txt"));

    script(
        base,
        name,
        &format!(
            "echo started>\"{started}\"\r\n\
             ping -n 4 127.0.0.1 >nul\r\n\
             echo finished>\"{finished}\"\r\n\
             echo the-child-outlived-its-deadline\r\n",
            started = started.display(),
            finished = finished.display()
        ),
        &format!(
            "echo started > \"{started}\"\n\
             sleep 3\n\
             echo finished > \"{finished}\"\n\
             echo the-child-outlived-its-deadline\n",
            started = started.display(),
            finished = finished.display()
        ),
    )
}

/// A stand-in that never stops printing.
fn runaway(base: &Base, name: &str) -> PathBuf {
    script(
        base,
        name,
        ":loop\r\necho 0123456789012345678901234567890123456789\r\ngoto loop\r\n",
        "while true; do echo 0123456789012345678901234567890123456789; done\n",
    )
}

/// A stand-in that fails the way a real command fails: something useful on stderr, then a
/// non-zero exit. It writes stderr first and sleeps before its stdout, so the detached
/// stderr drain has had a second to read the pipe before stdout reaches EOF — otherwise the
/// assertion on the tail would be asserting a race.
fn failer(base: &Base, name: &str) -> PathBuf {
    script(
        base,
        name,
        "echo boom-err 1>&2\r\nping -n 2 127.0.0.1 >nul\r\necho boom\r\nexit /b 3\r\n",
        "echo boom-err >&2\nsleep 1\necho boom\nexit 3\n",
    )
}

/// A stand-in that dumps its whole environment, so a scrubbed key is provably absent (I2).
fn dumper(base: &Base, name: &str) -> PathBuf {
    let env = base.mark(&format!("{name}-env.txt"));
    script(
        base,
        name,
        &format!("set>>\"{}\"\r\n", env.display()),
        &format!("env >> \"{}\"\n", env.display()),
    )
}

/// A stand-in that reads a file by a **relative** path, which is how the working directory is
/// proven: with the wrong `cwd` the read fails and the call is an error.
fn reader(base: &Base, name: &str) -> PathBuf {
    script(base, name, "type hello.txt\r\n", "cat hello.txt\n")
}

/// A stand-in that says nothing at all.
fn quiet(base: &Base, name: &str) -> PathBuf {
    script(base, name, "rem nothing\r\n", ":\n")
}

// ---------------------------------------------------------------------------------------
// Off by default, and every refusal paired with the control that stops it being vacuous
// ---------------------------------------------------------------------------------------

#[test]
fn with_an_empty_config_a_bash_call_is_answered_and_nothing_runs() {
    let base = Base::new();
    // The stand-in really does write its mark — the control below proves it — so its absence
    // here is evidence about pie and not about the script.
    let stand_in = counting(&base, "counter");

    let result = bash(
        &Tools::default(),
        json!({"argv": [stand_in.display().to_string()]}),
    );

    assert!(
        !base.mark("counter-calls.txt").exists(),
        "the command ran with no configuration at all: `{}`",
        base.mark("counter-calls.txt").display()
    );
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("tools.bash"),
        "a disabled tool is answered, and the answer names the config key: {}",
        result.output
    );
    assert_eq!(result.id, "toolu_bash", "the result carries the call's id");
}

/// **The opt-in is not overridable by the allow-list**, which the test above cannot say on
/// its own: with an empty config *either* the `tools.bash` gate or the `tools.workspace`
/// gate explains the refusal, so it pins the outcome but not which check produced it. This
/// is the one configuration where only `tools.bash` can be the reason nothing ran - the
/// workspace exists and the command's own name is allow-listed, so a tier that consulted the
/// allow-list first, or that treated an allow-list entry as the enabling decision, would
/// spawn it.
///
/// Two assertions, and each is doing a different job. The counter file makes "nothing ran" a
/// fact about the process rather than a claim about the response text. And the phrase
/// `tools.bash = true` appears in **only** one of the three refusal sentences, whereas the
/// looser `tools.bash` would also match `tools.bash_allow` and so would not identify the gate.
#[test]
fn an_allow_listed_program_still_does_not_run_while_bash_is_off() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");
    let tools = Tools {
        bash: false,
        ..base.tools(&["counter"])
    };

    let result = bash(&tools, json!({"argv": [stand_in.display().to_string()]}));

    assert!(
        !base.mark("counter-calls.txt").exists(),
        "`tools.bash` is false, so nothing may run even though the program is allow-listed \
         and a workspace is configured: `{}`",
        base.mark("counter-calls.txt").display()
    );
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("tools.bash = true"),
        "the refusal must be the one that names `tools.bash` as the decision, not the \
         workspace or the allow-list: {}",
        result.output
    );
    assert_eq!(result.id, "toolu_bash", "the result carries the call's id");
}

#[test]
fn bash_enabled_with_an_empty_allow_list_runs_nothing_and_names_the_key() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");
    let tools = Tools {
        bash_allow: Vec::new(),
        ..base.tools(&[])
    };

    let result = bash(&tools, json!({"argv": [stand_in.display().to_string()]}));

    assert!(
        !base.mark("counter-calls.txt").exists(),
        "an empty `bash_allow` is not \"everything is allowed\""
    );
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("tools.bash_allow"),
        "the refusal names the key and the fix: {}",
        result.output
    );
}

#[test]
fn without_a_workspace_a_bash_call_runs_nothing_and_names_the_key() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");
    // Everything else is enabled, so the workspace is the only thing the refusal can be
    // about — which is what makes this a test of the single enabling decision.
    let tools = Tools {
        workspace: None,
        ..base.tools(&["counter"])
    };

    let result = bash(&tools, json!({"argv": [stand_in.display().to_string()]}));

    assert!(
        !base.mark("counter-calls.txt").exists(),
        "no workspace, no command"
    );
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("tools.workspace"),
        "{}",
        result.output
    );
}

/// The control that stops the three refusals above from being vacuous.
///
/// Every one of them asserts that a counter file does **not** exist, and a `bash` tool that
/// never spawns anything satisfies all three. This is the same call with the configuration
/// completed, and it asserts the file **does** exist — so the refusals are evidence about
/// the gate rather than about a tool that cannot work.
#[test]
fn the_paired_control_an_enabled_allow_listed_command_actually_runs() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");
    let tools = base.tools(&["counter"]);

    let result = bash(&tools, json!({"argv": [stand_in.display().to_string()]}));

    assert!(
        base.mark("counter-calls.txt").exists(),
        "the stand-in never ran, so the refusal tests prove nothing: {}",
        result.output
    );
    assert_eq!(lines(&base.mark("counter-calls.txt")), vec!["ran"]);
    assert!(!result.is_error, "{}", result.output);
    assert!(
        result.output.contains("hello-from-the-counter"),
        "the command's own stdout is the answer: {}",
        result.output
    );
}

#[test]
fn a_program_outside_the_allow_list_is_refused_before_the_spawn() {
    let base = Base::new();
    counting(&base, "counter");
    let unlisted = counting(&base, "other");
    let tools = base.tools(&["counter"]);

    let result = bash(&tools, json!({"argv": [unlisted.display().to_string()]}));

    assert!(
        !base.mark("other-calls.txt").exists(),
        "an unlisted program was spawned: the allow-list is checked *after* the spawn"
    );
    assert!(
        !base.mark("counter-calls.txt").exists(),
        "a refused call ran something else"
    );
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("tools.bash_allow"),
        "the refusal names the key: {}",
        result.output
    );
    assert!(
        result.output.contains("counter"),
        "and it lists the names that are allowed: {}",
        result.output
    );
    assert!(
        result.output.contains("Do not retry"),
        "a refusal should not leave the model to burn a turn: {}",
        result.output
    );
}

/// The paired control for the refusal above: the same shape of call, with the name listed.
#[test]
fn the_same_program_runs_once_its_name_is_listed() {
    let base = Base::new();
    let listed = counting(&base, "counter");
    counting(&base, "other");
    let tools = base.tools(&["counter"]);

    let result = bash(&tools, json!({"argv": [listed.display().to_string()]}));

    assert!(
        base.mark("counter-calls.txt").exists(),
        "the listed program did not run, so the refusal above proves nothing: {}",
        result.output
    );
    assert!(
        !base.mark("other-calls.txt").exists(),
        "listing `counter` ran `other` as well"
    );
    assert!(!result.is_error, "{}", result.output);
}

#[test]
fn the_allow_list_matches_a_file_name_and_tolerates_an_executable_suffix() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");

    // The call names the executable by **path** while the allow-list names it by **file
    // name** — `…/bin/counter.bat` against `counter`. Both `file_name` and the Windows
    // suffix rule have to be right for this to run, and on unix the same test proves the
    // path is stripped to the last component.
    let result = bash(
        &base.tools(&["counter"]),
        json!({"argv": [stand_in.display().to_string()]}),
    );

    assert!(
        base.mark("counter-calls.txt").exists(),
        "a path was not reduced to its file name, or `.bat`/`.exe` was treated as part of it: \
         {}",
        result.output
    );
}

// ---------------------------------------------------------------------------------------
// The argument shape
// ---------------------------------------------------------------------------------------

#[test]
fn a_quoted_token_reaches_the_child_as_one_argument() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");
    let line = format!("{} --msg \"two words\"", stand_in.display());

    let result = bash(&base.tools(&["counter"]), json!({ "command": line }));

    assert!(!result.is_error, "{}", result.output);
    assert_eq!(
        lines(&base.mark("counter-argc.txt")),
        vec!["2"],
        "the line split into the wrong number of arguments"
    );
    assert_eq!(
        lines(&base.mark("counter-args.txt")),
        vec!["--msg", "two words"],
        "a quoted region must stay one argument, with the quotes removed"
    );
}

#[test]
fn the_argv_shape_needs_no_quoting_at_all() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");

    let result = bash(
        &base.tools(&["counter"]),
        json!({"argv": [stand_in.display().to_string(), "two words", "three"]}),
    );

    assert!(!result.is_error, "{}", result.output);
    // Two, not three: `argv[0]` is the program, so it is consumed as the executable rather
    // than handed to itself as an argument.
    assert_eq!(lines(&base.mark("counter-argc.txt")), vec!["2"]);
    assert_eq!(
        lines(&base.mark("counter-args.txt")),
        vec!["two words", "three"]
    );
}

#[test]
fn a_missing_argument_is_refused_by_name() {
    let base = Base::new();
    let tools = base.tools(&["counter"]);

    for input in [json!({}), json!({"other": "cargo test"})] {
        let result = bash(&tools, input.clone());
        assert!(result.is_error, "{input}: {}", result.output);
        assert!(
            result.output.contains("command"),
            "the refusal names the argument to send: {input} -> {}",
            result.output
        );
    }
    assert!(!base.mark("counter-calls.txt").exists());
}

#[test]
fn a_mistyped_or_empty_argument_is_refused_by_name() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");
    let path = stand_in.display().to_string();
    let tools = base.tools(&["counter"]);

    let cases: [(Value, &str); 6] = [
        (json!({"command": 3}), "command"),
        (json!({"command": ""}), "command"),
        (json!({"command": "   "}), "command"),
        (json!({"argv": []}), "argv"),
        (json!({"argv": [path.clone(), 3]}), "argv"),
        (json!({"argv": [""]}), "argv"),
    ];

    for (input, named) in cases {
        let result = bash(&tools, input.clone());
        assert!(result.is_error, "{input}: {}", result.output);
        assert!(
            result.output.contains(named),
            "{input} should be refused by naming `{named}`: {}",
            result.output
        );
    }
    // A batch or a shell would have run `cargo`, `3`, or nothing at all; none of them ran.
    assert!(!base.mark("counter-calls.txt").exists());
}

#[test]
fn an_unterminated_quote_is_refused_rather_than_guessed() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");
    let line = format!("{} \"unterminated", stand_in.display());

    let result = bash(&base.tools(&["counter"]), json!({ "command": line }));

    assert!(result.is_error, "{}", result.output);
    assert!(result.output.contains("unterminated"), "{}", result.output);
    assert!(!base.mark("counter-calls.txt").exists());
}

#[test]
fn both_argument_shapes_at_once_are_refused_rather_than_guessed() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");

    let result = bash(
        &base.tools(&["counter"]),
        json!({
            "command": format!("{} one", stand_in.display()),
            "argv": [stand_in.display().to_string(), "two"]
        }),
    );

    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("argv") && result.output.contains("command"),
        "the refusal names both shapes rather than picking one: {}",
        result.output
    );
    assert!(!base.mark("counter-calls.txt").exists());
}

#[test]
fn a_cwd_argument_is_refused_as_unsupported_in_this_build() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");

    let result = bash(
        &base.tools(&["counter"]),
        json!({"argv": [stand_in.display().to_string()], "cwd": base.ws().display().to_string()}),
    );

    assert!(
        !base.mark("counter-calls.txt").exists(),
        "a `cwd` must be refused, not quietly ignored while the command runs"
    );
    assert!(result.is_error, "{}", result.output);
    assert!(result.output.contains("cwd"), "{}", result.output);
    assert!(
        result.output.contains("not support"),
        "the refusal says it is unsupported in this build: {}",
        result.output
    );
}

#[test]
fn a_line_whose_first_token_is_a_shell_is_refused_as_that_token() {
    // The `argv[0]` in a shell implementation would be the shell, and the allow-list would
    // then be checked against the wrong program — which is the whole reason there is no
    // shell (`DESIGN.md` §20). Here the first token *is* a shell, and the refusal names it.
    let base = Base::new();
    let stand_in = counting(&base, "counter");
    let line = format!(
        "{} \"{}\"",
        if cfg!(windows) { "cmd /C" } else { "sh -c" },
        stand_in.display()
    );

    let result = bash(&base.tools(&["counter"]), json!({ "command": line }));

    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("tools.bash_allow"),
        "the shell's own name has to fail the allow-list before anything is spawned: {}",
        result.output
    );
    assert!(
        !base.mark("counter-calls.txt").exists(),
        "a line beginning with a shell reached the stand-in"
    );
}

/// The no-shell property, behaviourally — and only where a stand-in can show it.
///
/// A `.bat` is itself a `cmd.exe` script, so on Windows `cmd` parses the arguments at that
/// boundary whatever pie did, and the difference is unobservable (`ACCEPTANCE.md` §19's
/// `S20` records the same shape for I1). On unix the stand-in is a plain executable and
/// `sh` is a separate program, so the two implementations come apart: with `sh -c` the
/// `&&` would run the stand-in twice and the `>` would create a file.
#[cfg(not(windows))]
#[test]
fn a_shell_line_is_not_interpreted() {
    let base = Base::new();
    let first = counting(&base, "counter");
    let second = counting(&base, "other");
    let tools = base.tools(&["counter", "other"]);

    let chained = bash(
        &tools,
        json!({ "command": format!("{} a && {} b", first.display(), second.display()) }),
    );

    assert_eq!(
        lines(&base.mark("counter-calls.txt")).len(),
        1,
        "`&&` ran a second command, so the line reached a shell: {}",
        chained.output
    );
    assert_eq!(
        lines(&base.mark("counter-args.txt")),
        vec![
            "a".to_string(),
            "&&".to_string(),
            second.display().to_string(),
            "b".to_string()
        ],
        "`&&` is an ordinary argument, not a separator"
    );
    assert!(!base.mark("other-calls.txt").exists());

    let redirected = bash(
        &tools,
        json!({ "command": format!("{} > redirected.txt", first.display()) }),
    );

    assert!(
        !base.ws().join("redirected.txt").exists(),
        "`>` redirected the command's output, so a shell interpreted it"
    );
    assert!(
        redirected.output.contains("hello-from-the-counter"),
        "the command's stdout must come back to pie, not into a file: {}",
        redirected.output
    );
    assert_eq!(
        lines(&base.mark("counter-argc.txt")),
        vec!["2"],
        "`>` and its target are two ordinary arguments"
    );
}

// ---------------------------------------------------------------------------------------
// The clock, the bound, and the exit status
// ---------------------------------------------------------------------------------------

#[test]
fn a_command_outliving_its_deadline_is_killed_not_waited_on() {
    let base = Base::new();
    let stand_in = sleeper(&base, "sleeper");
    let tools = Tools {
        timeout_secs: 1,
        ..base.tools(&["sleeper"])
    };

    let (result, elapsed) = timed(&tools, json!({"argv": [stand_in.display().to_string()]}));

    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("tools.timeout_secs"),
        "the failure names the deadline that fired: {}",
        result.output
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "a 1s deadline returned after {elapsed:?} — the child was waited on, not killed"
    );
    assert!(
        base.mark("sleeper-started.txt").exists(),
        "the stand-in never ran, so the wall-clock assertion above proves nothing"
    );
    assert!(
        !result.output.contains("the-child-outlived-its-deadline"),
        "the child's output arrived after the deadline: {}",
        result.output
    );

    // The wall clock alone cannot tell "killed" from "the deadline fired and the child was
    // left running": a killed child never reaches its third line, and this is where that is
    // checked. The child sleeps three seconds and the call returned after about one, so the
    // mark has to appear inside the poll window if the child is still alive.
    let give_up = Instant::now() + Duration::from_secs(4);
    while Instant::now() < give_up {
        assert!(
            !base.mark("sleeper-finished.txt").exists(),
            "the child outlived its deadline and finished its work anyway"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // ... and the poll above is only evidence if that mark appears when nothing kills the
    // child, so the same stand-in runs again under a deadline it cannot outlive. Without
    // this, a stand-in that never wrote the mark at all would satisfy the poll.
    let uninterrupted = sleeper(&base, "uninterrupted");
    let allowed = bash(
        &Tools {
            timeout_secs: 30,
            ..base.tools(&["uninterrupted"])
        },
        json!({"argv": [uninterrupted.display().to_string()]}),
    );
    assert!(!allowed.is_error, "{}", allowed.output);
    assert!(
        base.mark("uninterrupted-finished.txt").exists(),
        "the stand-in never writes that mark even when nothing kills it: {}",
        allowed.output
    );
}

#[test]
fn a_runaway_output_is_bounded_and_the_call_still_returns() {
    let base = Base::new();
    let stand_in = runaway(&base, "runaway");
    let tools = Tools {
        max_output_bytes: 400,
        // A deadline far longer than the test's patience: if the bound were not doing the
        // work, the call would come back at the deadline instead of promptly, and the
        // elapsed-time assertion below is what notices.
        timeout_secs: 60,
        ..base.tools(&["runaway"])
    };

    let (result, elapsed) = timed(&tools, json!({"argv": [stand_in.display().to_string()]}));

    assert!(
        result.output.len() <= 400,
        "a command that prints forever returned {} bytes",
        result.output.len()
    );
    assert!(
        result.output.contains("tools.max_output_bytes"),
        "a bounded output says it was bounded and names the cap: {}",
        result.output
    );
    assert!(result.is_error, "{}", result.output);
    assert!(
        elapsed < Duration::from_secs(20),
        "the cap returned after {elapsed:?} — the output was accumulated until the deadline \
         instead of being bounded"
    );
}

#[test]
fn a_non_zero_exit_is_a_reported_failure_and_not_a_broken_tool() {
    let base = Base::new();
    let stand_in = failer(&base, "failer");

    let result = bash(
        &base.tools(&["failer"]),
        json!({"argv": [stand_in.display().to_string()]}),
    );

    // The ruling: the call ran, so it is answered with the real output and the status — not
    // refused, and not a protocol error. `is_error` is set because the command reported
    // failure, and it is set *by pie* rather than left for the model to parse out of prose.
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("boom"),
        "a failing command's own stdout is still the answer: {}",
        result.output
    );
    assert!(
        result.output.contains("status 3"),
        "the exit status is stated rather than implied: {}",
        result.output
    );
    assert!(
        result.output.contains("boom-err"),
        "stderr is where a compiler puts its errors, so it has to reach the model: {}",
        result.output
    );
}

#[test]
fn a_zero_exit_is_not_an_error_and_produces_no_status_line() {
    let base = Base::new();
    let stand_in = counting(&base, "counter");

    let result = bash(
        &base.tools(&["counter"]),
        json!({"argv": [stand_in.display().to_string()]}),
    );

    assert!(!result.is_error, "{}", result.output);
    assert!(
        !result.output.contains("exited with status"),
        "`exit 0` is not something to act on, so it is not stamped: {}",
        result.output
    );
}

#[test]
fn a_command_that_prints_nothing_says_so() {
    let base = Base::new();
    let stand_in = quiet(&base, "quiet");

    let result = bash(
        &base.tools(&["quiet"]),
        json!({"argv": [stand_in.display().to_string()]}),
    );

    assert!(!result.is_error, "{}", result.output);
    assert!(
        result.output.contains("no output"),
        "an empty result is indistinguishable from a tool that never ran: {:?}",
        result.output
    );
}

// ---------------------------------------------------------------------------------------
// The child's world: where it runs, and what it can see
// ---------------------------------------------------------------------------------------

#[test]
fn the_child_runs_in_the_workspace_root() {
    let base = Base::new();
    let stand_in = reader(&base, "reader");

    let inside = bash(
        &base.tools(&["reader"]),
        json!({"argv": [stand_in.display().to_string()]}),
    );
    assert!(
        inside.output.contains("workspace-file"),
        "the child's working directory is not the workspace root: {}",
        inside.output
    );

    // The control: with a root that does **not** hold `hello.txt` the same command fails, so
    // the read above succeeded because of the `cwd` and not because every root has the file.
    // A bare `tempfile::tempdir` rather than a second `Base`: `Base` always writes the file
    // into its workspace, which is exactly what made the first version of this control pass
    // for the wrong reason.
    let elsewhere = tempfile::tempdir().unwrap();
    let empty_root = elsewhere.path().join("ws");
    fs::create_dir_all(&empty_root).unwrap();
    let outside = bash(
        &Tools {
            workspace: Some(empty_root),
            ..base.tools(&["reader"])
        },
        json!({"argv": [stand_in.display().to_string()]}),
    );
    assert!(
        outside.is_error && !outside.output.contains("workspace-file"),
        "the reader found the file from an unrelated root, so `cwd` is not being set: {}",
        outside.output
    );
}

#[test]
fn the_child_does_not_see_a_scrubbed_api_key() {
    // I2 is about *every* child, and the key this removes is `BUILTIN_SCRUB`'s, which is what
    // `Tools::default()` carries. The test below covers the *other* half of the list - the keys
    // `Config::resolve` adds from `general.extra_scrub_env` (S21).
    const KEY: &str = "OPENAI_API_KEY";
    const VALUE: &str = "sk-must-not-reach-a-bash-child";

    let base = Base::new();
    let stand_in = dumper(&base, "dumper");
    let _restore = EnvVar::set(KEY, VALUE);

    let result = bash(
        &base.tools(&["dumper"]),
        json!({"argv": [stand_in.display().to_string()]}),
    );
    assert!(!result.is_error, "{}", result.output);

    let dumped = content(&base.mark("dumper-env.txt"));
    assert!(
        dumped.contains("PATH") || dumped.contains("Path"),
        "the dump is empty or incomplete, so the assertion below would be vacuous"
    );
    assert!(
        !dumped.contains(VALUE),
        "`{KEY}`'s value reached the child (I2)"
    );
    assert!(
        !dumped.contains(KEY),
        "`{KEY}`'s name reached the child (I2)"
    );
}

/// The other half of the scrub list, and the half **S21** added.
///
/// The test above covers `BUILTIN_SCRUB`, which `Tools::default()` carries; this covers the keys a
/// user adds in `general.extra_scrub_env`, which `Config::resolve` composes on top. Both are worth
/// asserting separately because the first version of this tier passed the constant and nothing
/// else - so a key someone had deliberately added to their own deny-list reached a *model-invoked*
/// command while every vendor child was correctly scrubbed. The gap was never that the scrub list
/// was wrong; it was that this child saw a different list from every other child.
#[test]
fn the_child_does_not_see_a_key_the_user_added_to_the_scrub_list() {
    const KEY: &str = "PIE_TEST_CORP_TOKEN";
    const VALUE: &str = "corp-token-must-not-reach-a-bash-child";

    let base = Base::new();
    let stand_in = dumper(&base, "dumper");
    let _restore = EnvVar::set(KEY, VALUE);

    let tools = Tools {
        scrub: vec![KEY.to_string()],
        ..base.tools(&["dumper"])
    };
    let result = bash(&tools, json!({"argv": [stand_in.display().to_string()]}));
    assert!(!result.is_error, "{}", result.output);

    let dumped = content(&base.mark("dumper-env.txt"));
    assert!(
        dumped.contains("PATH") || dumped.contains("Path"),
        "the dump is empty or incomplete, so the assertions below would be vacuous"
    );
    assert!(
        !dumped.contains(VALUE),
        "`{KEY}`'s value reached the child (I2, S21)"
    );
    assert!(
        !dumped.contains(KEY),
        "`{KEY}`'s name reached the child (I2, S21)"
    );
}

/// `std::env` is process-wide and the test harness is threaded, so the variable is removed
/// again on the way out rather than left for another test in this binary to inherit —
/// including on a panic, which is what a bare `set_var` plus `remove_var` would not do.
struct EnvVar;

impl EnvVar {
    fn set(key: &str, value: &str) -> Self {
        std::env::set_var(key, value);
        Self
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        std::env::remove_var("OPENAI_API_KEY");
    }
}

// ---------------------------------------------------------------------------------------
// What the source does not reach for
// ---------------------------------------------------------------------------------------

#[test]
fn the_tier_names_no_network_credential_or_vendor_store_symbol() {
    // Static, and therefore partial: it proves the source reaches for no socket, no
    // environment, and no vendor store *by name*. The behavioural half is the rest of this
    // file, where the only thing that ever happens is a spawn of an allow-listed path.
    let source =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/bash.rs")).unwrap();

    for forbidden in [
        "TcpStream",
        "UdpSocket",
        "reqwest",
        "hyper::",
        "std::env",
        "api_key",
        "credential",
        "Authorization",
        "PIE_HOME",
        "Command::new",
    ] {
        assert!(
            !source.contains(forbidden),
            "the executing tier must not name `{forbidden}`: it spawns one allow-listed \
             executable and reads nothing else"
        );
    }
}

#[test]
fn the_tier_reads_argv_zero_directly_and_names_no_shell() {
    // The Windows evidence for "there is no shell", and a source-text assertion for `S20`'s
    // reason: a `.bat` stand-in cannot show the difference on this platform (see the module
    // doc). Comments are skipped because this module *documents* the shell it refuses to use.
    let source =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/bash.rs")).unwrap();

    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<&str>>()
        .join("\n");

    // Non-vacuity first: a file with no spawn at all would satisfy the loop below.
    assert!(
        code.contains("ChildSession::spawn"),
        "the scan found no spawn, so `names no shell` says nothing about this file"
    );
    assert!(code.contains("resolve_binary"), "argv[0] is not resolved");
    assert!(code.contains("RunSpec {"), "no RunSpec is built");

    for forbidden in [
        "\"cmd\"",
        "\"cmd.exe\"",
        "\"sh\"",
        "\"sh.exe\"",
        "\"powershell\"",
        "\"bash -c\"",
        "\"sh -c\"",
        "\"cmd /\"",
        "\"-c\"",
        "\"/C\"",
        "\"/c\"",
    ] {
        assert!(
            !code.contains(forbidden),
            "the executing tier names `{forbidden}` outside a comment, which is how a shell \
             would get the command line — and the allow-list would then be checking the shell \
             rather than the program the model asked for"
        );
    }
    // `"bash"` is deliberately not in the list above: it is this module's backend id and the
    // tool's own name, so forbidding the literal would be a rule about a word rather than
    // about spawning a shell.
    assert!(code.contains("\"bash\""), "the backend id is gone?");
}

// ---------------------------------------------------------------------------------------
// Names the filesystem cannot honour
// ---------------------------------------------------------------------------------------

#[test]
fn a_name_with_no_executable_behind_it_is_refused_rather_than_spawned() {
    let base = Base::new();

    let missing = bash(
        &base.tools(&["nowhere-near-here"]),
        json!({"argv": ["nowhere-near-here"]}),
    );
    assert!(missing.is_error, "{}", missing.output);
    assert!(
        missing.output.contains("no such executable"),
        "an allow-listed name with nothing behind it is a stated refusal, not a raw spawn \
         error: {}",
        missing.output
    );

    // And the same refusal for names whose bytes are shorter than the executable suffixes:
    // `strip_exe_suffix` cuts `name.len() - 4` bytes, and on a name that ends mid-character
    // that cut is not a character boundary. The assertion is simply that a `ToolResult`
    // comes back at all — a panic inside the refusal path would fail the test by unwinding.
    for name in ["éabc", "café.exe"] {
        let odd = bash(&base.tools(&[name]), json!({"argv": [name]}));
        assert!(odd.is_error, "{name}: {}", odd.output);
        assert_eq!(odd.id, "toolu_bash");
    }
}
