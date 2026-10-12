//! The REA-lite `strings` leg: the *interesting* strings in a vendor's own artefact.
//!
//! `DESIGN.md` §17 describes the degraded structural check as version + `--help` + selected
//! binary strings, and for two milestones the third leg was inert
//! (`ACCEPTANCE.md` §13 B34): `CliSnapshot::strings` had **no producer**, and the obvious
//! one is *wrong* — running the extractor over the resolved program would read **node's**
//! bytes for codex, because codex is `node <codex.js>` on this platform, and would report
//! node's strings as codex's. That is the exact trap `version_command`/`help_command` exist
//! to avoid, one leg over.
//!
//! This module is the producer's pure half. [`crate::provider::Provider::artefact`] names
//! the file to read — for codex, the script rather than the launcher — and
//! [`crate::commands::watch`] performs the bounded read. Neither belongs here: a spawn or a
//! file read inside a pure function is a side effect no test can count, which is the same
//! reason [`crate::watch::drift::diff`] is pure.
//!
//! ## Only the strings that could signal a change
//!
//! Dumping every printable run from a 193 MB `agy.exe` would make one snapshot megabytes
//! and the diff unreadable, and a report nobody reads is a report that finds nothing. So
//! only two kinds of evidence are kept:
//!
//! * **long flag tokens** (`--print-timeout`, `--json`), because a flag is a contract pie
//!   builds argv against. The **long form only**, for the reason the first version of this
//!   leg had to be corrected: a short flag (`-x`) is a single letter, and in a binary a
//!   single letter after a dash is everywhere — format verbs, register names, getopt
//!   letters, `-C`, `-g`, `-1v`. Measured against the real artefacts, the short-flag rule
//!   produced 268 entries that were **almost entirely Go compiler and runtime noise**, on a
//!   list from which *every real `agy` flag was absent*; the noise churns on every rebuild,
//!   so the leg would have reported false drift while missing the drift it exists to see.
//!   The long form is what a CLI's contract is written in, and the thing the short form was
//!   meant to catch (`-h`, `-p`) is also the least likely to change in a way that matters.
//!   A *token* rather than a whole run, because a flag is rarely alone: a JavaScript file
//!   holds `"--json"`, a usage line holds `[--print <PROMPT>]`, and a launcher script holds
//!   `"%1"=="--version"`. Anchoring on the start of a run would miss every one of those.
//! * **policy, consent and automation vocabulary** ([`POLICY_WORDS`]), because that is what
//!   [`crate::watch::DriftClass::PieMustStop`] exists to notice, and it appears in a binary
//!   long before it appears in `--help`. Here the **whole run** is kept rather than the
//!   word: the sentence around `consent` is the evidence a human judges.
//!
//! ## Incremental by construction, and why that is not optional
//!
//! The first version read a **16 MiB prefix** of the artefact and stated that it had. On the
//! machine this was written for, `agy.exe` is 193 MB, so the prefix was decided *before* the
//! flags could appear — honesty about the prefix did not save the feature, because the
//! strings the leg exists to see were outside the read window.
//!
//! The bound that matters is on **memory**, not on how much is read: a file is streamed to
//! EOF in chunks and only the matches are kept, so reading cost is one pass of disk and the
//! kept state does not grow with the file. [`StringScanner`] is that stream, and it holds
//! the **partial run across chunk boundaries** — a tail between calls, never a re-read — so a
//! string straddling two reads is neither lost nor split into two. It is the same shape
//! [`crate::provider::sse::SseDecoder`] already uses for a different input, and for the same
//! reason: the chunk boundary is decided by the reader, not by the data.
//!
//! One consequence is a property this module must preserve and a test must pin: **the result
//! does not depend on how the bytes were chunked.** [`interesting_strings`] is the scanner
//! fed one slice, and the chunked driver in [`crate::commands::watch`] feeds the same
//! scanner many — so the two are the same code path by construction, and the test proves the
//! chunk boundaries are invisible.
//!
//! ## The cap may not let prose bury a flag
//!
//! Measured against the real `agy.exe` (193 MB, v1.3.3), the selection above finds ~580
//! long flags and ~14 900 policy sentences, and `--print-timeout` does not appear until
//! ~1 900 distinct strings into the file. A cap that admitted whichever string came first in
//! byte order would therefore be **full of prose before a contract flag arrived**, silently
//! losing the one thing this leg exists to see — the 16 MiB prefix failing again, one layer
//! down. So the cap is shared but not first-come: when it is reached a flag **evicts a
//! policy sentence** rather than being dropped by one, and only prose is ever lost to the
//! bound. The cap is still a hard bound on memory, and a crafted artefact cannot grow it.

use std::collections::BTreeSet;

use crate::error::truncate;

/// The most bytes read from any one artefact.
///
/// A *streaming* bound, not a prefix. The artefacts pie has seen are far below it — `agy.exe`
/// is ~193 MB and codex's native binary ~334 MB on the machine this was written for — so
/// every real artefact is read to EOF and the note stays `None`. It exists only so a
/// pathological multi-gigabyte file cannot make a monthly job read forever; when it is hit,
/// the snapshot's `strings_note` **states the number** rather than silently comparing a
/// prefix a reader would take for the whole file (`ACCEPTANCE.md` §13 B22).
pub const MAX_ARTEFACT_BYTES: u64 = 1024 * 1024 * 1024;

/// The most strings one snapshot keeps.
///
/// A single cap over the whole result, and a **memory** bound: it is enforced while the set
/// accumulates, not only on the returned vector, so a crafted artefact made of flag tokens
/// cannot grow the set without limit before the cap is applied. It is large enough for the
/// flags of a real CLI (the `agy.exe` this was measured against has ~580) while leaving room
/// for policy sentences — and, crucially, a flag may evict a policy sentence but never the
/// reverse, so the prose a vendor embeds cannot bury the contract.
pub const MAX_STRINGS: usize = 1024;

/// The longest one extracted string may be.
///
/// One printable region can be arbitrarily long, and a single one of those would push a
/// snapshot past the point where anyone reads it. The cut is marked by
/// [`crate::error::truncate`], so a truncated string is never mistaken for the whole one.
const MAX_STRING_CHARS: usize = 200;

/// How much of an open run is retained while it streams.
///
/// One over [`MAX_STRING_CHARS`] on purpose: at exactly the cap, [`truncate`] can still see a
/// string longer than the limit and mark the cut, while a run that fits is kept whole and
/// unmarked. Retaining more would be retaining bytes the cap throws away.
const RETENTION: usize = MAX_STRING_CHARS + 1;

/// The policy, consent and automation vocabulary that makes a run interesting on its own.
///
/// Deliberately the vocabulary [`crate::watch::DriftClass::PieMustStop`] exists to notice
/// (`PLAN.md` §11a): a vendor that starts gating unattended use writes `consent`, `opt-out`,
/// `telemetry` or `permission` into its binary long before the sentence reaches `--help`.
///
/// Matched case-insensitively and as a **substring**, which is the safe direction for this
/// job: over-selecting a string costs one line in a diff, while missing one costs the whole
/// point of the leg — and this leg exists so a policy change is seen *early*.
pub const POLICY_WORDS: [&str; 12] = [
    "consent",
    "terms",
    "agree",
    "policy",
    "privacy",
    "opt-out",
    "telemetry",
    "permission",
    "approve",
    "deny",
    "track",
    "analytics",
];

/// The widest window a vocabulary word needs, derived from the words themselves so it cannot
/// drift from the list.
const MAX_POLICY_LEN: usize = {
    let mut max = 0usize;
    let mut index = 0usize;
    while index < POLICY_WORDS.len() {
        let len = POLICY_WORDS[index].len();
        if len > max {
            max = len;
        }
        index += 1;
    }
    max
};

/// Whether a lower-cased byte can be the **last** byte of a vocabulary word.
///
/// Derived from [`POLICY_WORDS`] so it cannot drift from them, and 256 wide so the test is a
/// load rather than a scan: the run is 193 MB of a real binary, and a per-byte loop over the
/// whole vocabulary was the cost of the read (measured before this gate existed).
const POLICY_END: [bool; 256] = {
    let mut table = [false; 256];
    let mut index = 0usize;
    while index < POLICY_WORDS.len() {
        let bytes = POLICY_WORDS[index].as_bytes();
        table[bytes[bytes.len() - 1] as usize] = true;
        index += 1;
    }
    table
};

/// The interesting strings in `bytes`, de-duplicated, sorted and capped at `cap`.
///
/// Pure by construction: no file, no CLI, no clock, no allocation the caller did not ask
/// for. See the module docs for what "interesting" means.
///
/// The result is collected from a [`BTreeSet`], so two runs over the same bytes are
/// byte-identical. That is a requirement and not a nicety: a snapshot that reordered itself
/// between runs would be diffed as a change that never happened, and the whole leg would
/// turn into noise.
pub fn interesting_strings(bytes: &[u8], cap: usize) -> Vec<String> {
    let mut scanner = StringScanner::new(cap);
    scanner.feed(bytes);
    scanner.finish()
}

/// The incremental extractor: bytes in, strings out, no file and no clock.
///
/// [`interesting_strings`] is this scanner fed one slice; [`crate::commands::watch`] feeds it
/// the artefact in bounded chunks. Every piece of state is bounded by a constant — the open
/// run's retained head, the open flag token, the policy window and the capped result set —
/// so a huge file, or an endless run of flag-like bytes, cannot make the scanner's memory
/// grow with the input.
///
/// The run is the unit a *policy sentence* is read from, so the run — not the chunk — is
/// what must survive a boundary: a run split across two reads is finished only when the byte
/// that ends it arrives, however many chunks that takes.
#[derive(Debug)]
pub struct StringScanner {
    /// The most strings the result keeps, across both kinds.
    cap: usize,
    /// Long-flag tokens, de-duplicated.
    ///
    /// Bounded only by [`StringScanner::cap`], and given priority over policy sentences:
    /// the contract evidence must survive however much prose the vendor embeds.
    flags: BTreeSet<String>,
    /// Policy-vocabulary sentences, de-duplicated.
    ///
    /// Bounded by whatever room is left under [`StringScanner::cap`] once the flags have
    /// their share — the kind that may be evicted to make room for a flag.
    policy: BTreeSet<String>,
    /// The open run's first retained bytes, after its leading spaces.
    run_head: Vec<u8>,
    /// Spaces seen since the run's last non-space byte.
    ///
    /// Held back rather than appended because a run's **trailing** spaces are trimmed while
    /// the same spaces between words are content — and which one they are is only known once
    /// the next non-space byte (or the run's end) arrives.
    pending_spaces: usize,
    /// Whether the open run has a non-space byte yet; leading spaces never start it.
    run_started: bool,
    /// Whether the open run carries a [`POLICY_WORDS`] word, anywhere in the run.
    ///
    /// Tracked by [`StringScanner::window`] rather than by the retained head, so a word
    /// beyond the retained bytes is still seen.
    run_has_policy: bool,
    /// The open run's trailing bytes, lower-cased, wide enough to hold any vocabulary word.
    window: Vec<u8>,
    /// The open flag token's first retained bytes.
    token: Vec<u8>,
    /// Whether a printable run is currently open.
    in_run: bool,
}

impl StringScanner {
    /// A scanner that will keep at most `cap` strings.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            flags: BTreeSet::new(),
            policy: BTreeSet::new(),
            run_head: Vec::new(),
            pending_spaces: 0,
            run_started: false,
            run_has_policy: false,
            window: Vec::new(),
            token: Vec::new(),
            in_run: false,
        }
    }

    /// Feed one arbitrary chunk of the artefact.
    ///
    /// Chunking is invisible: the state that would straddle a boundary — the open run and
    /// the open token — is held here between calls.
    pub fn feed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if is_printable(byte) {
                if !self.in_run {
                    self.begin_run();
                }
                self.push_printable(byte);
            } else if self.in_run {
                // A byte that cannot be part of a run ends it. That includes the byte that
                // *would* be the straddling string's separator, so a run is never split by a
                // chunk boundary — only by the data.
                self.end_run();
            }
        }
    }

    /// Finish the input and take the de-duplicated, sorted result.
    ///
    /// A run with no separator to close it — the artefact's final string — is flushed here.
    /// Without this the last run is lost, and that is not an edge case: any file whose final
    /// bytes are a string does it, every time, and the missing string would be a silent hole
    /// in the baseline rather than an error.
    pub fn finish(mut self) -> Vec<String> {
        if self.in_run {
            self.end_run();
        }
        // Two bounded sets merged into one result: the merge is still a set, so the answer
        // is de-duplicated and sorted exactly as a single set would be.
        self.flags.extend(self.policy);
        self.flags.into_iter().collect()
    }

    /// Open a fresh run, dropping the previous run's state.
    fn begin_run(&mut self) {
        self.in_run = true;
        self.run_head.clear();
        self.pending_spaces = 0;
        self.run_started = false;
        self.run_has_policy = false;
        self.window.clear();
        self.token.clear();
    }

    /// Close the open run: flush its last token, then keep its sentence if it earned one.
    fn end_run(&mut self) {
        self.flush_token();
        self.keep_run_sentence();
        self.in_run = false;
    }

    /// One printable byte: an open token, the policy window, and the run's head.
    fn push_printable(&mut self, byte: u8) {
        // A flag token is a maximal run of token bytes, so a byte that cannot be inside one
        // closes the token that was open. This is independent of run boundaries because a
        // token's bytes are all printable, so a token cannot span a run.
        if is_token_byte(byte) {
            if self.token.len() < RETENTION {
                self.token.push(byte);
            }
        } else {
            self.flush_token();
        }

        // The policy window holds the run's tail, lower-cased on the way in so a vendor's
        // `Consent` matches the word `consent` and the comparison below is a plain byte
        // equality. The window is what carries a word across a chunk boundary. It is watched
        // only until the run has proved it carries a word; after that there is nothing left
        // for it to add, and a run that mentions `consent` is usually a long one.
        if !self.run_has_policy {
            let lowered = byte.to_ascii_lowercase();
            self.window.push(lowered);
            if self.window.len() > MAX_POLICY_LEN {
                self.window.remove(0);
            }
            self.note_policy(lowered);
        }

        // The run's retained head, trimmed: leading spaces never start it…
        if !self.run_started {
            if byte == b' ' {
                return;
            }
            self.run_started = true;
        }

        // …and trailing spaces wait in `pending_spaces` until a non-space proves they were
        // interior.
        if byte == b' ' {
            self.pending_spaces += 1;
            return;
        }

        self.commit_spaces();
        if self.run_head.len() < RETENTION {
            self.run_head.push(byte);
        }
    }

    /// Move the held-back spaces into the head, as far as the retention allows.
    fn commit_spaces(&mut self) {
        while self.pending_spaces > 0 && self.run_head.len() < RETENTION {
            self.run_head.push(b' ');
            self.pending_spaces -= 1;
        }
        // Whatever the cap could not hold would be truncated away from the sentence anyway,
        // so the count is dropped rather than carried.
        self.pending_spaces = 0;
    }

    /// Record whether the run's tail ends with a vocabulary word.
    ///
    /// The window is at most [`MAX_POLICY_LEN`] bytes, so every word that can occur has just
    /// occurred when its final byte arrives — which is why a substring anywhere in the run is
    /// caught without ever holding the run.
    fn note_policy(&mut self, lowered: u8) {
        if self.run_has_policy {
            return;
        }
        // Every word ends where its own last byte does, so a byte that cannot end one skips
        // the vocabulary entirely and the window still holds the bytes that follow it.
        if !POLICY_END[lowered as usize] {
            return;
        }
        for word in POLICY_WORDS {
            let bytes = word.as_bytes();
            if self.window.len() >= bytes.len()
                && &self.window[self.window.len() - bytes.len()..] == bytes
            {
                self.run_has_policy = true;
                return;
            }
        }
    }

    /// Keep the open token if it has the shape of a long flag.
    fn flush_token(&mut self) {
        if self.token.is_empty() {
            return;
        }
        // **A run can hold several flags fused together**, and that is the normal case rather
        // than an oddity: a vendor stores its flag table concatenated. Measured on the real
        // `codex.exe`, one run reads
        // `--sandbox-policy-cwd--permission-profile--use-legacy-landlock--managed-networkcommand`
        // — five flags and no separators. Keeping that as one token did two bad things: the
        // report showed a 90-character blob instead of naming a flag, and because a kept
        // string is cut at [`MAX_STRING_CHARS`], a change *late in the blob* was invisible.
        // That is the primary signal of this whole leg, lost to a bound meant for prose, so
        // the split is not cosmetic.
        // Collected before anything is kept: `flags_in` borrows the token while `keep_flag`
        // needs `&mut self`, and holding borrowed slices across the mutation is exactly what
        // the borrow checker is right to refuse.
        let flags: Vec<String> = flags_in(&self.token)
            .into_iter()
            // Every byte of a token is printable ASCII, so the lossy conversion is exact; it is
            // used because it cannot panic on a byte slice, which is what a binary is.
            .map(|flag| String::from_utf8_lossy(flag).into_owned())
            .collect();
        for flag in &flags {
            self.keep_flag(flag);
        }
        self.token.clear();
    }

    /// Keep the open run's sentence when the run carried a policy word.
    ///
    /// The whole sentence around the word is the evidence a human judges, so the run is kept
    /// — trimmed, and truncated by [`StringScanner::keep_truncated`] like anything else.
    fn keep_run_sentence(&mut self) {
        if !self.run_started || !self.run_has_policy {
            return;
        }
        let text = String::from_utf8_lossy(&self.run_head).into_owned();
        if !text.is_empty() {
            self.keep_policy(&text);
        }
    }

    /// Add one long-flag string, cut to [`MAX_STRING_CHARS`].
    ///
    /// A flag is the contract evidence this whole leg exists to see, so when the cap is
    /// reached it **evicts a policy sentence** rather than being dropped: only prose is ever
    /// lost to the bound. Without that rule the ~15k policy sentences a real 193 MB binary
    /// carries would fill the cap before the first flag arrived — measured, not theorised.
    fn keep_flag(&mut self, text: &str) {
        let text = truncate(text, MAX_STRING_CHARS);
        if self.flags.contains(&text) {
            return;
        }
        if self.total() < self.cap {
            self.flags.insert(text);
            return;
        }
        // The cap is reached: room can only be made if a policy sentence gives it up, and
        // only if there is one. `pop_last` is deterministic, so the eviction — and with it
        // the result — does not depend on hash order or on how the bytes were chunked.
        if self.flags.len() < self.cap && self.policy.pop_last().is_some() {
            self.flags.insert(text);
        }
    }

    /// Add one policy sentence, cut to [`MAX_STRING_CHARS`], when there is room for it.
    ///
    /// A sentence never evicts anything: it is the looser evidence (the substring match
    /// over-selects on purpose), so it yields to the flags rather than the other way round.
    fn keep_policy(&mut self, text: &str) {
        let text = truncate(text, MAX_STRING_CHARS);
        if self.policy.contains(&text) {
            return;
        }
        if self.total() < self.cap {
            self.policy.insert(text);
        }
    }

    /// How many strings the result holds so far, across both kinds.
    fn total(&self) -> usize {
        self.flags.len() + self.policy.len()
    }
}

/// Whether a byte can be part of a printable run.
///
/// ASCII graphic plus space, which is the rule the `strings(1)` this stands in for uses. The
/// consequence is worth stating: every byte at or above `0x80` is a separator, so a run is
/// always valid ASCII and a multi-byte character can never be cut at a non-boundary —
/// invariant I6 is satisfied by construction, on the least forgiving input the project has.
fn is_printable(byte: u8) -> bool {
    byte.is_ascii_graphic() || byte == b' '
}

/// The bytes a flag token is made of: a letter or digit, or a hyphen between them.
///
/// Deliberately not `_`: real long flags are hyphenated (`--print-timeout`, `--add-dir`), and
/// treating an underscore as a token byte would fuse `--foo_bar` into one word that matches
/// nothing.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-'
}

/// Whether a token is a **long** flag: `--`, then a letter, then letters, digits or hyphens.
///
/// Every byte of a token already satisfies the tail of that rule — [`is_token_byte`] is the
/// only source of token bytes — so the shape is decided by the first three bytes, which are
/// always retained. A short flag (`-x`, `-C`, `-1v`) is rejected on purpose: see the module
/// docs for the measurement that made it noise. So is `--`, `---` and `--2fast`: a name that
/// does not start with a letter is not a flag a CLI documents.
fn is_long_flag(token: &[u8]) -> bool {
    matches!(token, [b'-', b'-', third, ..] if third.is_ascii_alphabetic())
}

/// Every long flag inside a token, in order.
///
/// A flag's body ends at the next `--`, because a real long flag does not contain a double
/// hyphen — which is what makes the split safe, and what stops `--approve-for-me` from
/// absorbing whatever follows it in a fused table. The first three bytes are checked before a
/// candidate is accepted, so `--`, `---` and `--2fast` are still rejected ([`is_long_flag`]).
///
/// A token with a single flag returns that flag, so the unfused case is unchanged.
fn flags_in(token: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    let mut index = 0;

    while index < token.len() {
        let Some(rest) = token.get(index..) else {
            break;
        };
        if is_long_flag(rest) {
            let mut end = token.len();
            let mut probe = index + 2;
            while probe + 1 < token.len() {
                if token[probe] == b'-' && token[probe + 1] == b'-' {
                    end = probe;
                    break;
                }
                probe += 1;
            }
            out.push(&token[index..end]);
            // Resume at the separator, so the next flag is found rather than skipped: `end`
            // points at the first hyphen of the next `--`.
            index = end;
        } else {
            index += 1;
        }
    }

    out
}
