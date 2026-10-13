//! The monthly watchtower (`DESIGN.md` §17).
//!
//! Two jobs, one report, and **neither spends model tokens by default** — that single
//! clause from `PLAN.md` §11a decides the shape of this module, and it is the same
//! decision M7 makes:
//!
//! * [`drift`] spawns each CLI for its own `version` and `--help`. Those are the probes
//!   I8 already established and they cost **no** model tokens (a `--help` is not a turn),
//!   which is why drift is safe to run on a schedule at all.
//! * [`news`] fetches configured sources over the network and ranks them by **rule**
//!   against the stack configured in `profile.toml`. No model is consulted. A
//!   model-written digest is an opt-in extra behind the same budget gate M7 uses — one
//!   budget mechanism, two consumers.
//!
//! The drift job's third leg — the "REA-lite" `strings` comparison — reads each vendor's
//! **own** artefact (via [`crate::provider::Provider::artefact`], so codex's strings come
//! from its `codex.js` script rather than from node) and keeps only the runs a CLI change
//! could plausibly move. That extractor is [`strings`], and it is pure: the bounded read is
//! the command's job.
//!
//! ## Compliance framing is a requirement, not a tone choice
//!
//! `PLAN.md` §11a: the watchtower *"flags when a change means pie must change behavior or
//! stop, never how to evade a safety measure"*. [`DriftClass`] is that sentence as a
//! type, and an unclassifiable finding is reported as [`DriftClass::Unclassified`] rather
//! than defaulting to `Compatible` — a silent downgrade to "fine" is what would make the
//! whole job worthless.
//!
//! ## Layout
//!
//! `src/watch.rs` sits beside `src/watch/{drift,news}.rs`, mirroring `provider.rs` beside
//! `provider/*.rs`. `ACCEPTANCE.md` §3 forbids `mod.rs`, and until M6 that rule had no
//! enforcement at all — the M6 plan named `src/tui/mod.rs`, broke it, and the suite
//! stayed green. It has a gate now (G3c).

pub mod drift;
pub mod news;
pub mod strings;

use std::path::{Path, PathBuf};

use crate::paths;

pub use drift::{classify, diff};
pub use news::{rank, NewsItem, NewsSource};

/// A configured source that could **not** be fetched, and the reason it was not.
///
/// `ACCEPTANCE.md` §14's S15 row suggests this channel be typed `&[NewsSource]`. That is
/// the wrong shape: [`NewsSource`] is the *feed configuration* — a name and a URL — and
/// carries no reason, so a signature built on it could say which source failed and never
/// what went wrong. This pair carries both, which is what the report prints and what the
/// failing run's error names.
///
/// It exists so a failure never has to reach the report through the *headline* list. A
/// failure smuggled in as a [`NewsItem`] is a sentinel a caller can forget to strip, and
/// the failure it forgot would render as a headline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedSource {
    /// The source's configured name (`NewsSource::name`), as the report spells it.
    pub source: String,
    /// Why the fetch failed, verbatim from the fetch error.
    pub reason: String,
}

/// What a change means for pie. `PLAN.md` §11a's sentence, as a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftClass {
    /// A flag or field appeared; pie needs no change.
    Compatible,
    /// A flag moved, was renamed, or a stream field changed shape; a named pie file
    /// must follow.
    PieMustChange,
    /// A new policy, consent, or anti-automation measure. Driving the CLI is no longer
    /// the sanctioned path, and the report says so plainly rather than suggesting a way
    /// around it.
    PieMustStop,
    /// The finding could not be classified. **Never** collapsed into `Compatible`: that
    /// would turn "we do not know" into "this is fine" with no way to tell.
    Unclassified,
}

impl DriftClass {
    /// The word the report prints, and the word the tests assert on.
    pub fn label(self) -> &'static str {
        match self {
            DriftClass::Compatible => "compatible",
            DriftClass::PieMustChange => "pie-must-change",
            DriftClass::PieMustStop => "pie-must-stop",
            DriftClass::Unclassified => "unclassified",
        }
    }
}

/// One CLI's observed state at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliSnapshot {
    /// Backend id (`agy`, `codex`, `claude`).
    pub backend: String,
    /// The CLI's own version string.
    pub version: String,
    /// Its `--help` output, verbatim.
    pub help: String,
    /// Selected binary strings. "REA-lite": no disassembler is installed
    /// (`RISKS.md` §14), so this is the degraded form and the report labels it as such.
    ///
    /// Selected, not dumped: only the flag-like tokens and policy-vocabulary runs of
    /// [`crate::provider::Provider::artefact`]'s file, de-duplicated, sorted and capped
    /// ([`strings::interesting_strings`]). [`crate::watch::drift::diff`] reads them as a
    /// set, so the same artefact always yields the same vector and a probe that merely
    /// reordered its own output is not a finding.
    pub strings: Vec<String>,
    /// Why `strings` is not a complete picture of the artefact, when it is not.
    ///
    /// `None` means the whole artefact was read. `Some` states the reason — the artefact
    /// was larger than [`strings::MAX_ARTEFACT_BYTES`], so `strings` was taken from a
    /// **prefix**, or it could not be read at all. Recorded rather than left implicit
    /// because a prefix read that reports nothing is a false "no drift", the same shape as
    /// B35's empty `--help` one leg over.
    ///
    /// [`crate::watch::drift::diff`] deliberately does **not** compare this field: it
    /// describes how the strings were taken, and a finding about pie's own read budget is
    /// not a finding about the CLI.
    pub strings_note: Option<String>,
    /// When it was taken, in epoch milliseconds. A parameter, never a clock read.
    pub taken_ms: u64,
}

/// One thing that changed between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// Which backend changed.
    pub backend: String,
    /// What it means for pie.
    pub class: DriftClass,
    /// A one-line description of the change.
    pub what: String,
    /// The evidence — the before and after, so a reader can judge the classification
    /// rather than trust it.
    pub detail: String,
    /// The pie file that must follow, when a file follows at all.
    ///
    /// Set where the finding is **classified** (the only caller of `drift::classify`,
    /// `drift::finding`), because that is the one moment the backend and the class are
    /// both known. The renderer used to derive it from `backend` through a table of its
    /// own, which put a second copy of the mapping in the layer that draws the report; a
    /// finding rendered from a `Drift` this field says nothing about prints no file line
    /// rather than inventing one.
    ///
    /// `Some` for a `pie-must-change`/`pie-must-stop` finding, `None` for `Compatible`
    /// and `Unclassified`. `drift::pie_file`/`drift::follow` remain the one place the
    /// path is computed.
    pub pie_file: Option<String>,
}

/// Where snapshots are kept: `$PIE_HOME/watch`.
pub fn snapshot_dir(home: &Path) -> PathBuf {
    home.join("watch")
}

/// Where the monthly reports are written: `$PIE_HOME/reports`.
///
/// The roadmap names `~/.pie/reports/watchtower-YYYY-MM.md`, so the directory is
/// `reports`, not `watch` — snapshots and reports are different things with different
/// lifetimes, and one is diffed while the other is read.
pub fn reports_dir() -> PathBuf {
    paths::pie_home().join("reports")
}

/// The mode word the shipped report prints, because a finding line looks the same either
/// way.
///
/// `RISKS.md` §14: no disassembler is installed on this machine, so `drift` compares
/// version, `--help` and selected binary strings and **no structure at all**. Without the
/// label, a "REA-lite" report is read as a structural comparison pie never made.
pub const REA_MODE: &str = "REA-lite";

/// What [`REA_MODE`] did and did not check, printed beside the label.
///
/// A bare label is a word the reader has to look up, and looking it up is how "REA-lite"
/// gets shortened to "REA" in the retelling. Spelled out here, at the top of every report.
///
/// **It says what actually ran.** The first version named "selected binary strings" while
/// nothing produced them, so the header claimed a comparison the body then contradicted —
/// and the header is the part a reader skims. All three legs now run; the half that still
/// does not is the structural one, and it is named as missing with its reason.
pub const REA_MODE_DETAIL: &str =
    "version + --help + selected binary strings, all three diffed; the strings are the \
     flag-like tokens and policy-vocabulary runs of each vendor's own artefact (for codex, \
     its `codex.js` script, never the `node` launcher), read from a bounded prefix when the \
     artefact is larger than the cap and stated as such in the snapshot; no structure was \
     decompiled (no disassembler is installed - `RISKS.md` §14; `rea setup` enables the \
     structural half)";

/// The snapshot line for a run that had **no previous snapshot** to compare against.
///
/// Kept deliberately unequal to [`NO_DRIFT_NOTE`]: "we compared and found nothing" and
/// "we had nothing to compare" are different claims, and a report that prints only
/// findings cannot tell them apart. Only one of the two is true, and reading the wrong
/// one as "all clear" is the failure this line exists to prevent.
pub const FIRST_RUN_NOTE: &str =
    "first snapshot taken, nothing to diff — there is no earlier snapshot to compare against";

/// The snapshot line for a run that **did** compare, and found nothing.
pub const NO_DRIFT_NOTE: &str = "no drift found — every probe matched the previous snapshot";

/// How a report's comparison was made: the label it prints, and what it did not check.
///
/// A parameter of [`render_report`] rather than a constant read inside it, so a run in
/// another mode (the structural half `rea setup` enables, `RISKS.md` §14) can label itself
/// honestly instead of a renderer asserting one mode for every run. The label and its
/// detail travel together because a bare label is a word the reader has to look up, and
/// kept as one value because two adjacent `&str`s are a swap waiting to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportMode<'a> {
    /// The mode word, printed first.
    pub label: &'a str,
    /// What that mode did and did not check, printed beside the label.
    pub detail: &'a str,
}

/// The mode the shipped `pie watch` runs in — the pair above, as the caller passes it.
pub const REA_MODE_LITE: ReportMode<'static> = ReportMode {
    label: REA_MODE,
    detail: REA_MODE_DETAIL,
};

/// The digest line that names the failure, rather than letting silence imply a full scan.
pub const DIGEST_INCOMPLETE_NOTE: &str =
    "digest is INCOMPLETE — the source(s) below failed, so this is not the whole picture";

/// The sentence a `pie-must-stop` finding prints.
///
/// `PLAN.md` §11a: the watchtower flags when pie must change *or stop*, "never how to evade
/// a safety measure". So this line names the one sanctioned response — stop — and offers no
/// alternative argv, no retry and no flag that would work around the measure.
pub const PIE_MUST_STOP_NOTE: &str =
    "driving this CLI is no longer the sanctioned path: pie must stop calling it. pie does \
     not route around a vendor's safety, consent or anti-automation measure (`PLAN.md` §11a), \
     so there is no workaround to try";

/// The report's title line for `month`, and the file name it is written under.
///
/// The file name is built from the **injected** month in one place, because a name derived
/// from the system date can only be tested in the current month — which is exactly where a
/// date bug hides.
fn file_name(month: &str) -> String {
    format!("watchtower-{month}.md")
}

/// Record a **failed source**, as its own value rather than as a headline.
///
/// The `watch` command turns each `fetch` error into one of these and passes the list to
/// [`render_report`] beside the headlines. Two types hold one concept each, so a caller
/// that forgets about the failure list loses a stated failure — it does not render a
/// failure as a headline, which is what the old sentinel made possible.
pub fn failed_source(source: &str, error: &str) -> FailedSource {
    FailedSource {
        source: source.to_string(),
        reason: error.to_string(),
    }
}

/// Print `label: <first line>` and indent any continuation lines beneath it.
///
/// `Drift.detail` is the before-and-after evidence and is multi-line by nature. Joined
/// into one line it would splice unrelated output together; dropped to column 0 it would be
/// indistinguishable from the next finding's first line.
fn push_labelled(out: &mut String, label: &str, body: &str) {
    let mut lines = body.lines().map(str::trim_end).filter(|l| !l.is_empty());
    let Some(first) = lines.next() else {
        return;
    };
    out.push_str(&format!("  {label}: {first}\n"));
    for line in lines {
        out.push_str(&format!("    {line}\n"));
    }
}

/// Render the monthly report.
///
/// `month` is a **parameter**, never read from the clock: a file name that depends on the
/// system date can only be tested in the current month, which is where a date bug lives.
///
/// `failed` is the fetch failures, as their own list: a source that failed is not a
/// headline, and giving it a channel of its own is what makes that structural rather than
/// a convention the renderer has to remember to apply.
///
/// `mode` is the comparison the run actually made, passed rather than read from
/// [`REA_MODE`]/[`REA_MODE_DETAIL`] here — the report does not get to decide what produced
/// it.
///
/// `first_run` exists because a drift check with no previous snapshot must report "first
/// snapshot taken, nothing to diff" and **never** "no drift found". Those two are
/// indistinguishable in a report that only prints findings, and only one of them is true.
pub fn render_report(
    month: &str,
    drifts: &[Drift],
    news: &[NewsItem],
    failed: &[FailedSource],
    mode: ReportMode<'_>,
    first_run: bool,
) -> String {
    let mut out = String::new();

    out.push_str(&format!("# pie watchtower — {month}\n"));
    out.push_str(&format!(
        "report: {} (written under `$PIE_HOME/reports/`)\n",
        file_name(month)
    ));
    // The mode sits above the findings, not in a footnote: a reader must know what the
    // comparison covered before trusting a line that says "no drift found".
    out.push_str(&format!("rea mode: {} — {}\n", mode.label, mode.detail));

    if first_run {
        out.push_str(&format!("snapshot: {FIRST_RUN_NOTE}\n"));
    } else if drifts.is_empty() {
        out.push_str(&format!("snapshot: {NO_DRIFT_NOTE}\n"));
    } else {
        out.push_str(&format!(
            "snapshot: compared against the previous snapshot — {} finding(s)\n",
            drifts.len()
        ));
    }

    out.push_str("\n## drift\n");
    if drifts.is_empty() {
        if first_run {
            // The one place the distinction is worth repeating: with no previous snapshot
            // an empty drift section is the *absence of a comparison*, and a reader who
            // skims straight to "no findings" would take it as a clean bill of health.
            out.push_str(
                "  no findings — there was no previous snapshot to compare against, so this is \
                 not a clean bill of health\n",
            );
        } else {
            out.push_str("  no findings\n");
        }
    }
    for d in drifts {
        // `label()` is the one place the word is written, so the report and the tests can
        // never disagree about what a class is called.
        out.push_str(&format!(
            "- [{}] {}: {}\n",
            d.class.label(),
            d.backend,
            d.what
        ));
        push_labelled(&mut out, "evidence", &d.detail);
        match d.class {
            DriftClass::Compatible => {}
            DriftClass::PieMustChange => {
                // The file comes from the finding and nowhere else. Deriving it here from
                // `backend` is what let the renderer name a path for a `Drift` that never
                // carried one; a finding with `pie_file: None` prints no line at all.
                if let Some(file) = &d.pie_file {
                    out.push_str(&format!("  pie file that must follow: {file}\n"));
                }
            }
            DriftClass::PieMustStop => {
                out.push_str(&format!("  {PIE_MUST_STOP_NOTE}\n"));
                if let Some(file) = &d.pie_file {
                    out.push_str(&format!("  affected pie path: {file}\n"));
                }
            }
            // Never silently "fine": the only honest thing to say about a finding nobody
            // has judged is that nobody has judged it.
            DriftClass::Unclassified => {
                out.push_str(
                    "  not classified — this is NOT an all-clear: nobody has judged it yet, \
                     so look before treating it as fine\n",
                );
            }
        }
    }

    out.push_str("\n## news\n");
    if !failed.is_empty() {
        out.push_str(&format!("{DIGEST_INCOMPLETE_NOTE}\n"));
        for entry in failed {
            out.push_str(&format!("  - {}: {}\n", entry.source, entry.reason));
        }
    }
    if news.is_empty() {
        out.push_str("  no headlines this month\n");
    } else {
        for item in news {
            out.push_str(&format!(
                "- [impact {}] {} — {}\n",
                item.impact, item.title, item.source
            ));
            if !item.url.is_empty() {
                out.push_str(&format!("  {}\n", item.url));
            }
            if !item.matched.is_empty() {
                // The matched terms are printed so the score can be judged, not trusted.
                out.push_str(&format!("  matched: {}\n", item.matched.join(", ")));
            }
        }
    }

    out
}
