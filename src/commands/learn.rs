//! `pie learn` — mine pie's own sessions, extract by rule, and consolidate.
//!
//! Contract: `planning/DESIGN.md` §16; criteria: `planning/ACCEPTANCE.md` §15 (row
//! "D commands"). Three properties are the shape of this command, and each is a test
//! rather than a comment:
//!
//! * **The default path makes no model call.** [`plan`] is rule-based (`learn::extract`)
//!   plus a pure merge (`memory::consolidate`), and `apply` writes two files under
//!   `$PIE_HOME/memory`. Nothing here resolves a backend or spawns a process unless
//!   `--model` asks for the opt-in review, and *that* path is refused by
//!   `schedule::check_budget` **before** it spawns (`RISKS.md` §13 is the governing
//!   risk).
//! * **`--dry-run` writes nothing** — not the document, not the journal, not even the
//!   `memory/` directory — and spends nothing either, so `--model --dry-run` reports the
//!   rule-based plan without paying for a review. "Run with `--dry-run` to see what it
//!   would do" is only honest if looking is free.
//! * **The report is deterministic.** Every line comes from `extract`'s total order or
//!   from a decision `consolidate` recorded, and no clock is read to render it, so two
//!   runs over an unchanged store print byte-identical reports and a report can be
//!   diffed.
//!
//! What the command deliberately does **not** do is merge a model's reply into
//! `learned.md`. `DESIGN.md` §16 freezes no output format for a model's contribution,
//! so the review is *reported* and the document is written only by the rule-based pass.
//! A scheduled job therefore cannot pollute the persona with model prose, and the user
//! can read exactly what the quota bought.

use std::path::Path;

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::learn::{self, Candidate};
use crate::memory::{self, Consolidation, Journal};
use crate::paths;
use crate::provider::{self, ChatRequest, StreamEvent};
use crate::schedule::{self, Job, RunReport, Spend, Trigger};
use crate::session_log::now_millis;

/// What one `pie learn` pass is asked to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Decide and report, but write nothing and spend nothing.
    pub dry_run: bool,
    /// Print the full report: every candidate extraction found, not only the decisions
    /// taken about the candidates that qualified.
    pub report: bool,
    /// Ask a model to review the candidates. Opt-in, and refused unless a token budget
    /// is configured.
    pub model: Option<String>,
}

/// Everything one pass decided, before anything is written.
///
/// Split out of [`run`] so the decision half — mine, extract, consolidate — can be
/// inspected without touching the filesystem. That split is what makes "`--dry-run`
/// writes nothing" assertable rather than merely intended: the same [`Plan`] feeds both
/// the dry run and the real one, so a dry run cannot be a different code path that
/// happens to write less.
#[derive(Debug)]
pub struct Plan {
    /// Turns mined from pie's own session store. Vendor stores are opt-in and are not
    /// read here (`RISKS.md` §15).
    pub turns: usize,
    /// Candidates `extract` produced, in its own deterministic order.
    pub candidates: Vec<Candidate>,
    /// What `consolidate` decided to do with them.
    pub consolidation: Consolidation,
}

/// Mine pie's own sessions, extract by rule, and decide. Writes nothing; reads no clock
/// but the one the caller passes.
pub fn plan(home: &Path, now_ms: u64) -> Result<Plan> {
    let mined = learn::mine_sessions(&home.join(paths::SESSIONS_DIR))?;
    let candidates = learn::extract(&mined);
    let existing = memory::read_doc(home)?;
    let consolidation = memory::consolidate(&existing, &candidates, now_ms);
    Ok(Plan {
        turns: mined.len(),
        candidates,
        consolidation,
    })
}

/// Write a plan. The returned journal is the undo handle `pie memory revert` takes.
pub fn apply(plan: &Plan, home: &Path) -> Result<Journal> {
    memory::apply(&plan.consolidation, home)
}

/// The report one pass prints.
#[derive(Debug)]
pub struct Report<'a> {
    /// The decisions.
    pub plan: &'a Plan,
    /// The undo handle, when the pass actually wrote.
    ///
    /// `None` on a dry run, and also `None` when consolidation changed nothing — a run
    /// that writes nothing has nothing to undo, which is why `apply` reports that as an
    /// empty journal rather than an error.
    pub journal: Option<&'a Journal>,
    /// The opt-in review's output, when one ran.
    pub review: Option<&'a Spend>,
    /// What was asked.
    pub options: &'a Options,
    /// Where the document lives.
    pub home: &'a Path,
}

impl Report<'_> {
    /// Render the report. Deterministic: no clock is read.
    pub fn render(&self) -> String {
        let decisions = &self.plan.consolidation;
        let mut out = String::new();

        out.push_str(&format!(
            "learn: {} turn(s) mined, {} candidate(s) extracted\n",
            self.plan.turns,
            self.plan.candidates.len()
        ));

        if self.options.report {
            // The whole list, in `extract`'s total order. This is the half a reader needs
            // to judge the *rules* rather than the outcome, and it is what makes the
            // report worth diffing: a changed rule shows up here even when the document
            // it produces is byte-identical.
            out.push_str(
                "\ncandidates (ranked: confirmations, then recurrence, then kind, then text)\n",
            );
            if self.plan.candidates.is_empty() {
                out.push_str("  none\n");
            }
            for candidate in &self.plan.candidates {
                out.push_str(&format!(
                    "  {:<12} hits {:<3} {}/{} turn {}  {}\n",
                    kind_label(candidate.kind),
                    candidate.hits,
                    candidate.provenance.session_id,
                    source_label(candidate.provenance.source),
                    candidate.provenance.turn_index,
                    candidate.text,
                ));
            }
        }

        out.push_str(&format!(
            "\nadded {} · reinforced {} · demoted {} · skipped {}\n",
            decisions.added.len(),
            decisions.reinforced.len(),
            decisions.demoted.len(),
            decisions.skipped.len()
        ));

        // Every section is printed even when empty, because a report that omitted
        // "demoted 0" would leave the reader unable to tell "nothing was demoted" from
        // "decay was not evaluated at all".
        for (heading, lines) in [
            ("added", &decisions.added),
            ("reinforced", &decisions.reinforced),
            (
                "demoted to lower confidence, still present",
                &decisions.demoted,
            ),
            ("skipped, with the reason", &decisions.skipped),
        ] {
            for line in lines {
                out.push_str(&format!("  {heading}: {line}\n"));
            }
        }

        if self.options.dry_run {
            out.push_str(&format!(
                "\ndry run: nothing was written, and nothing was spent ({} would be written)\n",
                memory::doc_path(self.home).display()
            ));
        } else {
            out.push_str(&format!(
                "\ndoc: {}\n",
                memory::doc_path(self.home).display()
            ));
            match self.journal {
                // Printed only when there is something to undo. An undo id that undoes
                // nothing is worse than no id: it invites a user to revert a state that
                // is already the current one and think the command was a no-op.
                Some(journal) if !journal.added.is_empty() => {
                    out.push_str(&format!("{}\n", super::memory::undo_hint(journal)));
                }
                _ => out.push_str("nothing changed, so there is nothing to undo\n"),
            }
        }

        if let Some(review) = self.review {
            out.push_str(&format!(
                "\nmodel review (opt-in, {} token(s) reported)\n{}\n",
                review.tokens, review.output
            ));
            out.push_str(
                "the review is reported, not merged: `learned.md` is written only by the \
                 rule-based pass\n",
            );
        }

        out
    }
}

/// `pie learn [--dry-run] [--report] [--model <provider/model>]`
pub async fn run(cfg: &Config, options: &Options) -> Result<()> {
    let home = paths::pie_home();
    let plan = plan(&home, now_millis())?;

    // The review runs before anything is written, so a `--model` run the budget cannot
    // cover is a configuration error that leaves `learned.md` exactly as it was
    // (`ACCEPTANCE.md` §15: "a config error, not a silent spend").
    let review = match (&options.model, options.dry_run) {
        (Some(spec), false) => Some(review(cfg, Some(spec), &plan, &home).await?),
        _ => None,
    };

    let journal = match options.dry_run {
        true => None,
        false => Some(apply(&plan, &home)?),
    };

    let report = Report {
        plan: &plan,
        journal: journal.as_ref(),
        review: review.as_ref(),
        options,
        home: &home,
    };
    print!("{}", report.render());
    Ok(())
}

/// The opt-in model review, gated by `schedule::check_budget` **before** it spawns.
///
/// Shared with `pie schedule run learn`, which is the same job under [`Trigger::Manual`]
/// — one mechanism, two entry points, so the budget cannot be enforced on one path and
/// forgotten on the other.
///
/// The backend is resolved *before* the job runs so an unusable `--model` is a usage
/// error (exit 2) that costs nothing, rather than something the budget is blamed for.
pub(crate) async fn review(
    cfg: &Config,
    spec: Option<&str>,
    plan: &Plan,
    home: &Path,
) -> Result<Spend> {
    let payload = review_prompt(plan);
    let estimate = estimate_tokens(&payload);
    let (backend, model) = provider::resolve_target(cfg, spec)?;

    let job = Job::learn(cfg, estimate);
    let mut ledger = super::schedule::read_ledger(cfg, home)?;
    let report = schedule::run_job(&job, &mut ledger, Trigger::Manual, now_millis(), |_job| {
        spawn_review(cfg, &backend, model.as_deref(), &payload)
    })?;
    // Written after the run, and it records what the run *reported* rather than what it
    // was estimated at: the ledger is what makes the budget a per-window ceiling rather
    // than a per-run one (`DESIGN.md` §16, stage C's ruling).
    super::schedule::write_ledger(&ledger, home)?;

    match report {
        RunReport::Ran(spend) => Ok(spend),
        // Unreachable: `Trigger::Manual` does not consult the cadence. An error rather
        // than a zeroed `Spend`, because a review reported as free without having run
        // would be a lie written into the ledger.
        RunReport::NotDue => Err(PieError::Config(format!(
            "job `{}`: nothing to run, and no spend to record",
            job.id
        ))),
    }
}

/// Drive one real backend turn, from inside the synchronous spawn seam.
///
/// `schedule::run_job` takes `FnOnce(&Job) -> Result<Spend>` (stage C's frozen shape)
/// while every backend's `stream` is async, so the two are bridged here.
/// `block_in_place` needs the multi-threaded runtime `main.rs` starts (`#[tokio::main]`
/// defaults to it); on a current-thread runtime it panics rather than blocking, which is
/// the honest failure — the alternative, quietly skipping the turn, would record a spend
/// that never happened.
fn spawn_review(cfg: &Config, backend: &str, model: Option<&str>, payload: &str) -> Result<Spend> {
    let Some(provider) = provider::by_id(backend) else {
        return Err(PieError::UnknownBackend {
            backend: backend.to_string(),
            known: crate::commands::models::known_backends(),
        });
    };

    let mut request = ChatRequest::new(payload);
    request.model = model.map(str::to_string);
    // No persona, deliberately. This is pie asking a question about its own notes, not a
    // turn in the user's name: sending the projection would pay for the user's cached
    // prefix and would put their own rules in front of a question they did not ask.
    let mut streamed = String::new();
    let outcome = {
        let mut sink = |event: StreamEvent| {
            if let StreamEvent::TextDelta(delta) = event {
                streamed.push_str(&delta);
            }
        };
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(provider.stream(cfg, &request, &mut sink))
        })?
    };

    // A backend that streamed nothing still returned text; the streamed copy wins when
    // there is one, because that is what the user would have seen.
    let output = if streamed.is_empty() {
        outcome.text
    } else {
        streamed
    };
    Ok(Spend {
        tokens: outcome.usage.computed_total(),
        output,
    })
}

/// The payload the opt-in review sends.
///
/// A review question, not a consolidation: the reply is reported and never merged (see
/// the module docs). Its purpose is the one judgement M7 cannot make for itself —
/// whether the extracted candidates are *good* — which `ACCEPTANCE.md` §15 records as
/// "not verifiable here".
///
/// **One line, deliberately.** Every backend pie drives without a stdin prompt path
/// receives this in argv, and a Windows batch stand-in cannot receive a multi-line argv
/// at all — the constraint `tests/sessions.rs` documents. A payload with newlines in it
/// would make the only offline way to exercise this path platform-dependent, which is
/// exactly the machine-dependence gate G3d exists to catch.
fn review_prompt(plan: &Plan) -> String {
    let mut prompt = String::from(
        "pie extracted the following facts about a developer by counting recurrence in their \
         own session history. Judge each one on its own and reply with exactly one line per \
         fact, in the form `keep <text>` or `drop <text>`, and nothing else. Candidates:",
    );
    if plan.candidates.is_empty() {
        prompt.push_str(" (none)");
    }
    for candidate in &plan.candidates {
        prompt.push_str(&format!(
            " [{:?}] {} (hits {}, {} / {} turn {});",
            candidate.kind,
            candidate.text,
            candidate.hits,
            candidate.provenance.session_id,
            source_label(candidate.provenance.source),
            candidate.provenance.turn_index,
        ));
    }
    prompt
}

/// The job's token estimate, from the bytes it would send.
///
/// Floored at one: a job that estimated zero is a job a budget of `0` would *allow*,
/// and "a budget of `0` cannot be exceeded by one call" is the claim
/// `schedule::check_budget` exists to make true.
fn estimate_tokens(payload: &str) -> u64 {
    ((payload.len() as u64) / 4).max(1)
}

/// A candidate's kind, as the report spells it.
///
/// Derived from the variant name rather than a second table, so a renamed kind cannot
/// leave the report describing a kind the extractor no longer produces.
fn kind_label(kind: learn::CandidateKind) -> String {
    format!("{kind:?}").to_lowercase()
}

/// A provenance source, as the report spells it.
///
/// `memory.rs` renders the same token in the document's own lines; this one is for the
/// candidate listing, which is printed before anything is written.
fn source_label(source: learn::Source) -> &'static str {
    match source {
        learn::Source::PieSession => "pie-session",
        learn::Source::AgyHistory => "agy-history",
        learn::Source::CodexHistory => "codex-history",
        learn::Source::ClaudeHistory => "claude-history",
    }
}
