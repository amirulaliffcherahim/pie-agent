//! Gate G4 for M8 stage C: the monthly watchtower report.
//!
//! `planning/ACCEPTANCE.md` §16 lists stage C's checks; this file is where they are
//! enforced. Everything here is a **pure function over constructed values**: no vendor CLI
//! is spawned, no feed is fetched, no model is consulted, so the gate costs nothing and
//! cannot fail because a blog reordered its index.
//!
//! Three of the assertions below exist because the opposite defect is invisible in a
//! report that only prints findings, and each has a **control** proving the assertion can
//! fail at all:
//!
//! * a first run must not read as "no drift found" — the two strings differ, and a run
//!   with a previous snapshot must print the other one;
//! * an unclassifiable finding must never be downgraded to `compatible`, while a
//!   `compatible` finding still prints that word, so the negative is not vacuous;
//! * an incomplete digest must say so, while a complete one must not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pie::watch::{self, Drift, DriftClass, NewsItem};

fn drift(backend: &str, class: DriftClass, what: &str) -> Drift {
    Drift {
        backend: backend.to_string(),
        class,
        what: what.to_string(),
        detail: format!("before: {backend} --old-flag\n after: {backend} --new-flag"),
    }
}

fn headline(title: &str, source: &str, impact: u32, matched: &[&str]) -> NewsItem {
    NewsItem {
        title: title.to_string(),
        url: format!("https://example.invalid/{title}"),
        source: source.to_string(),
        impact,
        matched: matched.iter().map(|m| m.to_string()).collect(),
    }
}

/// Every class this enum has, so a class added later cannot quietly miss the loop below.
const ALL_CLASSES: [DriftClass; 4] = [
    DriftClass::Compatible,
    DriftClass::PieMustChange,
    DriftClass::PieMustStop,
    DriftClass::Unclassified,
];

/// The month is a parameter, and the file name comes from it — never from the clock.
///
/// One call site, two months, two file names. The past and future months are the real
/// assertion: a clock read anywhere in this path would substitute the current month for
/// them, and a report can only be diffed against last month's if it can be rendered at
/// any date.
#[test]
fn two_months_render_two_different_file_names_from_one_call_site() {
    let november = watch::render_report("2026-11", &[], &[], false);
    let december = watch::render_report("2026-12", &[], &[], false);

    assert!(
        november.contains("watchtower-2026-11.md"),
        "november's report must name its own file:\n{november}"
    );
    assert!(
        december.contains("watchtower-2026-12.md"),
        "december's report must name its own file:\n{december}"
    );
    assert_ne!(
        november, december,
        "two different months must not render byte-identical reports"
    );

    let past = watch::render_report("1999-01", &[], &[], false);
    let future = watch::render_report("2099-12", &[], &[], false);
    assert!(
        past.contains("watchtower-1999-01.md"),
        "a past month needs no current-date plausibility check:\n{past}"
    );
    assert!(
        future.contains("watchtower-2099-12.md"),
        "a future month needs no current-date plausibility check:\n{future}"
    );
}

/// Every finding carries its class, and the class word comes from `DriftClass::label()`.
///
/// The control matters: with no findings supplied, none of the four labels may appear. A
/// report that printed a legend of all classes would make the first loop pass for a reason
/// that has nothing to do with the findings.
#[test]
fn every_class_label_appears_when_a_finding_of_that_class_is_supplied() {
    for class in ALL_CLASSES {
        let rendered = watch::render_report(
            "2026-11",
            &[drift("agy", class, "a flag moved")],
            &[],
            false,
        );
        assert!(
            rendered.contains(class.label()),
            "`{}` must be printed for a finding of that class:\n{rendered}",
            class.label()
        );
    }

    let none = watch::render_report("2026-11", &[], &[], false);
    for class in ALL_CLASSES {
        assert!(
            !none.contains(class.label()),
            "`{}` must not appear when no finding of that class exists:\n{none}",
            class.label()
        );
    }
}

/// A finding nobody has judged is reported as `unclassified`, **never** as `compatible`.
///
/// This is the requirement `DESIGN.md` §17 calls out by name: a silent downgrade to "fine"
/// is what would make the whole job worthless. The `compatible` control below proves the
/// negative assertion is not passing because the word never appears anywhere.
#[test]
fn an_unclassifiable_finding_is_never_reported_as_compatible() {
    let unknown = watch::render_report(
        "2026-11",
        &[drift(
            "agy",
            DriftClass::Unclassified,
            "help output changed shape",
        )],
        &[],
        false,
    );
    assert!(
        unknown.contains(DriftClass::Unclassified.label()),
        "an unknown finding must say so:\n{unknown}"
    );
    assert!(
        !unknown.contains(DriftClass::Compatible.label()),
        "an unknown finding must never be downgraded to compatible:\n{unknown}"
    );

    // Control: a genuinely compatible finding *does* print the word, so the assertion
    // above is about the class rather than about a string the renderer never emits.
    let known = watch::render_report(
        "2026-11",
        &[drift("agy", DriftClass::Compatible, "a flag appeared")],
        &[],
        false,
    );
    assert!(
        known.contains(DriftClass::Compatible.label()),
        "a compatible finding must still print `compatible`:\n{known}"
    );
}

/// "First snapshot taken, nothing to diff" and "no drift found" are **different strings**.
///
/// They are indistinguishable in a report that only prints findings, and only one of them
/// is true — so the requirement is asserted directly, on the strings and on the two
/// rendered reports, in both directions.
#[test]
fn a_first_run_is_not_the_same_string_as_no_drift() {
    assert_ne!(
        watch::FIRST_RUN_NOTE,
        watch::NO_DRIFT_NOTE,
        "a first run and a clean comparison are different claims and need different words"
    );

    let first = watch::render_report("2026-11", &[], &[], true);
    let later = watch::render_report("2026-11", &[], &[], false);

    assert!(
        first.contains(watch::FIRST_RUN_NOTE),
        "the first run must say there was nothing to diff:\n{first}"
    );
    assert!(
        !first.contains(watch::NO_DRIFT_NOTE),
        "a first run must never read as `no drift found` — nothing compared it:\n{first}"
    );
    assert!(
        later.contains(watch::NO_DRIFT_NOTE),
        "a run with a previous snapshot must say it compared:\n{later}"
    );
    assert!(
        !later.contains(watch::FIRST_RUN_NOTE),
        "a run with a previous snapshot must not claim it was a first run:\n{later}"
    );
    assert_ne!(first, later);
}

/// The mode that produced the report is labelled, in every report — including an empty one.
///
/// `RISKS.md` §14: with no disassembler the check degrades to version + `--help` + binary
/// strings. A report that does not name its mode gets read as the structural comparison
/// pie never made.
#[test]
fn the_rea_mode_is_labelled_in_both_a_first_run_and_a_normal_report() {
    let first = watch::render_report("2026-11", &[], &[], true);
    let normal = watch::render_report(
        "2026-11",
        &[drift(
            "codex",
            DriftClass::PieMustChange,
            "a stream field changed shape",
        )],
        &[headline("a headline", "Hacker News", 3, &["codex"])],
        false,
    );

    for rendered in [&first, &normal] {
        assert!(
            rendered.contains("REA-lite"),
            "the shipped mode must be named in the report:\n{rendered}"
        );
        assert!(
            rendered.contains(watch::REA_MODE),
            "the label must come from `watch::REA_MODE`, the one place it is written:\n{rendered}"
        );
        assert!(
            rendered.contains("no structure was decompiled"),
            "the label must state the degradation, not just the abbreviation:\n{rendered}"
        );
    }
}

/// A digest that lost a source says so; a digest that lost nothing does not.
///
/// `DESIGN.md` §17 rule 2 (and `ACCEPTANCE.md` §16): a source that failed is a stated
/// error naming the source, never shorter output that reads as "nothing happened".
#[test]
fn an_incomplete_digest_says_so_and_a_complete_one_does_not() {
    let incomplete = watch::render_report(
        "2026-11",
        &[],
        &[
            headline("a headline", "Hacker News", 3, &["codex"]),
            watch::failed_source("arXiv cs.SE", "connection reset by peer"),
        ],
        false,
    );

    assert!(
        incomplete.contains(watch::DIGEST_INCOMPLETE_NOTE),
        "a failed source must be stated, not swallowed:\n{incomplete}"
    );
    assert!(
        incomplete.contains("arXiv cs.SE") && incomplete.contains("connection reset by peer"),
        "the incomplete line must name the source and the failure:\n{incomplete}"
    );
    // The headline that did arrive is still reported; a failure must not blank the digest.
    assert!(
        incomplete.contains("a headline"),
        "a failed source must not suppress the sources that answered:\n{incomplete}"
    );

    let complete = watch::render_report(
        "2026-11",
        &[],
        &[headline("a headline", "Hacker News", 3, &["codex"])],
        false,
    );
    assert!(
        !complete.contains("INCOMPLETE"),
        "a complete digest must not claim to be incomplete:\n{complete}"
    );
}

/// The same inputs render byte-identical output, so month N can be diffed against N-1.
///
/// A report containing a timestamp, a hash-ordered collection or an iteration over a `Map`
/// would break this, and the break would be invisible until someone tried to diff two
/// months and got noise.
#[test]
fn the_same_inputs_render_byte_identical_output() {
    let drifts = vec![
        drift("agy", DriftClass::Compatible, "a flag appeared"),
        drift("codex", DriftClass::PieMustChange, "--json gained a field"),
        drift(
            "claude",
            DriftClass::PieMustStop,
            "a consent gate was added",
        ),
        drift("newcli", DriftClass::Unclassified, "something moved"),
    ];
    let news = vec![
        headline(
            "codex ships a new flag",
            "Hacker News",
            3,
            &["codex", "cli"],
        ),
        watch::failed_source("arXiv cs.SE", "connection reset by peer"),
    ];

    let once = watch::render_report("2026-11", &drifts, &news, false);
    let twice = watch::render_report("2026-11", &drifts, &news, false);

    assert_eq!(
        once, twice,
        "a monthly report must be diffable against last month's"
    );
    assert!(
        !once.trim().is_empty(),
        "a report with findings is not empty"
    );
}

/// An empty report is still intelligible: not an empty string, not a bare header.
///
/// The month with nothing to say is the common case, and a reader must be able to tell
/// what was checked (and in which mode) from the artefact alone.
#[test]
fn an_empty_report_is_still_intelligible() {
    let empty = watch::render_report("2026-11", &[], &[], false);

    assert!(
        !empty.trim().is_empty(),
        "an empty report must not render as an empty string"
    );
    assert!(
        empty.contains("watchtower-2026-11.md"),
        "the report must still name its file:\n{empty}"
    );
    assert!(
        empty.contains(watch::NO_DRIFT_NOTE),
        "it must say what was compared and found:\n{empty}"
    );
    assert!(
        empty.contains("REA-lite"),
        "it must still name the mode that produced it:\n{empty}"
    );
    assert!(
        empty.contains("no headlines this month"),
        "it must account for the digest, not just the drift half:\n{empty}"
    );
    assert!(
        empty.lines().count() > 4,
        "a bare header is not a report:\n{empty}"
    );
}

/// A `pie-must-change` finding names the pie file that has to follow.
///
/// The class is only useful if it says *where* to go. An unknown backend names no invented
/// path: a confident wrong file is worse than an admitted gap.
#[test]
fn a_pie_must_change_finding_names_the_pie_file_that_must_follow() {
    let rendered = watch::render_report(
        "2026-11",
        &[drift(
            "codex",
            DriftClass::PieMustChange,
            "--json gained a field",
        )],
        &[],
        false,
    );
    assert!(
        rendered.contains("src/provider/codex.rs"),
        "the adapter that must follow the change has to be named:\n{rendered}"
    );
    assert!(
        !rendered.contains("src/provider/agy.rs"),
        "a codex finding must not name another backend's adapter:\n{rendered}"
    );

    let unknown = watch::render_report(
        "2026-11",
        &[drift("brandnew", DriftClass::PieMustChange, "a flag moved")],
        &[],
        false,
    );
    assert!(
        !unknown.contains("src/provider/brandnew.rs"),
        "an unknown backend must not get an invented path:\n{unknown}"
    );
}

/// A `pie-must-stop` finding says driving the CLI is no longer sanctioned, with no route.
///
/// `PLAN.md` §11a: the watchtower flags when pie must change or stop, *never how to evade
/// a safety measure*. So there is no per-finding adapter instruction here — the class is
/// not "change a file and carry on".
#[test]
fn a_pie_must_stop_finding_names_no_route_around_the_policy() {
    let rendered = watch::render_report(
        "2026-11",
        &[drift(
            "claude",
            DriftClass::PieMustStop,
            "an anti-automation gate was added",
        )],
        &[],
        false,
    );

    assert!(
        rendered.contains(watch::PIE_MUST_STOP_NOTE),
        "the stop sentence must be printed, not implied:\n{rendered}"
    );
    assert!(
        rendered.contains("no longer the sanctioned path"),
        "it must say plainly that driving the CLI is over:\n{rendered}"
    );
    assert!(
        rendered.contains("there is no workaround to try"),
        "and it must leave no route around the measure:\n{rendered}"
    );
    assert!(
        !rendered.contains("pie file that must follow"),
        "a stop finding must not be framed as an adapter change to carry on with:\n{rendered}"
    );
}
