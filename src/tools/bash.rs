//! `bash`, the executing tier (`DESIGN.md` §20).
//!
//! Off by default, and scoped when on. Four properties, each of which is a check in
//! `ACCEPTANCE.md` §19:
//!
//! 1. `argv[0]` is checked against `tools.bash_allow` **before the spawn**, so a refused
//!    command is a stated error rather than a `PermissionDenied` the model has to
//!    interpret.
//! 2. A command that outlives its deadline is **killed**, not waited on (`RISKS.md` §6).
//! 3. A command that prints forever is bounded rather than accumulated — the reason
//!    `read_body` is capped, and `B22`'s lesson applied to a new input.
//! 4. There is **no shell**. The line is parsed into argv by pie and `argv[0]` is exec'd
//!    directly — which is what the allow-list in (1) depends on, so it is spelled out
//!    below rather than left to be inferred from the code.
//!
//! # There is no shell, and that is the point rather than a shortcut
//!
//! If pie ran the command through a shell, then `argv[0]` would be *the shell*, and
//! `tools.bash_allow` would be checked against the wrong program: `bash_allow = ["cargo"]`
//! would admit `sh -c 'rm -rf /'` while looking like a boundary. So the line is tokenised
//! here and `argv[0]` is executed directly. The consequences are documented rather than
//! hidden, because a model has to be able to predict them:
//!
//! * no pipes, no redirection, no `&&`/`||`/`;`, no globbing, no `$VAR` expansion — every
//!   one of those characters reaches the program as part of an ordinary argument;
//! * the *program* decides what its arguments mean, and pie decides nothing else;
//! * an allow-listed `.cmd`/`.bat` **is itself a cmd script**, so `cmd.exe` parses the
//!   arguments pie hands it. That is a property of what was allow-listed, not of how pie
//!   spawns it — and it is the same class of residual as the one below.
//!
//! # The argument shape, chosen here because the contract did not fix one
//!
//! | Shape | Meaning |
//! |---|---|
//! | `{"command": "cargo test --quiet"}` | one line, split by pie's [`split`] |
//! | `{"argv": ["cargo", "test", "--quiet"]}` | already separated; no quoting questions |
//!
//! The splitter is deliberately minimal: whitespace separates, `'`/`"` group, and
//! **nothing is escaped** — a backslash is an ordinary character, which is what keeps a
//! Windows path intact. An unterminated quote, an empty line, an empty array, a
//! wrong-typed or missing field, and both shapes at once are each a stated refusal naming
//! the argument: a generic error makes a model guess, and the reader of these sentences is
//! a model. A `cwd` argument is refused too — see [`arguments`].
//!
//! # What this does not claim
//!
//! * **The allow-list is a name, not an identity.** A different executable that happens to
//!   be called `cargo` and sits earlier in `PATH` is admitted, because nothing here
//!   inspects the binary. `DESIGN.md` §20 admits exactly this boundary — *"the boundary is
//!   'this executable'"* — and a name match is its honest form; do not read a guarantee
//!   into it that it does not make.
//! * **`tools.workspace` is a starting directory, not a sandbox.** It is the child's
//!   working directory, and nothing more: an allow-listed `git` can still read and write
//!   anywhere the user can. What is decided is *which executables may run*, not what a
//!   command means.
//! * **stderr is a best-effort tail.** It is drained on a detached thread
//!   (`ChildSession`), so a child that exits immediately can outrun the drain; the tail is
//!   included when it is there, and its absence is not a claim that the command was quiet.
//!
//! # Ownership
//!
//! **Stage B of M11 owns this file** (`ACCEPTANCE.md` §19, stage B; gate
//! `cargo test --test tools_bash`), together with `tests/tools_bash.rs`. The signature
//! below is frozen by M11's contract; the body is not. `bash` spawns through
//! `process::ChildSession`, which is the **one** spawn policy (`DESIGN.md` §20, "One spawn
//! policy, not three") — a `std::process::Command` here would be a third copy of the
//! hidden-window, env-scrub and kill-tree guarantees, free to drift from the other two.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::Tools;
use crate::error::PieError;
use crate::process::{resolve_binary, ChildSession, RunSpec};
use crate::tool_loop::{ToolCall, ToolResult};

/// The backend id a `bash` child is spawned under, for `PieError` and the log.
const BACKEND: &str = "bash";

/// The refusal `bash = false` produces.
///
/// Carried from M11 wave 0 unchanged: it was written for a model to read, it names the
/// config key, and `ACCEPTANCE.md` §19's "the answer names the config key" check is about
/// this sentence and the one below.
const DISABLED: &str =
    "the `bash` tool is not enabled. `bash` runs arbitrary commands, so it is opt-in \
     and scoped: set `tools.bash = true` and list the executables it may run in \
     `tools.bash_allow`. Until then, continue without it.";

/// The refusal an empty `tools.bash_allow` produces: enabled, and still nothing may run.
const EMPTY_ALLOW: &str =
    "the `bash` tool is enabled but `tools.bash_allow` is empty, so no command may \
     run. Add the executables it may run (`bash_allow = [\"cargo\", \"git\"]`) — the \
     allow-list names executables, not command strings. Until then, continue without it.";

/// Run one `bash` call.
pub fn run(cfg: &Tools, call: &ToolCall) -> ToolResult {
    match execute(cfg, call) {
        Ok(outcome) => ToolResult {
            id: call.id.clone(),
            output: outcome.output,
            is_error: outcome.failed,
        },
        // A refusal is an error result the model can read and act on, and it carries the
        // call's id like every other result (`crate::tools::refused`).
        Err(why) => crate::tools::refused(call, why),
    }
}

/// What one settled call returns: the text a model reads, and whether the command failed.
#[derive(Debug)]
struct Outcome {
    output: String,
    failed: bool,
}

/// The call, or the sentence explaining why nothing was spawned.
///
/// The order is policy, then arguments, then spawn — and it is the *observable* part of
/// this tier: a disabled tool is refused without the call's arguments being read at all, an
/// unlisted `argv[0]` is refused without a process being created, and the allow-list is
/// decided on the program the **model named** rather than on anything pie resolved
/// afterwards.
fn execute(cfg: &Tools, call: &ToolCall) -> Result<Outcome, String> {
    if !cfg.bash {
        return Err(DISABLED.to_string());
    }

    // A workspace is the single enabling decision for the whole suite, and `bash` is not an
    // exception: without one there is no root to run in, and the refusal says which key
    // would supply it.
    let Some(configured) = cfg.workspace.as_deref() else {
        return Err(
            "the `bash` tool is not enabled. pie offers no tools until a workspace root is \
             configured: set `tools.workspace` in config.toml to the directory the tools may \
             work in. Until then, continue without it."
                .to_string(),
        );
    };

    if cfg.bash_allow.is_empty() {
        return Err(EMPTY_ALLOW.to_string());
    }

    let (program, args, cwd) = arguments(call)?;

    if !cfg
        .bash_allow
        .iter()
        .any(|allowed| same_program(allowed, &program))
    {
        return Err(format!(
            "`{program}` is not in `tools.bash_allow` (allowed: {}). The allow-list names \
             executables, not command strings, and it is checked before anything is spawned, \
             so nothing ran. Do not retry this command — use one of the allowed executables, \
             or say what has to be added to `tools.bash_allow`.",
            allowed_names(&cfg.bash_allow)
        ));
    }

    let root = canonical_root(configured)?;
    let Some(binary) = resolve_binary(&program) else {
        return Err(format!(
            "`{program}` is allowed by `tools.bash_allow` but no such executable was found (a \
             name with a path separator is used as given, otherwise `PATH` is searched). \
             Nothing ran. Check the spelling, or write the program as a full path — do not \
             retry this call unchanged."
        ));
    };

    // Resolved **after** the program is known to be allowed and to exist, so a refused command
    // touches no filesystem at all. The check itself is the shared ceiling, not a copy of it.
    let cwd = match cwd {
        Some(requested) => {
            let resolved = crate::tools::guard::resolve_within(&root, &requested)?;
            if !resolved.is_dir() {
                return Err(format!(
                    "`cwd` is `{requested}`, which is inside the workspace root but is not a \
                     directory. Give a directory, or omit `cwd` to run in the workspace root."
                ));
            }
            resolved
        }
        None => root.clone(),
    };

    run_command(cfg, &cwd, binary, args)
}

/// The workspace root as the child's `cwd`: resolved once, so the child starts in the
/// directory the configuration actually names rather than in whatever `..` happens to mean
/// from pie's own working directory.
fn canonical_root(configured: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(configured).map_err(|error| {
        format!(
            "the configured `tools.workspace` root `{}` cannot be resolved ({error}). Fix the \
             path in config.toml and run the call again.",
            configured.display()
        )
    })
}

// ---------------------------------------------------------------------------------------
// Arguments.
// ---------------------------------------------------------------------------------------

/// The program, its arguments, and an optional working directory, from either accepted shape.
///
/// **`cwd` is accepted and resolved through the shared ceiling** (`crate::tools::guard`), which
/// is the same check the read-only tier uses. M11 refused the argument outright, and the reason
/// was sound while it held: a second copy of the path check would be free to drift from the
/// first, and each copy would be tested only by its own tier's tests. Now that there is one
/// implementation (S19), accepting the argument costs nothing and the boundary is unchanged. A
/// `cwd` outside the root is refused exactly as a path outside the root is refused.
fn arguments(call: &ToolCall) -> Result<(String, Vec<String>, Option<String>), String> {
    let Value::Object(fields) = &call.input else {
        return Err(format!(
            "the `bash` tool takes an object with `command` or `argv`. {}",
            shape_help()
        ));
    };

    let cwd = match fields.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(format!(
                "`cwd` must be a string naming a directory inside the workspace root. Omit it to \
                 run in the workspace root. {}",
                shape_help()
            ));
        }
    };

    let (program, args) = match (fields.get("argv"), fields.get("command")) {
        // Refused rather than resolved by precedence: a call that sends both is a call whose
        // author was unsure, and silently picking one hides that.
        (Some(_), Some(_)) => {
            return Err(format!(
                "the `bash` tool takes `argv` or `command`, not both, and which one was meant is \
                 not something to guess. {}",
                shape_help()
            ))
        }
        (Some(value), None) => parse_argv(value)?,
        (None, Some(value)) => parse_command(value)?,
        (None, None) => {
            return Err(format!(
                "the `bash` tool needs `command` or `argv`, and neither was present, so nothing \
                 ran. {}",
                shape_help()
            ))
        }
    };

    Ok((program, args, cwd))
}

/// The sentence every argument-shaped refusal ends with, so a model gets the shape it needs
/// rather than a description of the one it sent.
fn shape_help() -> &'static str {
    "Send `{\"command\": \"cargo test --quiet\"}` (one line, split by pie, with `\"` and `'` \
     for arguments that contain spaces) or `{\"argv\": [\"cargo\", \"test\", \"--quiet\"]}` \
     (already separated, no quoting questions). Do not retry the call unchanged."
}

/// `{"argv": [...]}` — the program first, then its arguments.
fn parse_argv(value: &Value) -> Result<(String, Vec<String>), String> {
    let Value::Array(items) = value else {
        return Err(format!(
            "`argv` must be an array of strings, with the program first, but it is not an array. \
             {}",
            shape_help()
        ));
    };

    let mut words: Vec<String> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Value::String(word) = item else {
            return Err(format!(
                "`argv[{index}]` must be a string, so pie cannot tell which executable or \
                 argument was meant. {}",
                shape_help()
            ));
        };
        words.push(word.clone());
    }

    let program = match words.first() {
        Some(first) if !first.trim().is_empty() => first.clone(),
        _ => {
            return Err(format!(
                "`argv` must name the program to run as its first element, and it \
                 {} — so there is nothing to run. {}",
                if words.is_empty() {
                    "is empty"
                } else {
                    "starts with an empty string"
                },
                shape_help()
            ))
        }
    };

    Ok((program, words.into_iter().skip(1).collect()))
}

/// `{"command": "…"}` — one line, split by [`split`].
fn parse_command(value: &Value) -> Result<(String, Vec<String>), String> {
    let Value::String(line) = value else {
        return Err(format!(
            "`command` must be a string holding one command line, but it is not a string. {}",
            shape_help()
        ));
    };

    let mut words = split(line)?.into_iter();
    let Some(program) = words.next() else {
        return Err(format!(
            "`command` is empty, so there is nothing to run. {}",
            shape_help()
        ));
    };
    Ok((program, words.collect()))
}

/// Split a command line into arguments: whitespace separates, and `'`/`"` group.
///
/// Deliberately minimal, and documented because a model has to be able to predict it:
///
/// * whitespace outside quotes separates arguments;
/// * `'` or `"` starts a quoted region, ended by the same character, and the quotes are not
///   part of the argument (a quote may open mid-word: `--msg="a b"` is one argument);
/// * **no escapes at all** — a backslash is an ordinary character, which is what keeps a
///   Windows path (`"C:\tmp\x"`) intact where a shell-style escape rule would corrupt it;
/// * no expansion, no globbing, no redirection, no pipelines, no `&&` or `;`: each of those
///   is an ordinary character that reaches the program inside an argument.
///
/// An unterminated quote is a refusal rather than a guess: the split would depend on which
/// end pie decided to believe, and a command that runs differently from how it reads is the
/// one failure mode this tier must not have.
fn split(line: &str) -> Result<Vec<String>, String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;

    for character in line.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' => quote = Some(character),
            None if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
                continue;
            }
            None => current.push(character),
        }
        // Reached for every character that was appended to (or opened) a word — including
        // an empty quoted region, which is an argument that happens to be empty.
        started = true;
    }

    if let Some(open) = quote {
        return Err(format!(
            "`command` has an unterminated `{open}` quote, so pie cannot tell which arguments \
             were meant and nothing ran. Close the quote, or use `argv`, which has no quoting \
             at all."
        ));
    }
    if started {
        words.push(current);
    }
    Ok(words)
}

// ---------------------------------------------------------------------------------------
// The allow-list.
// ---------------------------------------------------------------------------------------

/// Whether `allowed` names `program`, compared on the **file name**.
///
/// A name match and not an identity match, deliberately and with the residual stated at the
/// top of this module: nothing here inspects the binary, so an executable called `cargo`
/// that is not the `cargo` meant is admitted. That is `DESIGN.md` §20's admitted boundary —
/// the alternative is claiming a guarantee (a hash, a signed path) that this build cannot
/// make, which would be worse than the admission.
///
/// A path or a bare name on either side is fine, because only the last component is
/// compared: `bash_allow = ["cargo"]` matches `C:\tools\cargo.exe` on Windows and
/// `/usr/bin/cargo` on unix alike.
#[cfg(windows)]
fn same_program(allowed: &str, program: &str) -> bool {
    // Case-insensitively, because Windows' filesystem is, and with the executable suffix
    // removed on both sides so that `cargo` and `cargo.exe` are one name rather than two.
    strip_exe_suffix(file_name(allowed)).eq_ignore_ascii_case(strip_exe_suffix(file_name(program)))
}

/// The unix arm: exact, and with no suffix rule.
///
/// Not a copy of the Windows arm with one detail changed — the *policy* differs, because
/// unix file names are case-sensitive and binary names carry no extension, so folding case
/// or stripping four characters here would admit names the filesystem does not consider the
/// same.
#[cfg(not(windows))]
fn same_program(allowed: &str, program: &str) -> bool {
    file_name(allowed) == file_name(program)
}

/// The last component of a program string, splitting on **either** separator.
///
/// Not `Path::file_name`: on unix that would leave `C:\tools\cargo.exe` whole (no `/` in
/// it), so a model that spells a Windows path would be compared against its directory too.
fn file_name(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// `cargo.exe` and `cargo` are the same executable on Windows, so a suffix is not part of
/// the name being compared. The list is `PATHEXT`'s, which is also how
/// [`resolve_binary`] finds a bare name in the first place.
#[cfg(windows)]
fn strip_exe_suffix(name: &str) -> &str {
    for suffix in [".exe", ".cmd", ".bat", ".com"] {
        if suffix.len() < name.len() {
            // `get` rather than slicing arithmetic: the name is model-supplied, and a
            // four-byte cut is not a character boundary in a name ending in a multi-byte
            // character ("café.exe" is fine, cutting "café" at four bytes would panic).
            if let Some(tail) = name.get(name.len() - suffix.len()..) {
                if tail.eq_ignore_ascii_case(suffix) {
                    return name.get(..name.len() - suffix.len()).unwrap_or(name);
                }
            }
        }
    }
    name
}

/// The allow-list as a refusal should show it: trimmed, empty entries dropped, and no
/// duplicates that differ only in case.
fn allowed_names(allowed: &[String]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for name in allowed {
        let name = name.trim();
        if name.is_empty() || seen.iter().any(|kept| kept.eq_ignore_ascii_case(name)) {
            continue;
        }
        seen.push(name);
    }
    if seen.is_empty() {
        "none".to_string()
    } else {
        seen.join(", ")
    }
}

// ---------------------------------------------------------------------------------------
// The spawn, the read, and the status.
// ---------------------------------------------------------------------------------------

/// Why the read loop stopped.
#[derive(Debug)]
enum Stop {
    /// The child closed its output: the normal end.
    Eof,
    /// The output budget was spent, so the child was killed rather than drained forever.
    Budget,
    /// The command outlived its deadline; `ChildSession` killed the tree on the way out.
    Deadline,
    /// The output could not be read.
    Unreadable(String),
}

/// Spawn the command, read it bounded, and settle its exit status.
fn run_command(
    cfg: &Tools,
    cwd: &Path,
    program: PathBuf,
    args: Vec<String>,
) -> Result<Outcome, String> {
    // A zero deadline would expire before the child could start. `Config::effective_timeout`
    // reads a zero as one second (`.max(1)`), and this reads it the same way rather than
    // inventing a second meaning for the same value.
    let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
    // `Instant + Duration` panics on overflow and `tools.timeout_secs` is an unvalidated
    // `u64` from a config file, so a nonsense value has to become a sentence.
    let started = Instant::now();
    let deadline = started.checked_add(timeout).ok_or_else(|| {
        format!(
            "`tools.timeout_secs` is {}s, which is too large to schedule. Lower it in config.toml \
             and run the call again.",
            cfg.timeout_secs
        )
    })?;

    let spec = RunSpec {
        backend: BACKEND.to_string(),
        program,
        args: args.into_iter().map(OsString::from).collect(),
        cwd: cwd.to_path_buf(),
        scrub: scrub(cfg),
        timeout,
    };

    let mut session = ChildSession::spawn(&spec).map_err(|error| {
        format!(
            "`{}` could not be started ({error}). Nothing ran, and this is not something a retry \
             changes.",
            spec.program.display()
        )
    })?;

    let budget = cfg.max_output_bytes;
    let mut body = String::new();

    let mut stop = loop {
        // Checked before the read rather than after the append: a body that has exactly
        // filled the budget is indistinguishable from one that will keep growing, and the
        // only way to tell them apart is another read — which is the block this bound
        // exists to prevent.
        if body.len() >= budget {
            break Stop::Budget;
        }

        match session.next_line() {
            Ok(Some(line)) => {
                let room = budget - body.len();
                let piece = head(&line.text, room);
                // `line.truncated` is `process.rs`'s per-line cap, not this budget: a line
                // longer than the line cap is a broken or hostile stream either way, and it
                // takes the same "stop and kill" path with the note saying so.
                let cut = piece.len() < line.text.len() || line.truncated;
                body.push_str(piece);
                if cut || body.len() >= budget {
                    break Stop::Budget;
                }
                body.push('\n');
            }
            Ok(None) => break Stop::Eof,
            Err(PieError::Timeout { .. }) => break Stop::Deadline,
            Err(error) => break Stop::Unreadable(error.to_string()),
        }
    };

    // Read *before* the session is dropped, because the drop is what closes the pipes and
    // kills the tree.
    let stderr = session.stderr_tail();

    // Every stop but `Eof` still needs the exit status if there is one, and none of them may
    // wait for it: `Eof` is the only case where the child has said it is finished.
    let mut status = None;
    let mut waited_out = false;
    if matches!(stop, Stop::Eof) {
        loop {
            match session.try_wait() {
                Ok(Some(exited)) => {
                    status = Some(exited);
                    break;
                }
                Ok(None) => {
                    // stdout EOF is not the exit: a child may close its output and keep
                    // working. It is waited for here, bounded by the same deadline the read
                    // used, and killed by the session's `Drop` if it overstays.
                    if Instant::now() >= deadline {
                        waited_out = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    stop = Stop::Unreadable(error.to_string());
                    break;
                }
            }
        }
    }

    // The session is dropped here, which closes the child's stdin and kills the tree — the
    // point rather than an accident for `Stop::Budget`, where the child is still printing.
    drop(session);

    let note = match &stop {
        Stop::Eof if waited_out => Some(format!(
            "\n[the command closed its output but did not exit within {}s (`tools.timeout_secs`), \
             so it was killed]",
            timeout.as_secs()
        )),
        Stop::Eof => None,
        Stop::Budget => Some(capped_note(budget)),
        Stop::Deadline => Some(format!(
            "\n[the command did not finish within {}s (`tools.timeout_secs`), so it was killed \
             along with anything it spawned]",
            timeout.as_secs()
        )),
        Stop::Unreadable(error) => Some(format!(
            "\n[the command's output could not be read ({error}), so it was killed]"
        )),
    };

    let status_line = status.as_ref().and_then(status_line);
    // The ruling, in one line: a non-zero exit is a **call that ran and reported a failure**,
    // not a protocol error and not a refusal — so the real output is returned, the status is
    // stated, and `is_error` is set. A command that legitimately fails (`cargo test`) must
    // not look like a broken tool, and a result that hid the status behind prose would make
    // the model re-derive it from the text.
    let failed = !matches!(status.as_ref(), Some(exited) if exited.success());

    Ok(Outcome {
        output: compose(
            body,
            &stderr,
            note.as_deref(),
            status_line.as_deref(),
            budget,
        ),
        failed,
    })
}

/// The environment a `bash` child does not see (invariant I2).
///
/// I2 is about *every* child, so this child gets the same deny-list a vendor CLI gets - and
/// since **S21** it gets the **whole** list, not just [`BUILTIN_SCRUB`]. `Tools::scrub` is
/// composed by `Config::resolve` from `BUILTIN_SCRUB` plus the user's `general.extra_scrub_env`,
/// so a key someone added to their own deny-list now reaches this child too.
///
/// The earlier version passed the constant and documented the shortfall, on the grounds that
/// re-parsing `config.toml` inside a tool call would make a child's environment depend on a
/// second read of the configuration (and on `HOME`, so a test could pass here and fail in CI).
/// That reasoning was right about the *mechanism* and wrong about the *conclusion*: the fix was
/// never to re-read anything, it was to carry the already-resolved list, which is why
/// `Tools::scrub` exists instead of a second parse or a hand-copied list.
fn scrub(cfg: &Tools) -> Vec<String> {
    cfg.scrub.clone()
}

/// The sentence that stops a runaway output.
///
/// Written without claiming the command *was* still printing: pie stops reading at the cap
/// and kills rather than find out, because finding out means another read and another read
/// is the block this bound exists to prevent.
fn capped_note(budget: usize) -> String {
    format!(
        "\n[output capped at {budget} bytes by `tools.max_output_bytes` (or a single line hit \
         pie's one-megabyte line cap); pie stopped reading there and killed the command rather \
         than drain it without bound — narrow the command, or raise the cap]"
    )
}

/// The exit status as a model should read it: silent on success, explicit otherwise.
///
/// Silent on `exit 0` because there is nothing to act on; explicit on anything else because
/// that is the whole reason `is_error` is set, and a status encoded only as "it failed"
/// would cost the model a second call to find out what happened.
fn status_line(exited: &std::process::ExitStatus) -> Option<String> {
    match exited.code() {
        Some(0) => None,
        Some(code) => Some(format!("\n[the command exited with status {code}]")),
        None => Some(
            "\n[the command was terminated by a signal rather than exiting, so it has no exit \
             code]"
                .to_string(),
        ),
    }
}

/// stdout, then stderr, then the note and the status — all inside one budget.
///
/// The stamps are laid out **before** the body is trimmed, for `read_only::capped`'s reason
/// in reverse: a truncation whose explanation was itself cut off reads as a complete answer,
/// and that is the one thing a bounded tool must not do. So the body is what gets cut, never
/// the sentence explaining why.
///
/// The consequence, stated rather than left to be discovered: with a cap smaller than the
/// stamps themselves the stamps win, and the result is longer than `tools.max_output_bytes`
/// by the length of one explanation. Nothing is *accumulated* in that case — the body never
/// exceeds the cap — and a cap that hides its own reason is worse than one that overruns by
/// a line.
fn compose(
    body: String,
    stderr: &str,
    note: Option<&str>,
    status_line: Option<&str>,
    budget: usize,
) -> String {
    let mut stamps = String::new();
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        // Half the budget at most: stderr is a diagnostic, and a chatty one must not crowd
        // out the answer it is meant to explain.
        let room = budget / 2;
        let header = "\n[stderr]\n";
        stamps.push_str(header);
        stamps.push_str(head(stderr, room.saturating_sub(header.len() + 1)));
        stamps.push('\n');
    }
    if let Some(note) = note {
        stamps.push_str(note);
    }
    if let Some(status_line) = status_line {
        stamps.push_str(status_line);
    }

    let keep = budget.saturating_sub(stamps.len());
    let mut out = head(&body, keep).to_string();
    out.push_str(&stamps);

    if out.trim().is_empty() {
        // An empty result is indistinguishable from a tool that did not run. One line says
        // which it was.
        out = "(the command produced no output and exited successfully)".to_string();
    }
    out
}

/// The longest prefix of `text` that fits in `budget` bytes, never splitting a character.
fn head(text: &str, budget: usize) -> &str {
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
