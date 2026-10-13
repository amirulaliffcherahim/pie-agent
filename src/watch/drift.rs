//! CLI drift detection (`DESIGN.md` §17).
//!
//! Compares two [`CliSnapshot`]s and says what changed **and what it means for pie**.
//! The classification is the point: a digest that only listed findings would be a
//! changelog, and the reason this job exists is to say when pie must change, or must
//! stop.
//!
//! Both functions here are **pure**. Probing a real CLI is a separate concern and lives
//! with the command; a `--help` costs no model tokens, but it does spawn, and a spawn in
//! a pure function is a spawn no test can count.
//!
//! ## The finding's label *is* its category
//!
//! `classify` is frozen as `classify(&Drift) -> DriftClass`, so the classifier is given
//! nothing but the finding's own text. [`diff`] therefore builds `what` from the
//! vocabulary below, and the classifier reads that vocabulary back — the table is the
//! single place a class is decided, and every finding gets its class by being passed
//! through it (`finding`), never by writing a class at the site that detected the change.
//! A site that wrote its own class could disagree with the legend in the report, and a
//! legend that contradicts its own rows is a report a reader stops believing.

use std::collections::BTreeSet;

use super::{CliSnapshot, Drift, DriftClass};
use crate::error::truncate;

// ---------------------------------------------------------------------------
// The finding vocabulary, and the classification table
// ---------------------------------------------------------------------------

/// The phrase every finding of this kind starts with. See the module docs.
const VERSION_CHANGED: &str = "version changed";
/// A flag appeared in `--help` that was not there before.
const FLAG_ADDED: &str = "flag added";
/// A flag that was there before is gone.
const FLAG_REMOVED: &str = "flag removed";
/// A removed flag and an added flag that name the same thing.
const FLAG_RENAMED: &str = "flag renamed";
/// A flag did not move, but the `--help` line describing it changed.
const FLAG_SHAPE_CHANGED: &str = "flag shape changed";
/// A selected binary string appeared.
const STRING_ADDED: &str = "binary string added";
/// A selected binary string is gone.
const STRING_REMOVED: &str = "binary string removed";
/// A `--help` prose line appeared.
const HELP_LINE_ADDED: &str = "help line added";
/// A `--help` prose line is gone.
const HELP_LINE_REMOVED: &str = "help line removed";
/// The change brought in a safety, consent or anti-automation measure.
const POLICY_MEASURE: &str = "policy measure added";
/// The pair handed to [`diff`] describes two different CLIs, so it cannot be compared.
const SNAPSHOT_MISMATCH: &str = "snapshot pair cannot be compared";

/// The whole classification, as a table — one row per kind of finding.
///
/// A table rather than a chain of `if`s, because `ACCEPTANCE.md` §16 requires *every*
/// class, `pie-must-stop` included, to be reachable from a fixture without a real CLI
/// moving underneath us, and because a reader has to be able to check the whole rule set
/// at a glance. A finding whose `what` matches no row is [`DriftClass::Unclassified`] —
/// **never** `Compatible`, which would turn "we do not know" into "this is fine"
/// (`DESIGN.md` §17, `PLAN.md` §11a).
///
/// The safe direction for a doubt is stated once, here, rather than argued per row: this
/// job runs monthly and unattended, so a class that over-escalates costs a human a minute
/// of reading, while a class that under-escalates tells the user that driving a CLI is
/// still sanctioned when it no longer is (`RISKS.md` §3).
const CLASSES: &[(&str, DriftClass)] = &[
    // A moved version invalidates the evidence pie's parsers were validated against —
    // which is exactly the failure `RISKS.md` §3 names ("CLI auto-update breaks
    // parsers"), and `ACCEPTANCE.md` §2 I8 records the version for this purpose.
    (VERSION_CHANGED, DriftClass::PieMustChange),
    // A flag appeared: pie needs no change (`DESIGN.md` §17).
    (FLAG_ADDED, DriftClass::Compatible),
    // Which flags pie passes is not knowable from a snapshot of a CLI's help text, so a
    // vanished flag cannot be assumed harmless: the file that builds argv is named and a
    // human decides.
    (FLAG_REMOVED, DriftClass::PieMustChange),
    (FLAG_RENAMED, DriftClass::PieMustChange),
    // Accepted values, a placeholder, or the description changed. This is the "stream
    // field changed shape" half of the contract.
    (FLAG_SHAPE_CHANGED, DriftClass::PieMustChange),
    // Something new is visible in the binary. On its own that says nothing about pie —
    // `RISKS.md` §14 is why this signal is as coarse as it is, and coarseness belongs in
    // the report's REA-mode label, not in a class.
    (STRING_ADDED, DriftClass::Compatible),
    // Evidence the last snapshot recorded has vanished. Without a disassembler we cannot
    // say why, so this is not reported as harmless.
    (STRING_REMOVED, DriftClass::PieMustChange),
    (HELP_LINE_ADDED, DriftClass::Compatible),
    (HELP_LINE_REMOVED, DriftClass::PieMustChange),
    // `PLAN.md` §11a: a policy, consent or anti-automation measure means driving the CLI
    // is no longer the sanctioned path, and the report says so plainly.
    (POLICY_MEASURE, DriftClass::PieMustStop),
    // The comparison itself was invalid. "We do not know" is the only honest answer.
    (SNAPSHOT_MISMATCH, DriftClass::Unclassified),
];

/// The wording a `pie-must-stop` finding carries, and nothing more.
///
/// Fixed text on purpose: the sentence must state that driving the CLI is no longer the
/// sanctioned path, and it must not hint at a way to keep doing it. A version of this
/// sentence that suggested one would be the single most damaging line pie could print,
/// so it is written once, in a place a test can scan (`tests/drift.rs` asserts no
/// workaround word appears in any `pie-must-stop` finding — the requirement is enforced
/// rather than remembered).
const POLICY_STATEMENT: &str = "driving this CLI headlessly is no longer the sanctioned path; \
     pie must stop driving it until a human has reviewed the change, because unattended use is \
     what this CLI is now guarding against";

/// Words that mark a change as a **measure against unattended use** rather than a feature.
///
/// Read only against the evidence the change *brought in*, never against the evidence it
/// removed: removing a consent requirement is not a reason to stop driving a CLI, and
/// scanning both sides would say it was.
///
/// The list is deliberately biased toward catching a measure. Escalating an ordinary
/// feature to `pie-must-stop` costs a human a minute of reading; missing a consent gate
/// costs them the whole point of the report, and possibly the account
/// (`knowledge/9router-findings.md` §0).
const POLICY_SIGNALS: &[&str] = &[
    "accept the terms",
    "accept-terms",
    "accept_terms",
    "terms of service",
    "terms-of-service",
    "consent",
    "attest",
    "acknowledge",
    "captcha",
    "human verification",
    "verify you are human",
    "verification challenge",
    "anti-automation",
    "anti automation",
    "automation detected",
    "automated access",
    "unattended",
    "not permitted",
    "prohibited",
    "forbidden",
    "interactive terminal",
    "run interactively",
    "interactive login",
    "rate-limited to protect",
    "abuse",
    "banned",
    "suspended",
    "risk_notice",
    "risk notice",
    "policy",
];

/// Two flag names count as a rename at or above this similarity.
///
/// Longest-common-subsequence based, so `--print-timeout` → `--turn-timeout` (0.83) is a
/// rename while `--verbose` → `--different-flag` (0.0) is a removal and an addition. A
/// wrong pairing can only *escalate* — a rename is `pie-must-change` where an addition is
/// `compatible` — never downgrade, which is the direction `RISKS.md` §3 asks for.
const RENAME_MIN_SIMILARITY: f64 = 0.5;

/// The shortest `--help` line that can be prose worth diffing.
///
/// Banner fragments (`Usage:`, a rule of dashes, a stray column header) change constant
/// and carry no contract; reporting them as findings is how a report becomes noise.
const PROSE_MIN_LEN: usize = 8;

/// What a snapshot's own version string is replaced by before anything is compared.
///
/// A CLI usually prints its version inside its own `--help`, so a version bump would
/// otherwise be reported **twice**: once as the truth, and once as a line change that
/// drags a second `pie-must-change` row into the report for the same event. Masking the
/// snapshot's own version removes the duplicate and nothing else.
const VERSION_PLACEHOLDER: &str = "<version>";

/// Shown when the previous help line is the flag name itself because the flag was
/// documented inline rather than on a line of its own.
const NO_LINE: &str = "(no help line)";

/// Shown as the current value of a flag that is no longer documented.
const ABSENT: &str = "(absent)";

// ---------------------------------------------------------------------------
// The differ
// ---------------------------------------------------------------------------

/// What changed between two snapshots, and what it means.
///
/// An identical pair yields **nothing** — asserted, because "no findings" is the answer
/// that a broken differ also gives, and a test that only checks the interesting case
/// cannot tell the two apart.
pub fn diff(previous: &CliSnapshot, current: &CliSnapshot) -> Vec<Drift> {
    // Two different CLIs are not comparable. Comparing them anyway would report either
    // nonsense or — worse, because it reads as an all-clear — "nothing changed", which
    // is the one answer this function must never invent.
    if previous.backend != current.backend {
        return vec![finding(
            &current.backend,
            format!(
                "{SNAPSHOT_MISMATCH}: {} vs {}",
                previous.backend, current.backend
            ),
            format!(
                "{}\nno comparison was made; re-take both snapshots for one backend",
                evidence(&previous.backend, &current.backend)
            ),
        )];
    }

    let backend = current.backend.as_str();
    let previous_help = mask_version(&previous.help, &previous.version);
    let current_help = mask_version(&current.help, &current.version);
    let mut drifts: Vec<Drift> = Vec::new();

    if previous.version != current.version {
        drifts.push(finding(
            backend,
            format!(
                "{VERSION_CHANGED}: {} -> {}",
                previous.version, current.version
            ),
            follow(&evidence(&previous.version, &current.version), backend),
        ));
    }

    collect_flag_changes(backend, &previous_help, &current_help, &mut drifts);
    collect_string_changes(backend, previous, current, &mut drifts);
    collect_help_line_changes(backend, &previous_help, &current_help, &mut drifts);

    drifts
}

/// Classify one finding. **Table-driven and pure**, so every class is reachable in a test
/// without a real CLI changing underneath us.
///
/// A finding that matches nothing is [`DriftClass::Unclassified`], never `Compatible`.
pub fn classify(entry: &Drift) -> DriftClass {
    let what = entry.what.to_ascii_lowercase();
    CLASSES
        .iter()
        .find(|(label, _)| what.starts_with(*label))
        .map(|(_, class)| *class)
        // Not `Compatible`: an unmatched finding is one nobody has explained yet, and
        // calling it fine is precisely the silent downgrade `PLAN.md` §11a forbids.
        .unwrap_or(DriftClass::Unclassified)
}

/// Flags that moved, went, or arrived — plus the flags whose own line moved underneath
/// them, which is the "schema changed" case.
fn collect_flag_changes(
    backend: &str,
    previous_help: &str,
    current_help: &str,
    drifts: &mut Vec<Drift>,
) {
    let before = flags(previous_help);
    let after = flags(current_help);

    let removed: Vec<String> = before.difference(&after).cloned().collect();
    let added: Vec<String> = after.difference(&before).cloned().collect();

    // A rename is one event, so it is one finding: a removal and an addition of the same
    // thing reads as two changes, and a report that overstates its own findings is one
    // nobody trusts (`HANDOFF.md` §9). Pairing runs over both sets in sorted order, so the
    // result does not depend on the order the CLI happened to print its help in.
    let mut pool: Vec<&String> = added.iter().collect();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for old in &removed {
        let mut best: Option<(usize, f64)> = None;
        for (index, candidate) in pool.iter().enumerate() {
            let score = similarity(old, candidate);
            let better = match best {
                None => true,
                Some((_, seen)) => score > seen,
            };
            if better {
                best = Some((index, score));
            }
        }
        // The score is only the pairing decision; the finding reports the two names.
        if let Some((index, _)) = best.filter(|(_, score)| *score >= RENAME_MIN_SIMILARITY) {
            let new = pool.remove(index).clone();
            pairs.push((old.clone(), new.clone()));
        }
    }

    for (old, new) in &pairs {
        let line_before = flag_line(previous_help, old).unwrap_or(old.as_str());
        let line_after = flag_line(current_help, new).unwrap_or(new.as_str());
        let arrived = format!("{new} — {line_after}");
        match policy_signal(&arrived) {
            Some(phrase) => drifts.push(policy_finding(
                backend,
                format!("{FLAG_RENAMED}: {old} -> {new}"),
                line_before,
                line_after,
                phrase,
            )),
            None => drifts.push(finding(
                backend,
                format!("{FLAG_RENAMED}: {old} -> {new}"),
                follow(&evidence(line_before, line_after), backend),
            )),
        }
    }

    let paired: BTreeSet<&String> = pairs.iter().map(|(_, new)| new).collect();
    for old in removed
        .iter()
        .filter(|old| !pairs.iter().any(|(from, _)| from == *old))
    {
        let line = flag_line(previous_help, old).unwrap_or(old.as_str());
        drifts.push(finding(
            backend,
            format!("{FLAG_REMOVED}: {old}"),
            follow(&evidence(line, ABSENT), backend),
        ));
    }
    for new in added.iter().filter(|new| !paired.contains(new)) {
        let line = flag_line(current_help, new).unwrap_or(new.as_str());
        // The policy signal is read off the whole line, not just the flag name: a consent
        // requirement that arrives as one sentence next to a new flag is the same event and
        // must not be reported as an ordinary addition.
        let arrived = format!("{new} — {line}");
        match policy_signal(&arrived) {
            Some(phrase) => drifts.push(policy_finding(
                backend,
                format!("{FLAG_ADDED}: {new}"),
                ABSENT,
                line,
                phrase,
            )),
            None => drifts.push(finding(
                backend,
                format!("{FLAG_ADDED}: {new}"),
                evidence(ABSENT, line),
            )),
        }
    }

    for flag in before.intersection(&after) {
        let line_before = flag_line(previous_help, flag).unwrap_or(NO_LINE);
        let line_after = flag_line(current_help, flag).unwrap_or(NO_LINE);
        if line_before == line_after {
            continue;
        }
        match policy_signal(line_after) {
            Some(phrase) => drifts.push(policy_finding(
                backend,
                format!("{FLAG_SHAPE_CHANGED}: {flag}"),
                line_before,
                line_after,
                phrase,
            )),
            None => drifts.push(finding(
                backend,
                format!("{FLAG_SHAPE_CHANGED}: {flag}"),
                follow(&evidence(line_before, line_after), backend),
            )),
        }
    }
}

/// The selected binary strings that came and went. `RISKS.md` §14: this is the degraded
/// "REA-lite" signal, because no disassembler is installed, so it is set membership on
/// strings pie chose to select — not a structural comparison, and the report says which
/// mode produced it.
fn collect_string_changes(
    backend: &str,
    previous: &CliSnapshot,
    current: &CliSnapshot,
    drifts: &mut Vec<Drift>,
) {
    let before = masked_strings(previous);
    let after = masked_strings(current);

    for gone in before.difference(&after) {
        drifts.push(finding(
            backend,
            format!("{STRING_REMOVED}: {}", truncate(gone, 80)),
            // A vanished string is `pie-must-change`, so it names the file that follows
            // — the same rule as every other finding in that class.
            follow(&evidence(gone, ABSENT), backend),
        ));
    }
    for arrived in after.difference(&before) {
        match policy_signal(arrived) {
            Some(phrase) => drifts.push(policy_finding(
                backend,
                format!("{STRING_ADDED}: {}", truncate(arrived, 80)),
                ABSENT,
                arrived,
                phrase,
            )),
            None => drifts.push(finding(
                backend,
                format!("{STRING_ADDED}: {}", truncate(arrived, 80)),
                evidence(ABSENT, arrived),
            )),
        }
    }
}

/// Prose `--help` lines that came and went.
///
/// Lines naming a flag are skipped and covered by the flag findings instead: counting one
/// change twice would make the report's own count a poor guide to how much moved. What is
/// left is prose — and prose is where a vendor states a policy, which is why the added side
/// is scanned for one.
fn collect_help_line_changes(
    backend: &str,
    previous_help: &str,
    current_help: &str,
    drifts: &mut Vec<Drift>,
) {
    let before = prose_lines(previous_help);
    let after = prose_lines(current_help);

    for gone in before.difference(&after) {
        drifts.push(finding(
            backend,
            format!("{HELP_LINE_REMOVED}: {}", truncate(gone, 80)),
            follow(&evidence(gone, ABSENT), backend),
        ));
    }
    for arrived in after.difference(&before) {
        match policy_signal(arrived) {
            Some(phrase) => drifts.push(policy_finding(
                backend,
                format!("{HELP_LINE_ADDED}: {}", truncate(arrived, 80)),
                ABSENT,
                arrived,
                phrase,
            )),
            None => drifts.push(finding(
                backend,
                format!("{HELP_LINE_ADDED}: {}", truncate(arrived, 80)),
                evidence(ABSENT, arrived),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Building a finding
// ---------------------------------------------------------------------------

/// Assemble one finding and let the table decide what it means.
///
/// The class comes from [`classify`] rather than from the site that detected the change,
/// so there is exactly one place where a class is decided (see the module docs).
///
/// The `pie_file` field is filled here, in the same breath as the class, because this is
/// the only place where a finding has both a backend and a class. Both come from the
/// verdict: a finding that has to be acted on names the file to act on, and one that is
/// compatible or unjudged names nothing rather than a path it has no reason to expect.
fn finding(backend: &str, what: String, detail: String) -> Drift {
    let mut entry = Drift {
        backend: backend.to_string(),
        class: DriftClass::Unclassified,
        what,
        detail,
        pie_file: None,
    };
    let class = classify(&entry);
    entry.class = class;
    entry.pie_file = match class {
        DriftClass::PieMustChange | DriftClass::PieMustStop => Some(pie_file(backend)),
        DriftClass::Compatible | DriftClass::Unclassified => None,
    };
    entry
}

/// A finding whose evidence brought in a policy, consent or anti-automation measure.
///
/// The label is [`POLICY_MEASURE`] rather than the mechanical kind, because the class this
/// produces (`pie-must-stop`) overrides the mechanical kind's class: the vendor's measure
/// is the finding, and which flag carried it is only the vehicle.
fn policy_finding(
    backend: &str,
    kind: String,
    previous: &str,
    current: &str,
    phrase: &'static str,
) -> Drift {
    finding(
        backend,
        format!("{POLICY_MEASURE}: {kind}"),
        format!(
            "{}\nmatched policy phrase: {phrase}\n{}\n{POLICY_STATEMENT}",
            evidence(previous, current),
            follow("", backend)
        ),
    )
}

/// The evidence block every finding carries.
///
/// Both values, under the same two labels, always. The reader is asked to judge the
/// classification rather than trust it, and a finding whose before-and-after is missing
/// can only be trusted — never judged. `tests/drift.rs` asserts both labels on every
/// finding its fixtures produce.
fn evidence(previous: &str, current: &str) -> String {
    format!("previous: {previous}\ncurrent: {current}")
}

/// Add the "pie file to follow" line a `pie-must-change` finding needs.
///
/// "pie must change" without saying *where* is a verdict the reader cannot act on, so the
/// file is named in the detail (`ACCEPTANCE.md` §16). `evidence` is empty for a finding
/// that only needs the file line.
fn follow(evidence: &str, backend: &str) -> String {
    let file = pie_file(backend);
    if evidence.is_empty() {
        format!("pie file to follow: {file}")
    } else {
        format!("{evidence}\npie file to follow: {file}")
    }
}

/// The pie file that must follow a change to this backend.
///
/// `DESIGN.md` §2 locks the layout: one module per backend under `src/provider/`, beside
/// the shared trait in `src/provider.rs`. Deriving the path from the backend id is what a
/// pure function can do — reading the tree to check it would be a side effect in a differ,
/// which is the one thing the module docs say these two functions do not have.
fn pie_file(backend: &str) -> String {
    let slug: String = backend
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase();
    if slug.is_empty() {
        "src/provider.rs".to_string()
    } else {
        format!("src/provider/{slug}.rs")
    }
}

// ---------------------------------------------------------------------------
// Reading a snapshot
// ---------------------------------------------------------------------------

/// Replace a snapshot's own version string with [`VERSION_PLACEHOLDER`].
///
/// Applied to `help` and to every binary string before anything is compared, so a version
/// bump is reported once, by [`VERSION_CHANGED`], instead of once by that finding and again
/// as a help-line or string change that is really the same event.
fn mask_version(text: &str, version: &str) -> String {
    if version.trim().is_empty() {
        return text.to_string();
    }
    text.replace(version, VERSION_PLACEHOLDER)
}

/// A snapshot's binary strings, version-masked and de-duplicated.
fn masked_strings(snapshot: &CliSnapshot) -> BTreeSet<String> {
    snapshot
        .strings
        .iter()
        .map(|s| mask_version(s, &snapshot.version))
        .collect()
}

/// The flag-like tokens in a `--help` text.
///
/// Tolerant by rule (`RISKS.md` §3): help output is prose, not a schema, so anything this
/// does not recognise is simply not a flag here, and a flag it does recognise is reported
/// by name. Getting this wrong costs a missed finding, never a panicked differ.
fn flags(help: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for token in help.split_whitespace() {
        // Trailing punctuation is the CLI's, not the flag's: `--print,` is `--print`.
        let token =
            token.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        // `--flag=value`, `--flag=<value>` and `--flag <value>` all name `--flag`.
        let name = token.split(['=', '[']).next().unwrap_or(token);
        if is_flag(name) {
            found.insert(name.to_string());
        }
    }
    found
}

/// `--print` and `-p` are flags; `-`, `--` and `<PROMPT>` are not.
///
/// The `-p` case is why a lone dash is excluded rather than treated as a one-letter flag:
/// `-` and `--` are stdio markers that appear in help text, and a differ that reported
/// `--` appearing as a new flag would report the same thing on every run.
fn is_flag(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('-') else {
        return false;
    };
    match rest.strip_prefix('-') {
        // Long form: at least two characters, so `--` is not a flag, and at least one
        // letter, so a numeric value is not one either.
        Some(long) => {
            long.len() >= 2
                && long.chars().any(|c| c.is_ascii_alphabetic())
                && long
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        }
        // Short form: exactly one letter.
        None => rest.len() == 1 && rest.chars().all(|c| c.is_ascii_alphabetic()),
    }
}

/// The help line a flag is documented on, if any.
///
/// Built on [`flags`] rather than on a second parser, so a flag that `diff` saw is a flag
/// whose line this can find: two parsers would eventually disagree, and the disagreement
/// would show up as a finding that names a flag and no evidence.
fn flag_line<'a>(help: &'a str, flag: &str) -> Option<&'a str> {
    help.lines().find(|line| flags(line).contains(flag))
}

/// `--help` lines that carry prose rather than flags.
///
/// Trimmed, long enough to say something, and naming no flag. Lines naming a flag are
/// covered by the flag findings, and counting them again as prose would report one change
/// as two.
fn prose_lines(help: &str) -> BTreeSet<String> {
    help.lines()
        .map(str::trim)
        .filter(|line| {
            line.len() >= PROSE_MIN_LEN
                && line.chars().any(|c| c.is_ascii_alphabetic())
                && flags(line).is_empty()
        })
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Recognising a measure
// ---------------------------------------------------------------------------

/// Which policy phrase the evidence the change *brought in* matched, if any.
///
/// Returned rather than a `bool` so the finding can print the phrase it matched: a
/// classification the reader cannot check is one they have to take on faith.
fn policy_signal(evidence: &str) -> Option<&'static str> {
    let lowered = evidence.to_ascii_lowercase();
    POLICY_SIGNALS
        .iter()
        .copied()
        .find(|needle| lowered.contains(needle))
}

/// How alike two flag names are, 0.0–1.0, by longest common subsequence.
fn similarity(left: &str, right: &str) -> f64 {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let common = lcs_len(&left, &right);
    2.0 * common as f64 / (left.len() + right.len()) as f64
}

/// Length of the longest common subsequence, in one rolling row.
fn lcs_len(left: &[char], right: &[char]) -> usize {
    let mut row = vec![0usize; right.len() + 1];
    for candidate in left {
        let mut diagonal = 0usize;
        for (index, other) in right.iter().enumerate() {
            let above = row[index + 1];
            row[index + 1] = if candidate == other {
                diagonal + 1
            } else {
                row[index].max(above)
            };
            diagonal = above;
        }
    }
    row[right.len()]
}
