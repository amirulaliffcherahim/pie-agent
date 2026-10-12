//! The news digest (`DESIGN.md` §17).
//!
//! Fetches configured sources and ranks them by **rule** against the stack the user
//! configured in `profile.toml`. No model is consulted, so this costs no quota — which is
//! `PLAN.md` §11a's "network-only (no model tokens unless you ask for a digest)".
//!
//! The transport is a trait rather than a direct `reqwest` call for one reason: **a test
//! must not reach the network.** A digest test that depends on a live feed is a test that
//! fails when a blog reorders its index, which teaches nothing about pie.

use crate::error::{truncate, PieError, Result};
use crate::process;

use std::cmp::Reverse;

/// One configured feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsSource {
    /// A short name for the report ("Hacker News", "arXiv cs.SE").
    pub name: String,
    /// The URL to fetch.
    pub url: String,
}

/// A transport, so [`fetch`] is testable without a network.
pub trait Http: Send + Sync {
    /// Fetch a URL and return its body.
    fn get(&self, url: &str) -> Result<String>;
}

/// One headline, after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsItem {
    /// The headline.
    pub title: String,
    /// Where it came from.
    pub url: String,
    /// The source's name.
    pub source: String,
    /// Rule-based impact score against the user's stack.
    pub impact: u32,
    /// Which stack terms matched, so the score can be judged rather than trusted.
    pub matched: Vec<String>,
}

/// Score and order items against the user's stack. **Pure.**
///
/// An empty stack ranks everything at zero and preserves input order — it does not
/// invent an order, because a digest sorted by nothing is a digest the reader cannot
/// interpret.
///
/// ## The rule, stated because a score nobody can explain is a score nobody can trust
///
/// One **distinct** matched stack term is one point of [`NewsItem::impact`], and every term
/// that contributed is recorded in [`NewsItem::matched`], in the order the user configured
/// them. A term matches when it appears as a **whole word** in the headline or in the
/// item's URL — whole words because `ai` is a plausible stack term and "said" is a plausible
/// headline, and a substring rule would score the second as a hit. The URL is included
/// because a project's name often lives there (`github.com/rust-lang/…`) even when the
/// headline only implies it; it is not counted twice when the headline names it too.
///
/// Terms are trimmed, compared case-insensitively, and deduplicated, so the score does not
/// depend on how the user happened to spell or order `profile.toml`. `impact` and `matched`
/// are **recomputed, never accumulated**: ranking an item twice gives the same answer as
/// ranking it once.
///
/// Order is by impact, descending, and **ties keep the input order** — the feed's own
/// order, which is part of what the feed said. Stable sorting is what makes two runs over
/// identical input byte-identical, which is what lets a digest be compared rather than
/// re-read.
pub fn rank(items: Vec<NewsItem>, stack: &[String]) -> Vec<NewsItem> {
    let terms = normalise_stack(stack);

    let mut ranked: Vec<NewsItem> = items
        .into_iter()
        .map(|mut item| {
            item.matched = matching_terms(&item, &terms);
            item.impact = item.matched.len() as u32;
            item
        })
        .collect();

    // `sort_by_key` is **stable**, so equal impacts keep the order they arrived in. An
    // unstable sort here would make two runs over the same input disagree for no gain,
    // which is also why the digest is asserted on as a whole `Vec` rather than row by row.
    ranked.sort_by_key(|item| Reverse(item.impact));
    ranked
}

/// Fetch and parse one source.
///
/// A failure is a **stated error naming the source and the URL**, never an empty digest:
/// the same rule M5's `ocr` stage shipped (`ACCEPTANCE.md` §8). A digest silently missing
/// a source reads as "nothing happened".
///
/// # Formats
///
/// **RSS 2.0 and Atom 1.0**, decided from the document's root element. Anything else — an
/// HTML error page, a JSON error body, a proxy notice, an empty body — is a stated error,
/// because a body pie cannot read is not the same thing as a source with no news. (RSS 1.0
/// / RDF is read too, since it carries the same `<item>`/`<title>`/`<link>` names.)
pub fn fetch(source: &NewsSource, http: &dyn Http) -> Result<Vec<NewsItem>> {
    // The transport's error is wrapped rather than propagated with `?`: a bare
    // `reqwest`/I/O message names neither the source nor the URL, and an error that names
    // one of the two still leaves the reader unable to act.
    let body = http
        .get(&source.url)
        .map_err(|error| unreachable_source(source, &error))?;

    parse(source, &body)
}

// ------------------------------------------------------------------------- ranking --

/// A configured stack term: the user's own spelling for the report, and the form the
/// comparison uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    /// As configured, so `matched` reports the user's spelling rather than a rewrite of it.
    display: String,
    /// Trimmed and lowercased, which is the form looked for in a headline.
    needle: String,
}

/// Trim, lowercase, drop blanks and collapse duplicates.
///
/// Blanks are dropped rather than matched: a stack of `["", "  "]` is a stack the user has
/// not filled in yet, and treating a blank term as a match would score every headline
/// equally instead of ranking none of them.
fn normalise_stack(stack: &[String]) -> Vec<Term> {
    let mut terms: Vec<Term> = Vec::new();

    for raw in stack {
        let needle = raw.trim().to_lowercase();
        if needle.is_empty() || terms.iter().any(|term| term.needle == needle) {
            continue;
        }
        terms.push(Term {
            display: raw.trim().to_string(),
            needle,
        });
    }

    terms
}

/// Every term that matched, in the order the user configured them.
fn matching_terms(item: &NewsItem, terms: &[Term]) -> Vec<String> {
    let title = item.title.to_lowercase();
    let url = item.url.to_lowercase();

    terms
        .iter()
        .filter(|term| contains_term(&title, &term.needle) || contains_term(&url, &term.needle))
        .map(|term| term.display.clone())
        .collect()
}

/// Does `haystack` contain `needle` as a whole word?
///
/// A word is delimited by anything that is not alphanumeric or `_`, so `rust` matches
/// "rust-lang.org" and "Rust 1.87" but not "trust", and `go` does not match "Google".
///
/// Every slice here is indexed at a boundary `str::find` reported, never at a byte count
/// this code chose: a multi-byte character sliced through is a panic on external input,
/// which is invariant I6.
fn contains_term(haystack: &str, needle: &str) -> bool {
    // `needle` is never empty (`normalise_stack` drops blanks) and an empty needle would
    // match at every offset, so this is a loop that does not terminate rather than a
    // ranking bug.
    if needle.is_empty() {
        return false;
    }

    let mut from = 0;

    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();

        let before_is_word = haystack[..start]
            .chars()
            .next_back()
            .is_some_and(is_word_char);
        let after_is_word = haystack[end..].chars().next().is_some_and(is_word_char);

        if !before_is_word && !after_is_word {
            return true;
        }

        // Resume after this hit: a rejected hit can contain an accepted one ("aiai" holds
        // no whole `ai`, so the next offset is the only chance to find one).
        from = end;
    }

    false
}

/// What counts as part of a word for the whole-word rule above.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ------------------------------------------------------------------------- parsing --

/// Parse a fetched body into items.
///
/// Three outcomes, and the difference between them is the point of the whole function: an
/// empty body or a body that is not a feed is an **error** (something is wrong with the
/// fetch, or with the URL, or with a proxy in front of it); a feed whose items are all
/// unusable is an **error** (a blank digest is the outcome `ACCEPTANCE.md` §16 rules out);
/// and a feed with headlines is news.
fn parse(source: &NewsSource, body: &str) -> Result<Vec<NewsItem>> {
    if body.trim().is_empty() {
        return Err(stated(
            source,
            "returned an empty body — a feed with no bytes is a failure, not a digest with \
             no items",
        ));
    }

    let tokens = tokenize(body);
    let root = tokens.iter().find_map(|token| match token {
        Token::Open { name, .. } => Some(name.as_str()),
        _ => None,
    });
    let (items, containers) = collect_items(source, &tokens);

    let Some(root) = root else {
        return Err(not_a_feed(
            source,
            body,
            "the body contains no element at all",
        ));
    };

    // The item names are read whatever the root says, so a feed that declares `rss` and
    // carries `<entry>` is still readable. The root only decides what "not a feed" means.
    if !matches!(root, "rss" | "feed" | "rdf:rdf") && items.is_empty() {
        return Err(not_a_feed(
            source,
            body,
            &format!("the first element is `{root}`, not `rss` or `feed`"),
        ));
    }

    if items.is_empty() {
        return Err(stated(source, &no_items(containers)));
    }

    Ok(items)
}

/// A body that is not a feed, with enough of the body quoted to see why.
///
/// The excerpt is the useful part: the failure this catches in practice is a 200 carrying
/// an HTML error page or a proxy notice, and "not a feed" alone would not say which.
fn not_a_feed(source: &NewsSource, body: &str, reason: &str) -> PieError {
    stated(
        source,
        &format!(
            "did not answer with a feed: {reason}. An HTML error page or a proxy notice is \
             the usual cause. First bytes: `{}`",
            snippet(body)
        ),
    )
}

/// Why a source produced nothing usable, said precisely enough to act on.
///
/// A feed that answered but yielded no headline is reported rather than passed along as an
/// empty digest, because an empty digest reads as "nothing happened". A genuinely quiet
/// blog lands here too — the count is what tells the two apart, and a report line naming
/// the source is diagnosable where a blank one is not.
fn no_items(containers: usize) -> String {
    if containers == 0 {
        "answered with a feed that contains no <item> or <entry> — refusing to report an \
         empty digest"
            .to_string()
    } else {
        format!(
            "answered with {containers} items, none of which carried a title — refusing to \
             report an empty digest"
        )
    }
}

/// A stated failure naming the source **and** the URL.
///
/// Both, not one: a failure that names the source but not the URL leaves the reader unable
/// to find what answered, and a URL with no source name leaves them unable to find the
/// entry in their own config.
fn stated(source: &NewsSource, detail: &str) -> PieError {
    PieError::protocol(source.name.clone(), format!("{} {detail}", source.url))
}

/// A transport failure, restated so it names the source and the URL.
fn unreachable_source(source: &NewsSource, error: &PieError) -> PieError {
    let cause = truncate(&process::redact(&error.to_string()), 300);

    // A URL pie cannot use is not a network problem, so "check your connection" is the
    // wrong advice for it. The cause keeps its own kind and exit code (`error.rs`: a config
    // error exits 2), and both messages still name the source and the URL.
    if error.kind() == "config" {
        return PieError::Config(format!(
            "news source `{}` at {} is unusable: {cause}",
            source.name, source.url
        ));
    }

    PieError::NotReachable(format!(
        "news source `{}` at {}: {cause} — the digest is incomplete without it",
        source.name, source.url
    ))
}

/// A bounded, redacted, single-line excerpt of a body, for a diagnostic.
///
/// Redacted because the text is not ours — a proxy can echo a header back — and bounded
/// because it is rendered onto a terminal line and into a markdown report.
fn snippet(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = process::redact(&collapsed);

    if redacted.trim().is_empty() {
        return "(empty body)".to_string();
    }

    // Char-based truncation: cutting at a byte count would panic on a multi-byte character
    // (I6), and the text being cut is exactly the text nobody promised us.
    truncate(redacted.trim(), 120)
}

/// The lexical events a feed is reduced to.
///
/// No XML dependency: `Cargo.toml` is not this stage's to change, and a feed needs four
/// events rather than a document model. A tree would also need a depth limit to stay
/// panic-free on hostile input, and this scanner never recurses.
#[derive(Debug)]
enum Token {
    /// Character data, entities decoded. CDATA contributes its text verbatim, per XML.
    Text(String),
    /// A start tag or an empty-element tag.
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        empty: bool,
    },
    /// An end tag.
    Close { name: String },
}

/// The longest entity worth looking for. `&#x10FFFF;` is ten bytes, and a `&` in prose is
/// not an entity at all.
const ENTITY_WINDOW: usize = 12;

/// Reduce a body to tokens.
///
/// Junk is skipped rather than rejected here; [`parse`] decides what makes a feed. The one
/// hard rule is that **this never panics**: a body cut off mid-tag, mid-entity or
/// mid-character ends the walk and leaves the tokens already read (I6).
fn tokenize(body: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = body;

    while let Some(tag_start) = rest.find('<') {
        let text = &rest[..tag_start];
        if !text.trim().is_empty() {
            tokens.push(Token::Text(decode_entities(text)));
        }

        let after = &rest[tag_start..];

        if let Some(inner) = after.strip_prefix("<!--") {
            match inner.find("-->") {
                Some(end) => rest = &inner[end + 3..],
                // An unterminated comment swallows the rest of the document, which is what
                // a comment means. Nothing after it can be trusted either way.
                None => break,
            }
            continue;
        }

        if let Some(inner) = after.strip_prefix("<![CDATA[") {
            match inner.find("]]>") {
                Some(end) => {
                    if !inner[..end].is_empty() {
                        tokens.push(Token::Text(inner[..end].to_string()));
                    }
                    rest = &inner[end + 3..];
                }
                None => break,
            }
            continue;
        }

        // Declarations and processing instructions (`<?xml …?>`, `<!DOCTYPE …>`) carry no
        // item data.
        if after.starts_with("<?") || after.starts_with("<!") {
            match after.find('>') {
                Some(end) => rest = &after[end + 1..],
                None => break,
            }
            continue;
        }

        let end = match after.find('>') {
            Some(end) => end,
            None => break,
        };
        // `1` is the byte after `<` and `end` is the byte of `>`; both characters are
        // ASCII, so both are character boundaries and neither slice can panic.
        let inner = &after[1..end];
        rest = &after[end + 1..];

        if let Some(name) = inner.strip_prefix('/') {
            if let Some(name) = local_name(name) {
                tokens.push(Token::Close { name });
            }
            continue;
        }

        let name_end = inner
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(inner.len());
        let Some(name) = local_name(&inner[..name_end]) else {
            continue;
        };

        tokens.push(Token::Open {
            name,
            attrs: attributes(&inner[name_end..]),
            empty: inner.trim_end().ends_with('/'),
        });
    }

    tokens
}

/// An element's name, lowercased; `None` for a nameless tag (`<>`, `</>`).
fn local_name(raw: &str) -> Option<String> {
    let name = raw.split_whitespace().next()?;
    // `<`, `=`, `"` and `'` cannot appear in an element name, so a "tag" holding one is not
    // a tag at all: `<<<>>>` must not be reported as an element named `<<`, and a scanner
    // that accepts it turns junk into a headline's source.
    if name.is_empty() || name.contains(['<', '=', '"', '\'']) {
        return None;
    }
    Some(name.to_ascii_lowercase())
}

/// Attribute name/value pairs: names lowercased, values entity-decoded.
///
/// Works on one tag's interior, which is what bounds the walk. Every advance moves `rest`
/// forward, so a tag full of `=` cannot loop.
fn attributes(raw: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut rest = raw;

    while let Some(eq) = rest.find('=') {
        let before = &rest[..eq];
        // The name is the last word before the `=`, so a tag with a valueless attribute
        // (`<item rdf:about="…">`) still reports the right name for the one that has a
        // value.
        let name = before
            .rsplit_once(char::is_whitespace)
            .map_or(before, |(_, last)| last);
        let name = name.trim().to_ascii_lowercase();

        let value_part = rest[eq + 1..].trim_start();
        let (value, consumed) = read_value(value_part);
        if !name.is_empty() {
            attrs.push((name, decode_entities(&value)));
        }
        rest = &value_part[consumed..];
    }

    attrs
}

/// A value starting at `raw[0]`, and how many bytes it occupied.
///
/// Quoted and unquoted values are both legal XML. Each branch returns a boundary index, so
/// the caller's slice is always valid.
fn read_value(raw: &str) -> (String, usize) {
    for quote in ['"', '\''] {
        if let Some(inner) = raw.strip_prefix(quote) {
            return match inner.find(quote) {
                // The closing quote is one byte, as is the one that was stripped.
                Some(end) => (inner[..end].to_string(), end + 2),
                // An unclosed quote takes the rest of the tag: the only other reading is
                // to guess where it ended, which would also swallow the next attribute.
                None => (inner.to_string(), raw.len()),
            };
        }
    }

    let end = raw.find(char::is_whitespace).unwrap_or(raw.len());
    (raw[..end].to_string(), end)
}

/// XML entities in character data, decoded.
///
/// A bare `&` is legal in prose and in a query string, and an entity pie does not know is
/// not a licence to drop text: both survive as themselves. An unknown numeric reference
/// (`&#x110000;`) is not a character either, so it survives too rather than becoming a
/// replacement character.
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }

    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];

        match entity_at(after) {
            Some((decoded, consumed)) => {
                out.push_str(&decoded);
                rest = &after[consumed..];
            }
            None => {
                out.push('&');
                // `after` starts with `&`, which is one ASCII byte.
                rest = &after[1..];
            }
        }
    }

    out.push_str(rest);
    out
}

/// Decode the entity at the very start of `text` (which begins with `&`): the replacement,
/// and how many bytes it occupied.
fn entity_at(text: &str) -> Option<(String, usize)> {
    const NAMED: [(&str, &str); 5] = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
    ];

    for (source, decoded) in NAMED {
        if text.starts_with(source) {
            return Some((decoded.to_string(), source.len()));
        }
    }

    let digits = text.strip_prefix("&#")?;
    let (digits, radix) = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (digits, 10),
    };

    // Bounded, because an unterminated `&#` would otherwise send us to the end of the body
    // once per occurrence — a quadratic walk over input from the network. The window is cut
    // on a character boundary (see `floor_char_boundary`), so the cut itself is safe.
    let window = floor_char_boundary(digits, ENTITY_WINDOW);
    let end = digits[..window].find(';')?;
    if end == 0 {
        return None;
    }

    let value = u32::from_str_radix(&digits[..end], radix).ok()?;
    let decoded = char::from_u32(value)?;

    let tail = &digits[end + 1..];
    Some((decoded.to_string(), text.len() - tail.len()))
}

/// The largest character boundary at or below `index`.
///
/// Written out because `str::floor_char_boundary` is not stable on the pinned toolchain
/// (`rust-version = "1.75"`), and because a bounded window cut at a byte count is exactly
/// the slice that panics on a multi-byte character (I6).
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut end = index.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// A headline under construction.
#[derive(Debug, Default)]
struct Draft {
    title: String,
    url: String,
    guid: String,
}

impl Draft {
    /// Fold one finished field into the draft. The **first** value wins: an item with two
    /// `<title>` elements is malformed, and taking the later one would let a boilerplate
    /// trailer replace the headline.
    fn absorb(&mut self, field: &str, text: &str) {
        match field {
            "title" if self.title.is_empty() => self.title = collapse(text),
            "link" if self.url.is_empty() => self.url = text.trim().to_string(),
            "guid" | "id" if self.guid.is_empty() => self.guid = text.trim().to_string(),
            _ => {}
        }
    }

    /// An Atom `<link rel="alternate" href="…"/>`: the URL is an attribute, and the feed's
    /// own `rel="self"` link is not the item's.
    fn take_link(&mut self, attrs: &[(String, String)]) {
        if !self.url.is_empty() {
            return;
        }

        let rel = attr(attrs, "rel").unwrap_or_default();
        if !rel.is_empty() && rel != "alternate" {
            return;
        }

        if let Some(href) = attr(attrs, "href") {
            self.url = href.trim().to_string();
        }
    }

    /// The item, or `None` when this was not a headline.
    ///
    /// A title is what makes a headline: an item without one is a trailer, an advert or a
    /// truncated element, and all three are noise in a digest. A missing link falls back to
    /// the feed's own URL — a real place to look — rather than to a fabricated one.
    fn take_item(self, source: &NewsSource) -> Option<NewsItem> {
        let title = self.title.trim().to_string();
        if title.is_empty() {
            return None;
        }

        let url = [self.url, self.guid]
            .into_iter()
            .map(|candidate| candidate.trim().to_string())
            .find(|candidate| !candidate.is_empty())
            .unwrap_or_else(|| source.url.clone());

        Some(NewsItem {
            title,
            url,
            source: source.name.clone(),
            impact: 0,
            matched: Vec::new(),
        })
    }
}

/// The value of an attribute, whose name is already lowercased.
fn attr(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

/// Collapse internal whitespace, so a pretty-printed multi-line title is one line.
///
/// A literal newline in a title would break the report's markdown row, which is a
/// rendering bug that reads like a content bug.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The element names an item container can be.
fn is_container(name: &str) -> bool {
    matches!(name, "item" | "entry")
}

/// The element names a headline field can be.
fn is_field(name: &str) -> bool {
    matches!(name, "title" | "link" | "guid" | "id")
}

/// Read the item containers out of a token stream.
///
/// `(items, containers)`: how many containers were opened, so a feed that yielded nothing
/// can say whether it had no items or only unusable ones.
///
/// Fields are read **only inside a container**, which is what stops a channel's own
/// `<title>` and `<link>` from becoming a headline's.
fn collect_items(source: &NewsSource, tokens: &[Token]) -> (Vec<NewsItem>, usize) {
    let mut items: Vec<NewsItem> = Vec::new();
    let mut containers = 0usize;
    let mut open_containers = 0usize;
    let mut current = Draft::default();
    // The field currently collecting text, and what it has collected. A nested element
    // (`<title>a <b>c</b></title>`) does not end it: its text belongs to the title.
    let mut open_field: Option<(String, String)> = None;

    for token in tokens {
        match token {
            Token::Text(text) => {
                if let Some((_, buffer)) = open_field.as_mut() {
                    buffer.push_str(text);
                }
            }

            Token::Open { name, attrs, empty } => {
                if is_container(name) {
                    if *empty {
                        // `<item/>` holds no headline, and counting it as open would make
                        // every following sibling look nested and lose the rest of the feed.
                        if open_containers == 0 {
                            containers += 1;
                        }
                        continue;
                    }
                    open_containers += 1;
                    if open_containers == 1 {
                        containers += 1;
                        current = Draft::default();
                    }
                    continue;
                }

                // Outside an item (a channel's own `<link>`), or inside a field that is
                // already collecting: neither is this item's data.
                if open_containers == 0 || open_field.is_some() {
                    continue;
                }

                // An attribute-carried link: in Atom the URL is an attribute, not text. The
                // test is `href`, **not** "has attributes" — an RSS `<guid isPermaLink="false">`
                // carries its identifier as text, and treating a tag with attributes as
                // empty silently threw that identifier away.
                if name == "link" && attr(attrs, "href").is_some() {
                    current.take_link(attrs);
                    continue;
                }

                // An empty element has no text to collect.
                if *empty {
                    continue;
                }

                if is_field(name) {
                    open_field = Some((name.clone(), String::new()));
                }
            }

            Token::Close { name } => {
                if is_container(name) {
                    if open_containers > 0 {
                        open_containers -= 1;
                        if open_containers == 0 {
                            // An unclosed field dies with its item: a `<title>` that never
                            // ends is not a headline pie can vouch for.
                            open_field = None;
                            if let Some(item) = std::mem::take(&mut current).take_item(source) {
                                items.push(item);
                            }
                        }
                    }
                    continue;
                }

                if let Some((field, buffer)) = open_field.take() {
                    if field == *name {
                        current.absorb(&field, &buffer);
                    } else {
                        // Some other element closed while a field is open: keep collecting.
                        open_field = Some((field, buffer));
                    }
                }
            }
        }
    }

    (items, containers)
}
