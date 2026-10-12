//! M8 stage A gate: the drift differ.
//!
//! Contract: `planning/DESIGN.md` §17, criteria: `planning/ACCEPTANCE.md` §16 (row "A
//! drift"). The gate is `cargo test --test drift`.
//!
//! Every input here is a fixture built in this file: no `agy`, no `codex`, no network, no
//! `PIE_LIVE`. A differ test that needs a vendor CLI installed is green on the developer's
//! machine and red on CI — bug B32's shape (`HANDOFF.md` §4) — and the watchtower exists
//! to run unattended, on a machine that has none of them.
//!
//! Two properties are the subject of most of this file, and neither is visible from a
//! single interesting case:
//!
//! * **The empty answer is only correct for an identical pair.** "No findings" is also
//!   what a differ that never reports anything returns, so it is asserted on its own, and
//!   the cases that produce findings are asserted by *count* as well as by content.
//! * **A finding's class is what the job is for.** A digest that only listed findings
//!   would be a changelog, so `compatible` / `pie-must-change` / `pie-must-stop` are each
//!   asserted from a fixture, `unclassified` is asserted never to be what an explained
//!   finding gets, and the `pie-must-stop` wording is scanned rather than trusted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use pie::watch::{classify, diff, CliSnapshot, Drift, DriftClass};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The backend the base fixtures describe.
const BACKEND: &str = "agy";

/// Its recorded version, which also appears inside the base help text — as a real CLI's
/// version does.
const VERSION: &str = "1.3.3";

/// A fixed clock reading. Never changed, except by the test that asserts a newer timestamp
/// is not itself a finding.
const TAKEN_MS: u64 = 1_700_000_000_000;

/// The `--help` text, shaped like the real one: a banner carrying the version, a usage
/// line, flag lines with placeholders, and a closing prose line.
const BASE_HELP: &str = "\
agy 1.3.3 — headless coding agent

Usage: agy [options] <task>

Options:
  -p, --print <PROMPT>          Print one answer to stdout and exit.
  --print-timeout <N>s          Wait up to N seconds for the turn to finish.
  --output-format <FORMAT>      One of: json, stream-json.
  --disable-slash-commands      Do not expand a leading slash command.
  --add-dir <DIR>               Allow the turn to read files under DIR.
  --verbose                     Print more detail about each step.

Report a problem at https://example.invalid/issues.
";

/// Selected binary strings, including one that is exactly the version string and one whose
/// name also exists as a flag — both on purpose, because the differ has to keep those two
/// kinds of evidence apart.
const STRINGS: [&str; 4] = [
    "agy 1.3.3",
    "stream-json",
    "print-timeout",
    "print-timeout-seconds",
];

/// A closing prose line, used as the anchor for changes that add or remove one.
const CLOSING_LINE: &str = "Report a problem at https://example.invalid/issues.";

/// Wording that would turn `pie-must-stop` into advice about how to keep going.
///
/// The scan runs over pie's own sentences plus the evidence the fixtures quote, and the
/// fixtures are written to contain none of these words — so a hit means pie told the user
/// how to keep driving a CLI that has asked it to stop (`PLAN.md` §11a, `RISKS.md` §3).
const WORKAROUND_WORDS: [&str; 12] = [
    "skip",
    "disable",
    "bypass",
    "workaround",
    "work around",
    "circumvent",
    "evade",
    "override",
    "route around",
    "get around",
    "spoof",
    "fake",
];

/// One snapshot, built from text rather than probed from a CLI.
fn snapshot(backend: &str, version: &str, help: &str, strings: &[&str]) -> CliSnapshot {
    CliSnapshot {
        backend: backend.to_string(),
        version: version.to_string(),
        help: help.to_string(),
        strings: strings.iter().map(|s| (*s).to_string()).collect(),
        taken_ms: TAKEN_MS,
    }
}

fn base() -> CliSnapshot {
    snapshot(BACKEND, VERSION, BASE_HELP, &STRINGS)
}

/// The base help text with **one** thing changed.
///
/// Derived by replacement rather than written out per case, so each case states the single
/// change it makes: a case that quietly changed two things would make an assertion about
/// "exactly one finding" pass or fail for a reason nobody can see in the test.
fn help_with(from: &str, to: &str) -> String {
    assert!(
        BASE_HELP.contains(from),
        "the fixture help text no longer contains `{from}`, so this case changes nothing"
    );
    BASE_HELP.replace(from, to)
}

/// The base help text with `addition` inserted just before the closing prose line.
///
/// The closing line is kept, not replaced: a case that replaced it would change two
/// things — the thing under test and the sentence it displaced — so an assertion about
/// "exactly one finding" would be measuring the wrong event.
fn help_plus(addition: &str) -> String {
    help_with(CLOSING_LINE, &format!("{addition}\n\n{CLOSING_LINE}"))
}

/// The pair `base()` versus a help text, version unchanged.
fn with_help(help: &str) -> Vec<Drift> {
    diff(&base(), &snapshot(BACKEND, VERSION, help, &STRINGS))
}

/// The pair `base()` versus a changed version, with the version moved everywhere it
/// appears — the banner and the selected strings — because that is what a real bump does.
fn with_version(version: &str, help: &str) -> Vec<Drift> {
    let bumped: Vec<String> = STRINGS
        .iter()
        .map(|s| s.replace(VERSION, version))
        .collect();
    let strings: Vec<&str> = bumped.iter().map(String::as_str).collect();
    diff(&base(), &snapshot(BACKEND, version, help, &strings))
}

/// The pair `base()` versus different binary strings, help and version unchanged.
fn with_strings(strings: &[&str]) -> Vec<Drift> {
    diff(&base(), &snapshot(BACKEND, VERSION, BASE_HELP, strings))
}

/// Every fixture pair this file reasons about, named for the change it makes.
///
/// One list, read by the cross-cutting assertions at the bottom of this file, so an
/// invariant like "every `pie-must-change` finding names a file" is checked against every
/// case rather than against the one case that happened to be at hand.
fn corpus() -> Vec<(&'static str, Vec<Drift>)> {
    vec![
        ("an identical pair", diff(&base(), &base())),
        (
            "a changed version",
            with_version("1.4.0", &help_with("agy 1.3.3", "agy 1.4.0")),
        ),
        (
            "a renamed flag",
            with_help(&help_with("--print-timeout", "--turn-timeout")),
        ),
        (
            "an added flag",
            with_help(&help_plus(
                "  --no-color                     Print without colour.",
            )),
        ),
        (
            "a removed flag",
            with_help(&help_with(
                "  --verbose                     Print more detail about each step.\n",
                "",
            )),
        ),
        (
            "a changed schema line",
            with_help(&help_with(
                "One of: json, stream-json.",
                "One of: json, stream-json, msgpack.",
            )),
        ),
        (
            "a changed binary string",
            with_strings(&[
                "agy 1.3.3",
                "stream-json",
                "turn-timeout",
                "print-timeout-seconds",
            ]),
        ),
        (
            "a removed help line",
            with_help(&help_with(CLOSING_LINE, "")),
        ),
        (
            // Also the case that proves a lone `-` is not read as a flag.
            "an added help line",
            with_help(&help_plus(
                "Pass - to read the task from stdin instead of argv.",
            )),
        ),
        (
            "a consent requirement in prose",
            with_help(&help_plus(
                "An unattended run requires that you accept the terms of service first.",
            )),
        ),
        (
            "a consent requirement on a new flag",
            with_help(&help_plus(
                "  --accept-terms                Required before an unattended run.",
            )),
        ),
    ]
}

/// Every finding in the corpus, paired with the case that produced it.
fn all_findings() -> Vec<(&'static str, Drift)> {
    corpus()
        .into_iter()
        .flat_map(|(name, drifts)| drifts.into_iter().map(move |entry| (name, entry)))
        .collect()
}

/// The one finding whose `what` contains `needle`.
///
/// Panics with the whole finding list, because the useful question when this fails is what
/// the differ said *instead*.
fn only<'a>(drifts: &'a [Drift], needle: &str) -> &'a Drift {
    let matches: Vec<&Drift> = drifts
        .iter()
        .filter(|entry| entry.what.contains(needle))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one `{needle}` finding, got {drifts:#?}"
    );
    matches[0]
}

/// Assert the differ did *not* say this, which is how an over-reporting change is caught.
fn none(drifts: &[Drift], needle: &str) {
    assert!(
        !drifts.iter().any(|entry| entry.what.contains(needle)),
        "unexpected `{needle}` finding in {drifts:#?}"
    );
}

/// A finding built by hand, as `classify` may be handed one no snapshot produced.
fn drift(what: &str, detail: &str) -> Drift {
    Drift {
        backend: BACKEND.to_string(),
        class: DriftClass::Unclassified,
        what: what.to_string(),
        detail: detail.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The empty answer, asserted where it belongs
// ---------------------------------------------------------------------------

#[test]
fn an_identical_pair_yields_no_findings() {
    // Asserted separately from every interesting case, because "no findings" is also what
    // a differ that never reports anything returns. Two independently built snapshots, not
    // one value compared with itself, so a differ that accidentally compared pointers
    // would fail here rather than pass for the wrong reason.
    assert!(diff(&base(), &base()).is_empty());
}

#[test]
fn a_newer_timestamp_is_not_a_change() {
    // The clock is a parameter, never a finding. A differ that reported `taken_ms` as
    // drift would report every run as a change, and a report in which everything always
    // changed is one no reader can act on.
    let later = CliSnapshot {
        taken_ms: TAKEN_MS + 86_400_000,
        ..base()
    };
    assert!(diff(&base(), &later).is_empty());
}

// ---------------------------------------------------------------------------
// One finding per kind, with the class the contract gives it
// ---------------------------------------------------------------------------

#[test]
fn a_changed_version_is_pie_must_change_and_names_the_backend_module() {
    let drifts = with_version("1.4.0", &help_with("agy 1.3.3", "agy 1.4.0"));

    // Exactly one finding: the version also appears inside the fixture's banner and in the
    // selected strings, and a bump reported both as a version change and as a help-line or
    // string change would double every release in the report.
    assert_eq!(drifts.len(), 1, "{drifts:#?}");

    let entry = only(&drifts, "version changed");
    assert_eq!(entry.class, DriftClass::PieMustChange);
    assert!(entry.detail.contains("1.3.3"), "{:#?}", entry.detail);
    assert!(entry.detail.contains("1.4.0"), "{:#?}", entry.detail);
    assert!(
        entry.detail.contains("src/provider/agy.rs"),
        "{:#?}",
        entry.detail
    );
}

#[test]
fn the_pie_file_a_change_names_follows_the_backend_it_came_from() {
    // A hardcoded `src/provider/agy.rs` would satisfy the agy case and be wrong for every
    // other backend, so the mapping is asserted on two.
    for (backend, file) in [
        ("agy", "src/provider/agy.rs"),
        ("codex", "src/provider/codex.rs"),
    ] {
        let drifts = diff(
            &snapshot(backend, "0.9.0", BASE_HELP, &STRINGS),
            &snapshot(backend, "1.0.0", BASE_HELP, &STRINGS),
        );
        let entry = only(&drifts, "version changed");
        assert!(
            entry.detail.contains(file),
            "{backend}: {:#?}",
            entry.detail
        );
    }
}

#[test]
fn a_renamed_flag_is_one_finding_rather_than_a_removal_and_an_addition() {
    let drifts = with_help(&help_with("--print-timeout", "--turn-timeout"));

    assert_eq!(drifts.len(), 1, "{drifts:#?}");

    let entry = only(&drifts, "flag renamed");
    assert_eq!(entry.class, DriftClass::PieMustChange);
    assert!(
        entry.detail.contains("--print-timeout"),
        "{:#?}",
        entry.detail
    );
    assert!(
        entry.detail.contains("--turn-timeout"),
        "{:#?}",
        entry.detail
    );

    // The same event split into two findings would make the report's own count a poor
    // guide to how much actually moved.
    none(&drifts, "flag removed");
    none(&drifts, "flag added");
}

#[test]
fn an_added_flag_is_compatible() {
    let drifts = with_help(&help_plus(
        "  --no-color                     Print without colour.",
    ));

    assert_eq!(drifts.len(), 1, "{drifts:#?}");

    let entry = only(&drifts, "flag added");
    assert_eq!(entry.class, DriftClass::Compatible);
    assert!(entry.detail.contains("--no-color"), "{:#?}", entry.detail);

    // Also the control for the classes that are *not* this: a differ that answered
    // `pie-must-change` or `pie-must-stop` to everything would satisfy the interesting
    // cases and make the report useless.
    assert_ne!(entry.class, DriftClass::PieMustChange);
    assert_ne!(entry.class, DriftClass::PieMustStop);
}

#[test]
fn a_removed_flag_is_pie_must_change_because_a_snapshot_cannot_know_pie_ignored_it() {
    let drifts = with_help(&help_with(
        "  --verbose                     Print more detail about each step.\n",
        "",
    ));

    assert_eq!(drifts.len(), 1, "{drifts:#?}");

    let entry = only(&drifts, "flag removed");
    assert_eq!(entry.class, DriftClass::PieMustChange);
    assert_ne!(entry.class, DriftClass::Compatible);
    assert!(entry.detail.contains("--verbose"), "{:#?}", entry.detail);
    assert!(entry.detail.contains("(absent)"), "{:#?}", entry.detail);
    assert!(
        entry.detail.contains("src/provider/agy.rs"),
        "{:#?}",
        entry.detail
    );
}

#[test]
fn a_changed_schema_line_is_reported_even_though_no_flag_name_moved() {
    // The "stream field changed shape" half of the contract: the flag is still called
    // `--output-format`, and the set of values pie may pass it is not.
    let drifts = with_help(&help_with(
        "One of: json, stream-json.",
        "One of: json, stream-json, msgpack.",
    ));

    assert_eq!(drifts.len(), 1, "{drifts:#?}");

    let entry = only(&drifts, "flag shape changed");
    assert_eq!(entry.class, DriftClass::PieMustChange);
    assert!(
        entry.detail.contains("--output-format"),
        "{:#?}",
        entry.detail
    );
    assert!(entry.detail.contains("stream-json"), "{:#?}", entry.detail);
    assert!(entry.detail.contains("msgpack"), "{:#?}", entry.detail);
}

#[test]
fn a_binary_string_that_vanishes_and_one_that_appears_are_separate_findings() {
    let drifts = with_strings(&[
        "agy 1.3.3",
        "stream-json",
        "turn-timeout",
        "print-timeout-seconds",
    ]);

    // Two, not three: `agy 1.3.3` is still selected and the version-masked comparison
    // reports nothing for it. The version finding is where a version belongs.
    assert_eq!(drifts.len(), 2, "{drifts:#?}");

    let gone = only(&drifts, "binary string removed");
    assert_eq!(gone.class, DriftClass::PieMustChange);
    assert!(gone.detail.contains("print-timeout"), "{:#?}", gone.detail);
    assert!(gone.detail.contains("(absent)"), "{:#?}", gone.detail);

    let arrived = only(&drifts, "binary string added");
    assert_eq!(arrived.class, DriftClass::Compatible);
    assert!(
        arrived.detail.contains("turn-timeout"),
        "{:#?}",
        arrived.detail
    );
}

#[test]
fn a_removed_prose_line_is_pie_must_change_and_names_the_file_to_follow() {
    // A `--help` line that documented behaviour pie may rely on is gone, and a snapshot
    // cannot say whether anything replaced it.
    let drifts = with_help(&help_with(CLOSING_LINE, ""));

    assert_eq!(drifts.len(), 1, "{drifts:#?}");

    let entry = only(&drifts, "help line removed");
    assert_eq!(entry.class, DriftClass::PieMustChange);
    assert!(entry.detail.contains("(absent)"), "{:#?}", entry.detail);
    assert!(
        entry.detail.contains("src/provider/agy.rs"),
        "{:#?}",
        entry.detail
    );
}

#[test]
fn a_lone_dash_in_help_is_not_read_as_a_new_flag() {
    // `-` and `--` are stdio markers a CLI documents in its help text. A differ that read
    // one as a flag would report the same "new flag" on every run forever, which is how a
    // report trains its reader to skim past it.
    let drifts = with_help(&help_plus(
        "Pass - to read the task from stdin instead of argv.",
    ));

    assert_eq!(drifts.len(), 1, "{drifts:#?}");
    none(&drifts, "flag added");

    let entry = only(&drifts, "help line added");
    assert_eq!(entry.class, DriftClass::Compatible);
}

// ---------------------------------------------------------------------------
// The compliance half: a measure is never reported as "fine"
// ---------------------------------------------------------------------------

#[test]
fn a_consent_requirement_stated_in_prose_is_pie_must_stop() {
    let drifts = with_help(&help_plus(
        "An unattended run requires that you accept the terms of service first.",
    ));

    let entry = only(&drifts, "policy measure added");
    assert_eq!(entry.class, DriftClass::PieMustStop);
    assert_ne!(entry.class, DriftClass::Compatible);

    // The report says so plainly, and it names the phrase it recognised, so the
    // classification can be judged rather than trusted.
    assert!(
        entry.detail.contains("terms of service"),
        "{:#?}",
        entry.detail
    );
    assert!(
        entry.detail.contains("matched policy phrase"),
        "{:#?}",
        entry.detail
    );
    assert!(entry.detail.contains("sanctioned"), "{:#?}", entry.detail);
}

#[test]
fn a_consent_requirement_carried_by_a_new_flag_is_pie_must_stop_not_compatible() {
    // The same measure arriving as a flag rather than as prose: the vehicle is different,
    // the meaning is not — and an implementation that classified on the flag's shape rather
    // than on what the vendor said would report this one as `compatible`.
    let drifts = with_help(&help_plus(
        "  --accept-terms                Required before an unattended run.",
    ));

    let entry = only(&drifts, "policy measure added");
    assert_eq!(entry.class, DriftClass::PieMustStop);
    assert_ne!(entry.class, DriftClass::Compatible);
    assert!(
        entry.detail.contains("--accept-terms"),
        "{:#?}",
        entry.detail
    );
}

#[test]
fn removing_a_consent_requirement_is_not_reported_as_a_new_measure() {
    // The measure scan reads only what a change *brought in*. A differ that scanned both
    // sides would answer "stop driving this CLI" to a vendor relaxing a restriction — the
    // wrong direction, and the one that teaches a reader to ignore `pie-must-stop`.
    let gated = snapshot(
        BACKEND,
        VERSION,
        &help_plus("  --accept-terms                Required before an unattended run."),
        &STRINGS,
    );
    let drifts = diff(&gated, &base());

    assert_eq!(drifts.len(), 1, "{drifts:#?}");

    let entry = only(&drifts, "flag removed");
    assert_eq!(entry.class, DriftClass::PieMustChange);
    assert_ne!(entry.class, DriftClass::PieMustStop);
}

#[test]
fn no_pie_must_stop_finding_offers_a_route_around_the_measure() {
    let stops: Vec<(&str, Drift)> = all_findings()
        .into_iter()
        .filter(|(_, entry)| entry.class == DriftClass::PieMustStop)
        .collect();

    // A vacuous scan is worse than no scan: it looks like enforcement and checks nothing.
    assert!(
        !stops.is_empty(),
        "no fixture produced `pie-must-stop`, so this scan proves nothing"
    );

    for (case, entry) in stops {
        let text = format!("{} {}", entry.what, entry.detail).to_lowercase();
        for word in WORKAROUND_WORDS {
            assert!(
                !text.contains(word),
                "{case}: `{word}` appears in a pie-must-stop finding — {entry:#?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Classifying: the table, and the answer that must never be invented
// ---------------------------------------------------------------------------

#[test]
fn a_finding_nothing_matches_is_unclassified_and_never_compatible() {
    let unknown = drift(
        "vendor added a telemetry notice",
        "previous: none\ncurrent: see the release notes",
    );

    assert_eq!(classify(&unknown), DriftClass::Unclassified);
    assert_ne!(
        classify(&unknown),
        DriftClass::Compatible,
        "`compatible` would turn `we do not know` into `this is fine`"
    );
}

#[test]
fn classification_reads_the_finding_and_not_a_hardcoded_answer() {
    // The control for the test above: without it, `classify` could return `Unclassified`
    // for everything and the unknown case would still pass.
    let produced = with_help(&help_plus(
        "  --no-color                     Print without colour.",
    ));
    let real = only(&produced, "flag added").clone();

    let rebuilt = drift(&real.what, &real.detail);
    assert_eq!(classify(&rebuilt), DriftClass::Compatible);
    assert_eq!(classify(&rebuilt), real.class);
}

#[test]
fn every_drift_class_is_reachable_from_a_fixture() {
    // A class no test can reach is a class nobody has checked — and all four are part of
    // the frozen contract (`DESIGN.md` §17).
    let mut labels: BTreeSet<&'static str> = BTreeSet::new();
    for (_, entry) in all_findings() {
        labels.insert(entry.class.label());
    }

    // The only `unclassified` the differ itself produces: a pair describing two different
    // CLIs — plus the hand-built finding above.
    for entry in diff(
        &snapshot("agy", VERSION, BASE_HELP, &STRINGS),
        &snapshot("codex", VERSION, BASE_HELP, &STRINGS),
    ) {
        labels.insert(entry.class.label());
    }
    labels.insert(classify(&drift("nothing like anything", "")).label());

    assert_eq!(
        labels,
        BTreeSet::from([
            "compatible",
            "pie-must-change",
            "pie-must-stop",
            "unclassified"
        ])
    );
}

#[test]
fn a_pair_from_two_backends_is_not_silently_empty() {
    // Every other path in the differ would compare agy's flags with codex's and answer
    // either nonsense or — the answer that matters, because it reads as an all-clear —
    // "nothing changed". The pair is invalid, and the report has to say so.
    let drifts = diff(
        &snapshot("agy", VERSION, BASE_HELP, &STRINGS),
        &snapshot("codex", VERSION, BASE_HELP, &STRINGS),
    );

    assert_eq!(drifts.len(), 1, "{drifts:#?}");

    let entry = &drifts[0];
    assert_eq!(entry.class, DriftClass::Unclassified);
    assert_ne!(entry.class, DriftClass::Compatible);
    assert!(entry.detail.contains("agy"), "{:#?}", entry.detail);
    assert!(entry.detail.contains("codex"), "{:#?}", entry.detail);
    // Filed against the snapshot that was just taken, so a report grouped by backend shows
    // the trouble under the CLI that was probed.
    assert_eq!(entry.backend, "codex");
}

// ---------------------------------------------------------------------------
// Properties that hold for every finding, not just the interesting one
// ---------------------------------------------------------------------------

#[test]
fn every_detail_carries_both_the_previous_and_the_current_value() {
    // The reader is asked to judge the classification rather than trust it, and a finding
    // whose before-and-after is missing can only be trusted.
    for (case, entry) in all_findings() {
        assert!(entry.detail.contains("previous:"), "{case}: {entry:#?}");
        assert!(entry.detail.contains("current:"), "{case}: {entry:#?}");
    }
}

#[test]
fn every_pie_must_change_finding_names_a_pie_file_to_follow() {
    let changes: Vec<(&str, Drift)> = all_findings()
        .into_iter()
        .filter(|(_, entry)| entry.class == DriftClass::PieMustChange)
        .collect();

    assert!(
        !changes.is_empty(),
        "no fixture produced `pie-must-change`, so this scan proves nothing"
    );

    for (case, entry) in changes {
        assert!(
            entry.detail.contains("src/provider/") && entry.detail.contains(".rs"),
            "{case}: a `pie-must-change` finding must name the file that follows — {entry:#?}"
        );
    }
}

#[test]
fn the_same_pair_always_produces_the_same_findings() {
    // The report is meant to be read as a diff against last month, so the finding order has
    // to come from the fixtures and not from a hash map's iteration order.
    assert_eq!(corpus(), corpus());
}

#[test]
fn a_reviewed_change_is_never_left_unclassified() {
    // The guard on the label/table coupling: a new kind of finding whose label has no row
    // in the class table would surface here, instead of reaching a reader as `we do not
    // know` for the rest of the project's life.
    for (case, entry) in all_findings() {
        assert_ne!(entry.class, DriftClass::Unclassified, "{case}: {entry:#?}");
    }
}

#[test]
fn the_selected_strings_are_a_set_and_not_a_sequence() {
    // A probe that selected the same strings in a different order has found nothing. A
    // differ that compared them positionally would report the binary as changed on every
    // run whose selection order moved, which is noise a reader has to filter by hand.
    assert!(with_strings(&[
        "print-timeout-seconds",
        "print-timeout",
        "stream-json",
        "agy 1.3.3",
    ])
    .is_empty());
}

#[test]
fn every_finding_kind_is_reported_under_its_own_phrase() {
    // The label vocabulary is asserted here on purpose: the labels are what the monthly
    // report's legend prints, and `what` is what `classify` reads. Renaming one is a
    // deliberate act that updates this list in the same change — the failure this guards
    // against is an unlabeled *added* kind, which would classify as `we do not know`.
    let labels: BTreeSet<String> = all_findings()
        .into_iter()
        .map(|(_, entry)| {
            entry
                .what
                .split(':')
                .next()
                .unwrap_or(entry.what.as_str())
                .trim()
                .to_string()
        })
        .collect();

    for expected in [
        "binary string added",
        "binary string removed",
        "flag added",
        "flag removed",
        "flag renamed",
        "flag shape changed",
        "help line added",
        "help line removed",
        "policy measure added",
        "version changed",
    ] {
        assert!(
            labels.contains(expected),
            "no fixture produced a `{expected}` finding; labels seen: {labels:?}"
        );
    }
}
