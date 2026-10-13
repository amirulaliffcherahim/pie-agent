//! Gate for **stage A of M11** (`ACCEPTANCE.md` §19): the read-only tier.
//!
//! `cargo test --test tools` is the stage's command, and every `.output` assertion below
//! is on what a *model* would read, because that is the reader the refusals are written
//! for.
//!
//! Two things here are deliberate rather than incidental. First, the fixture lives in two
//! trees: a workspace, and a sibling the workspace may not reach — so `..` and a symlink
//! have something real to escape *to*, and a refusal can be proven by the content that
//! must not appear. Second, the "no tool runs at all" claim is asserted with a counting
//! stand-in rather than by the absence of an error, since an absence is what a decorative
//! gate returns.
//!
//! Runs no vendor CLI, opens no socket, and needs no credential.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};

use pie::config::Tools;
use pie::tool_loop::{ToolCall, ToolResult, ToolRunner};
use pie::tools::ToolSet;

/// One fixture workspace plus the sibling tree that escapes from it must reach.
///
/// ```
/// <base>/ws/top.txt            "alpha\nbeta\n"
/// <base>/ws/a.rs
/// <base>/ws/sub/note.txt       line 3 is "the needle line"
/// <base>/ws/sub/b.rs
/// <base>/ws/empty/
/// <base>/escaped.txt           a sibling outside the root, holding "the needle"
/// <base>/outside/secret.txt    a directory outside the root, for the symlink case
/// <base>/ws-extra/twin.txt     a sibling whose name *starts with* the workspace's name
/// ```
struct Fixture {
    base: tempfile::TempDir,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("ws");
        fs::create_dir_all(workspace.join("sub")).unwrap();
        fs::create_dir_all(workspace.join("empty")).unwrap();
        fs::write(workspace.join("top.txt"), "alpha\nbeta\n").unwrap();
        fs::write(workspace.join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(
            workspace.join("sub/note.txt"),
            "one\ntwo\nthe needle line\nfour\n",
        )
        .unwrap();
        fs::write(workspace.join("sub/b.rs"), "fn b() {}\n").unwrap();
        fs::write(base.path().join("escaped.txt"), "outside\nthe needle\n").unwrap();
        fs::create_dir_all(base.path().join("outside")).unwrap();
        fs::write(base.path().join("outside/secret.txt"), "outside secret\n").unwrap();
        // A sibling whose *name* starts with the workspace's name. `"…\\ws-extra"` begins
        // with the text `"…\\ws"`, so this is the directory a string-prefix check admits
        // and a component check does not.
        fs::create_dir_all(base.path().join("ws-extra")).unwrap();
        fs::write(
            base.path().join("ws-extra/twin.txt"),
            "outside\nthe needle\n",
        )
        .unwrap();
        Self { base, workspace }
    }

    fn root(&self) -> &Path {
        &self.workspace
    }

    fn base(&self) -> &Path {
        self.base.path()
    }

    fn tools(&self) -> Tools {
        Tools {
            workspace: Some(self.workspace.clone()),
            ..Tools::default()
        }
    }

    /// One call through the real router, configured for this workspace.
    fn run(&self, name: &str, input: Value) -> ToolResult {
        ToolSet::new(self.tools()).run(&call(name, input))
    }
}

fn call(name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: "toolu_1".to_string(),
        name: name.to_string(),
        input,
    }
}

/// Counts the calls it forwards.
///
/// The point of a counting stand-in is that it distinguishes "the tier answered" from
/// "nothing happened and no error was printed" — the two are indistinguishable to a test
/// that only greps for an absence.
struct Counting<'a> {
    set: &'a ToolSet,
    calls: AtomicUsize,
}

impl<'a> Counting<'a> {
    fn new(set: &'a ToolSet) -> Self {
        Self {
            set,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ToolRunner for Counting<'_> {
    fn run(&self, call: &ToolCall) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.set.run(call)
    }
}

/// Every read-tier tool name with a well-formed argument pointing at a real fixture file.
fn read_tier_calls() -> [(&'static str, Value); 4] {
    [
        ("read", json!({"file_path": "top.txt"})),
        ("ls", json!({"path": "."})),
        ("grep", json!({"pattern": "needle"})),
        ("find", json!({"pattern": "*.rs"})),
    ]
}

// ---------------------------------------------------------------------------------------
// The tier works.
// ---------------------------------------------------------------------------------------

#[test]
fn a_file_reads() {
    let f = Fixture::new();
    let result = f.run("read", json!({"file_path": "top.txt"}));
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(result.output, "alpha\nbeta\n");
}

#[test]
fn a_read_accepts_the_path_alias() {
    let f = Fixture::new();
    let result = f.run("read", json!({"path": "top.txt"}));
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(result.output, "alpha\nbeta\n");
}

#[test]
fn a_directory_listing_is_stable() {
    let f = Fixture::new();
    let once = f.run("ls", json!({"path": "."}));
    let again = f.run("ls", json!({"path": "."}));
    assert!(!once.is_error, "{}", once.output);
    assert_eq!(
        once.output, again.output,
        "a listing that changes between calls is one a model cannot reason about"
    );
    assert_eq!(once.output, "a.rs\nempty/\nsub/\ntop.txt");
}

#[test]
fn a_grep_reports_the_path_and_the_line_number() {
    let f = Fixture::new();
    let result = f.run("grep", json!({"pattern": "needle", "path": "sub"}));
    assert!(!result.is_error, "{}", result.output);
    assert!(
        result.output.contains("sub/note.txt:3:the needle line"),
        "the report must carry path and line number, and be relative so it can be fed back \
         to `read`: {}",
        result.output
    );
}

#[test]
fn a_grep_says_so_when_it_finds_nothing() {
    let f = Fixture::new();
    let result = f.run("grep", json!({"pattern": "zzz-not-here"}));
    assert!(!result.is_error, "{}", result.output);
    assert!(
        result.output.contains("no line"),
        "an empty result must be stated, not returned as an empty string: {}",
        result.output
    );
}

#[test]
fn a_find_matches_a_basename_glob() {
    let f = Fixture::new();
    let result = f.run("find", json!({"pattern": "*.rs"}));
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(result.output, "a.rs\nsub/b.rs");
}

#[test]
fn a_find_matches_a_recursive_glob() {
    let f = Fixture::new();
    let result = f.run("find", json!({"pattern": "**/*.rs"}));
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(result.output, "a.rs\nsub/b.rs");
}

#[test]
fn a_find_with_no_wildcard_is_a_substring_of_the_name() {
    let f = Fixture::new();
    let result = f.run("find", json!({"pattern": "note"}));
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(result.output, "sub/note.txt");
}

// ---------------------------------------------------------------------------------------
// The ceiling: checked on the resolved path, for both ways a textual check is wrong.
// ---------------------------------------------------------------------------------------

#[test]
fn a_dotdot_path_escaping_the_root_is_refused() {
    let f = Fixture::new();
    let result = f.run("read", json!({"file_path": "../escaped.txt"}));
    assert!(
        result.is_error,
        "a `..` escape must be refused: {}",
        result.output
    );
    assert!(
        result.output.contains("outside the workspace root")
            && result.output.contains("tools.workspace"),
        "the refusal must say the root is a ceiling and name the key: {}",
        result.output
    );
    assert!(
        !result.output.contains("the needle"),
        "the escaped file's content must not appear in the refusal: {}",
        result.output
    );

    // The same escape, reached from a subdirectory, and through the other two tools. The
    // check is on where the path lands, so no spelling of the walk escapes it.
    let deep = f.run("read", json!({"file_path": "sub/../../escaped.txt"}));
    assert!(deep.is_error, "{}", deep.output);
    assert!(!deep.output.contains("the needle"), "{}", deep.output);

    let listed = f.run("ls", json!({"path": ".."}));
    assert!(listed.is_error, "{}", listed.output);
    assert!(!listed.output.contains("escaped.txt"), "{}", listed.output);

    let found = f.run("find", json!({"pattern": "*.txt", "path": ".."}));
    assert!(found.is_error, "{}", found.output);
}

#[test]
fn a_dotdot_grep_escape_is_refused() {
    let f = Fixture::new();
    // `escaped.txt` holds "the needle", so a grep that escaped would report it.
    let result = f.run("grep", json!({"pattern": "needle", "path": ".."}));
    assert!(result.is_error, "{}", result.output);
    assert!(!result.output.contains("escaped.txt"), "{}", result.output);
}

#[test]
fn a_symlink_escaping_the_root_is_refused() {
    let f = Fixture::new();
    let link = f.root().join("link");
    if let Err(reason) = link_out_of_tree(&f.base().join("outside"), &link) {
        eprintln!(
            "SKIPPED: this machine cannot create a link out of the tree ({reason}), so the \
             symlink half of the ceiling criterion was NOT exercised here"
        );
        return;
    }

    let read = f.run("read", json!({"file_path": "link/secret.txt"}));
    assert!(
        read.is_error,
        "a symlink escape must be refused: {}",
        read.output
    );
    assert!(
        read.output.contains("outside the workspace root"),
        "the check is on the resolved path, so the refusal says so: {}",
        read.output
    );
    assert!(!read.output.contains("outside secret"), "{}", read.output);

    let ls = f.run("ls", json!({"path": "link"}));
    assert!(
        ls.is_error,
        "the ceiling is per tool, not per tool that reads files"
    );

    // And a recursive search must not walk through it either.
    let grep = f.run("grep", json!({"pattern": "outside secret"}));
    assert!(!grep.is_error, "{}", grep.output);
    assert!(
        grep.output.contains("no line"),
        "recursive walks must skip paths that resolve outside the ceiling: {}",
        grep.output
    );
}

#[test]
fn a_sibling_whose_name_merely_starts_with_the_root_is_refused() {
    let f = Fixture::new();
    // The case a textual prefix check gets wrong in the *other* direction: an absolute path
    // into `…\ws-extra` starts with the characters `…\ws`, so a `starts_with` on the
    // string admits it. Only a component comparison refuses it.
    let twin = f.base().join("ws-extra/twin.txt");
    assert!(
        twin.to_string_lossy()
            .starts_with(f.root().to_string_lossy().as_ref()),
        "the fixture must be the trap it claims: a string prefix check has to admit it"
    );
    let result = f.run("read", json!({"file_path": twin.to_string_lossy()}));
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("outside the workspace root"),
        "{}",
        result.output
    );
    assert!(!result.output.contains("the needle"), "{}", result.output);
}

#[test]
fn a_dotdot_that_stays_inside_the_root_is_allowed() {
    let f = Fixture::new();
    // The other failure mode of a ceiling: refusing a path that never left. `sub/../top.txt`
    // resolves inside the root and must read, or the check is a textual ban on `..` rather
    // than a ceiling on where the path lands.
    let result = f.run("read", json!({"file_path": "sub/../top.txt"}));
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(result.output, "alpha\nbeta\n");
}

#[test]
fn a_grep_over_one_file_reports_only_that_file() {
    let f = Fixture::new();
    let result = f.run("grep", json!({"pattern": "needle", "path": "sub/note.txt"}));
    assert!(!result.is_error, "{}", result.output);
    assert!(
        result.output.contains("sub/note.txt:3:"),
        "{}",
        result.output
    );
    assert!(!result.output.contains("escaped"), "{}", result.output);
}

#[test]
fn a_verbatim_prefixed_path_outside_the_root_is_refused() {
    let f = Fixture::new();
    // `\\?\C:\…` is the spelling `canonicalize` itself returns on Windows. It is the same
    // directory as the plain spelling, so any comparison that is textual rather than by
    // component sees a path outside the root it is actually inside of.
    let outside = f.base().join("escaped.txt");
    let verbatim = format!("\\\\?\\{}", outside.display());
    let result = f.run("read", json!({"file_path": verbatim}));
    assert!(
        result.is_error,
        "a verbatim-prefixed path outside the root must be refused: {}",
        result.output
    );
    assert!(!result.output.contains("the needle"), "{}", result.output);
}

#[test]
fn a_verbatim_prefixed_path_inside_the_root_still_works() {
    let f = Fixture::new();
    // The converse, and the reason the check is by component: the verbatim spelling of a
    // path *inside* the root must be allowed, or the ceiling would be refusing the very
    // form `canonicalize` produces.
    let inside = f.root().join("top.txt");
    let verbatim = format!("\\\\?\\{}", inside.display());
    let result = f.run("read", json!({"file_path": verbatim}));
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(result.output, "alpha\nbeta\n");
}

#[cfg(windows)]
#[test]
fn a_drive_relative_path_is_refused() {
    let f = Fixture::new();
    let drive = f.base().to_string_lossy().chars().next().unwrap();
    let result = f.run("read", json!({"file_path": format!("{drive}:escaped.txt")}));
    assert!(
        result.is_error,
        "a drive-relative path is neither inside nor outside by the text alone, so it must be \
         refused rather than interpreted: {}",
        result.output
    );
    assert!(!result.output.contains("the needle"), "{}", result.output);
}

#[test]
fn a_missing_path_is_refused_with_an_actionable_sentence() {
    let f = Fixture::new();
    let result = f.run("read", json!({"file_path": "nope.txt"}));
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("ls"),
        "the refusal must tell the model what to do instead: {}",
        result.output
    );
}

// ---------------------------------------------------------------------------------------
// Config decides: the empty config, the disabled tier, and the malformed call.
// ---------------------------------------------------------------------------------------

#[test]
fn with_an_empty_config_no_read_tier_tool_runs() {
    // The fixture is built even though nothing may read it: it is what makes the "no
    // fixture content appeared" assertion below a claim about a file that exists.
    let _f = Fixture::new();
    let set = ToolSet::new(Tools {
        workspace: None,
        ..Tools::default()
    });
    let counting = Counting::new(&set);

    for (name, input) in read_tier_calls() {
        let result = counting.run(&call(name, input));
        assert!(result.is_error, "`{name}` must not run with no workspace");
        assert_eq!(
            result.id, "toolu_1",
            "a result must answer the call it was given"
        );
        assert!(
            result.output.contains("tools.workspace"),
            "the refusal must name the key that would enable it: {}",
            result.output
        );
        assert!(
            !result.output.contains("alpha") && !result.output.contains("needle line"),
            "no tool may have run, so no fixture content may appear: {}",
            result.output
        );
    }

    assert_eq!(
        counting.calls(),
        4,
        "every call must be *answered* — a loop that ignored a call leaves the model waiting \
         for a result that never comes"
    );
}

#[test]
fn a_disabled_tier_is_refused_by_naming_its_config_key() {
    let f = Fixture::new();
    let tools = Tools {
        read: false,
        ..f.tools()
    };
    let set = ToolSet::new(tools);

    for (name, input) in read_tier_calls() {
        let result = set.run(&call(name, input));
        assert!(result.is_error, "`{name}` must be refused");
        assert!(
            result.output.contains("tools.read"),
            "the answer must name `tools.read` so the model can proceed instead of retrying: {}",
            result.output
        );
    }
}

#[test]
fn a_call_missing_its_argument_is_refused_naming_the_argument() {
    let f = Fixture::new();
    let result = f.run("read", json!({}));
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("file_path"),
        "the refusal must name the missing argument: {}",
        result.output
    );
}

#[test]
fn a_call_with_a_mistyped_argument_is_refused_naming_the_argument_and_its_type() {
    let f = Fixture::new();
    let result = f.run("read", json!({"file_path": 5}));
    assert!(result.is_error, "{}", result.output);
    assert!(result.output.contains("file_path"), "{}", result.output);
    assert!(
        result.output.contains("a number"),
        "the refusal must say what was sent, not only what was wanted: {}",
        result.output
    );
}

#[test]
fn a_malformed_input_object_is_refused_rather_than_panicking() {
    let f = Fixture::new();
    for input in [json!("top.txt"), json!([1, 2]), json!(null), json!(7)] {
        let result = f.run("read", input.clone());
        assert!(result.is_error, "`{input}` must be a stated refusal");
        assert!(
            result.output.contains("JSON object"),
            "the refusal must say what shape is wanted: {}",
            result.output
        );
    }

    let empty = f.run("read", json!({"file_path": ""}));
    assert!(empty.is_error, "{}", empty.output);
    assert!(empty.output.contains("file_path"), "{}", empty.output);

    let nulled = f.run("grep", json!({"pattern": null}));
    assert!(nulled.is_error, "{}", nulled.output);
    assert!(nulled.output.contains("pattern"), "{}", nulled.output);
}

#[test]
fn an_unknown_name_is_refused_rather_than_panicking() {
    // `ToolSet` never routes this here; the branch exists so that a future router mistake
    // is a sentence rather than a panic in a loop with no user in it.
    let f = Fixture::new();
    let result = ToolSet::new(f.tools()).run(&call("teleport", json!({})));
    assert!(result.is_error);
    assert!(
        result.output.contains("no `teleport` tool"),
        "{}",
        result.output
    );
}

// ---------------------------------------------------------------------------------------
// Bounds, and the promise that nothing is written.
// ---------------------------------------------------------------------------------------

#[test]
fn a_ten_megabyte_file_is_bounded_by_the_output_cap() {
    let f = Fixture::new();
    fs::write(f.root().join("big.txt"), "a".repeat(10 * 1024 * 1024)).unwrap();
    let tools = Tools {
        max_output_bytes: 4096,
        ..f.tools()
    };
    let result = ToolSet::new(tools).run(&call("read", json!({"file_path": "big.txt"})));

    assert!(!result.is_error, "{}", result.output);
    assert!(
        result.output.len() <= 4096,
        "one call may return at most `tools.max_output_bytes`: {}",
        result.output.len()
    );
    assert!(
        result.output.contains("tools.max_output_bytes"),
        "a truncated read must say it was truncated and name the cap: {}",
        &result.output[..result.output.len().min(200)]
    );
}

#[test]
fn a_directory_with_many_entries_is_bounded_by_the_output_cap() {
    let f = Fixture::new();
    let many = f.root().join("many");
    fs::create_dir_all(&many).unwrap();
    for i in 0..2000 {
        fs::write(many.join(format!("f{i:05}.txt")), "").unwrap();
    }
    let tools = Tools {
        max_output_bytes: 512,
        ..f.tools()
    };
    let result = ToolSet::new(tools).run(&call("ls", json!({"path": "many"})));

    assert!(!result.is_error, "{}", result.output);
    assert!(
        result.output.len() <= 512,
        "a directory with 100k entries must be bounded, not accumulated: {}",
        result.output.len()
    );
    assert!(
        result.output.contains("tools.max_output_bytes"),
        "the cap must be visible in the listing: {}",
        result.output
    );
}

#[test]
fn a_grep_that_fills_the_budget_is_bounded_by_the_output_cap() {
    let f = Fixture::new();
    let many = f.root().join("many");
    fs::create_dir_all(&many).unwrap();
    for i in 0..500 {
        fs::write(many.join(format!("f{i:05}.txt")), "needle\n").unwrap();
    }
    fs::write(f.root().join("long.txt"), "needle\n".repeat(200)).unwrap();
    let tools = Tools {
        max_output_bytes: 256,
        ..f.tools()
    };
    let set = ToolSet::new(tools.clone());

    // Across many files, and inside one file: the cross-file cap used to append its note
    // *after* hitting the cap, which made the note the one thing that broke the bound it
    // described.
    let across = set.run(&call("grep", json!({"pattern": "needle", "path": "many"})));
    assert!(!across.is_error, "{}", across.output);
    assert!(
        across.output.len() <= 256,
        "grep across files returned {} bytes",
        across.output.len()
    );
    assert!(
        across.output.contains("tools.max_output_bytes"),
        "{}",
        across.output
    );

    let inside = set.run(&call(
        "grep",
        json!({"pattern": "needle", "path": "long.txt"}),
    ));
    assert!(!inside.is_error, "{}", inside.output);
    assert!(
        inside.output.len() <= 256,
        "grep inside one file returned {} bytes",
        inside.output.len()
    );
}

#[test]
fn a_full_stage_run_leaves_the_tree_byte_identical() {
    let f = Fixture::new();
    let before = tree_snapshot(f.base());

    // Every tool, over the workspace, plus the runs that reach the ceiling.
    for (name, input) in read_tier_calls() {
        f.run(name, input);
    }
    f.run("grep", json!({"pattern": "needle", "path": "sub/note.txt"}));
    f.run("find", json!({"pattern": "**/*"}));
    f.run("read", json!({"file_path": "../escaped.txt"}));
    f.run("ls", json!({"path": "empty"}));

    let after = tree_snapshot(f.base());
    assert_eq!(
        before, after,
        "a read-only tier that wrote anything would fail this, and the tree includes the \
         sibling the refusals must not have touched"
    );
}

#[test]
fn the_tier_names_no_network_credential_or_vendor_store_symbol() {
    // Static, and therefore partial: it proves the source reaches for no socket, no
    // environment, and no store by name. The dynamic half is the byte-compare above, which
    // would catch a read that mutated something.
    let source = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/tools/read_only.rs"
    ))
    .unwrap();
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
            "the read-only tier must not name `{forbidden}`: it reads files inside one root and \
             nothing else"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------------------

/// A content hash over every file under `root`, path and bytes.
///
/// Not a file count and not a size: "byte-compared" in `ACCEPTANCE.md` §19 means a write
/// that preserved both would still be caught.
fn tree_snapshot(root: &Path) -> u64 {
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files.sort();
    let mut hasher = DefaultHasher::new();
    for file in files {
        let rel = file.strip_prefix(root).unwrap_or(&file);
        rel.to_string_lossy().replace('\\', "/").hash(&mut hasher);
        fs::read(&file).unwrap_or_default().hash(&mut hasher);
    }
    hasher.finish()
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Creating a directory link needs a privilege on Windows.
///
/// `symlink_dir` wants developer mode or elevation. A **junction** (`mklink /J`) is the same
/// reparse point from `canonicalize`'s point of view and needs no privilege, so it is the
/// fallback — the alternative is a criterion this machine silently declines to exercise.
///
/// `Err` carries the reason, so a skip says *why* rather than passing quietly.
fn link_out_of_tree(target: &Path, link: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let error = match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => return Ok(()),
            Err(e) => e.to_string(),
        };
        // Captured rather than inherited: `mklink` prints a success line nobody asked for,
        // and a skip needs the reason rather than a line of cmd noise.
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output();
        match created {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(format!(
                "symlink_dir said `{error}`; mklink /J said `{}`",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("symlink_dir said `{error}`; cmd said `{e}`")),
        }
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).map_err(|e| e.to_string())
    }
}
