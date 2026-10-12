//! Gate G4 for M8 stage B: the news digest's `fetch` and `rank`.
//!
//! **Nothing here reaches the network.** `DESIGN.md` §17 makes the transport a `trait Http`
//! for exactly this reason: a test that fetches a live feed fails when a blog reorders its
//! index, which teaches nothing about pie. Every source in this file is answered by
//! [`FakeHttp`], a table of recorded bytes, and the last test proves statically that
//! `src/watch/news.rs` has no transport of its own to reach.
//!
//! The two rules that get asserted over and over, because they are the ones a plausible
//! refactor breaks:
//!
//! * **an empty digest is never a success** — an unreachable source, an empty body, an HTML
//!   error page and a feed with no usable items are four stated errors, each naming the
//!   source and the URL (`ACCEPTANCE.md` §8's rule, and §16's);
//! * **the order is reproducible** — two rankings of one input are byte-identical, and ties
//!   keep the feed's own order, so the empty-stack case is not a special case but the same
//!   rule with no scores in it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Mutex;

use pie::error::{PieError, Result};
use pie::watch::news::{fetch, rank, Http, NewsItem, NewsSource};

// --------------------------------------------------------------- the fixtures --

const RUST_BLOG_URL: &str = "https://blog.rust-lang.org/feed.xml";
const ARXIV_URL: &str = "https://export.arxiv.org/rss/cs.SE";

/// An RSS 2.0 feed with the shapes that actually turn up: a CDATA title, an escaped
/// ampersand, an item with no title at all (a trailer), a multi-byte title, and a channel
/// whose own `<title>`/`<link>` must never be mistaken for a headline.
const RSS_BLOG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Rust Blog</title>
    <link>https://blog.rust-lang.org/</link>
    <description>News from the Rust team</description>
    <item>
      <title>Async closures land in Rust 1.87</title>
      <link>https://blog.rust-lang.org/2026/01/14/async-closures.html</link>
      <guid isPermaLink="false">rust-blog-async-closures</guid>
      <description>The 1.87 release stabilises async closures.</description>
    </item>
    <item>
      <title><![CDATA[Announcing tokio 2.0 & the new runtime]]></title>
      <link>https://tokio.rs/blog/2026-02-tokio-2</link>
    </item>
    <item>
      <title>Quarterly bake sale &amp; raffle</title>
      <link>https://example.com/bake-sale</link>
    </item>
    <item>
      <link>https://example.com/sponsored</link>
      <description>Sponsored: no headline at all</description>
    </item>
    <item>
      <title>Cooking with gas: 日本語のテキスト</title>
      <link>https://example.com/kitchen</link>
    </item>
  </channel>
</rss>"#;

/// An Atom 1.0 feed. The three `<link>` shapes are deliberate: the feed's own `rel="self"`
/// at channel level, an entry-local `rel="self"` that precedes the real link, and a bare
/// `href` with no `rel` at all.
const ATOM_ARXIV: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>arXiv cs.SE</title>
  <link rel="self" href="https://export.arxiv.org/rss/cs.SE"/>
  <updated>2026-02-01T00:00:00Z</updated>
  <entry>
    <title>Do LLM agents reproduce the bugs they report?</title>
    <link rel="alternate" type="text/html" href="https://arxiv.org/abs/2601.00001"/>
    <id>https://arxiv.org/abs/2601.00001</id>
    <summary>An empirical study.</summary>
  </entry>
  <entry>
    <title>Mutation testing at scale</title>
    <link rel="self" href="https://export.arxiv.org/rss/cs.SE"/>
    <link rel="alternate" href="https://arxiv.org/abs/2601.00002"/>
    <id>tag:arxiv.org,2026:2601.00002</id>
  </entry>
  <entry>
    <title>A third paper with a bare link</title>
    <link href="https://arxiv.org/abs/2601.00003"/>
    <id>tag:arxiv.org,2026:2601.00003</id>
  </entry>
</feed>"#;

// ---------------------------------------------------------------- the stand-in --

/// How the fake answers a URL it has no body for.
///
/// Two shapes, because they are two different failures with two different exit codes, and
/// the difference is what tells the reader whether to check their network or their config.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// Nothing is listening, or the transport refused.
    Refused,
    /// The URL itself is unusable — `htp://`, not a network problem.
    BadUrl,
}

impl Failure {
    fn error(self) -> PieError {
        match self {
            Failure::Refused => {
                PieError::NotReachable("client error: connection refused (test fake)".into())
            }
            Failure::BadUrl => PieError::Config("invalid URL scheme `htp` (test fake)".into()),
        }
    }
}

/// A transport that answers from a table of recorded bytes, and records what it was asked.
///
/// The call log is what distinguishes "answered from the injected transport" from "asked
/// somewhere else as well" — the latter would be a live request in a gate that must run
/// offline forever.
#[derive(Debug)]
struct FakeHttp {
    bodies: Vec<(String, String)>,
    failure: Failure,
    calls: Mutex<Vec<String>>,
}

impl FakeHttp {
    /// Serve `body` for `url`; refuse anything else.
    fn answering(url: &str, body: &str) -> Self {
        Self {
            bodies: vec![(url.to_string(), body.to_string())],
            failure: Failure::Refused,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Refuse every URL.
    fn refusing() -> Self {
        Self {
            bodies: Vec::new(),
            failure: Failure::Refused,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Fail every URL because the URL cannot be used.
    fn bad_url() -> Self {
        Self {
            bodies: Vec::new(),
            failure: Failure::BadUrl,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Http for FakeHttp {
    fn get(&self, url: &str) -> Result<String> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.to_string());

        self.bodies
            .iter()
            .find(|(known, _)| known == url)
            .map(|(_, body)| body.clone())
            .ok_or_else(|| self.failure.error())
    }
}

// ------------------------------------------------------------------- helpers --

fn source(name: &str, url: &str) -> NewsSource {
    NewsSource {
        name: name.to_string(),
        url: url.to_string(),
    }
}

/// A fetched item, in the shape `fetch` hands to `rank`.
fn item(source_name: &str, title: &str, url: &str) -> NewsItem {
    NewsItem {
        title: title.to_string(),
        url: url.to_string(),
        source: source_name.to_string(),
        impact: 0,
        matched: Vec::new(),
    }
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|text| text.to_string()).collect()
}

fn titles(items: &[NewsItem]) -> Vec<String> {
    items.iter().map(|item| item.title.clone()).collect()
}

/// The source URL prefix every stated failure has to carry.
fn asserts_the_failure_names(source: &NewsSource, error: &PieError) {
    let message = error.to_string();
    assert!(
        message.contains(&source.name),
        "a failure must name the source, or the reader cannot find it in their config: {message}"
    );
    assert!(
        message.contains(&source.url),
        "a failure must name the URL, or the reader cannot find what answered: {message}"
    );
    assert!(!message.trim().is_empty(), "never an empty failure");
}

// ------------------------------------------------------------- parsing a feed --

#[test]
fn an_rss_feed_parses_into_one_item_per_headline_with_its_source_and_link() {
    let feed = source("Rust Blog", RUST_BLOG_URL);
    let http = FakeHttp::answering(RUST_BLOG_URL, RSS_BLOG);

    let items = fetch(&feed, &http).unwrap();

    assert_eq!(
        items,
        vec![
            item(
                "Rust Blog",
                "Async closures land in Rust 1.87",
                "https://blog.rust-lang.org/2026/01/14/async-closures.html"
            ),
            item(
                "Rust Blog",
                "Announcing tokio 2.0 & the new runtime",
                "https://tokio.rs/blog/2026-02-tokio-2"
            ),
            item(
                "Rust Blog",
                "Quarterly bake sale & raffle",
                "https://example.com/bake-sale"
            ),
            item(
                "Rust Blog",
                "Cooking with gas: 日本語のテキスト",
                "https://example.com/kitchen"
            ),
        ],
        "a freshly fetched item scores nothing: ranking is a separate, pure step"
    );

    assert_eq!(
        http.calls(),
        vec![RUST_BLOG_URL.to_string()],
        "one source, one request — and through the injected transport, never the network"
    );
}

/// A CDATA title is one headline (not text plus markup), and an entity pie does not know is
/// **kept**: silently dropping `&nosuch;` would be the parser editing the headline.
#[test]
fn a_cdata_title_is_one_headline_and_an_unknown_entity_survives_as_itself() {
    const FEED: &str = r#"<rss version="2.0"><channel>
      <item><title>AT&amp;T vs Vodafone &#39;deal&#x27; &amp; more</title><link>https://example.com/a?x=1&amp;y=2</link></item>
      <item><title>Tom &amp; Jerry, &nosuch; &amp; &#x110000;</title><link>https://example.com/b</link></item>
    </channel></rss>"#;

    let feed = source("Rust Blog", RUST_BLOG_URL);
    let items = fetch(&feed, &FakeHttp::answering(RUST_BLOG_URL, FEED)).unwrap();

    assert_eq!(items[0].title, "AT&T vs Vodafone 'deal' & more");
    assert_eq!(
        items[0].url, "https://example.com/a?x=1&y=2",
        "an ampersand in a query string is data, not an entity"
    );
    assert_eq!(
        items[1].title, "Tom & Jerry, &nosuch; & &#x110000;",
        "an unknown entity and an impossible code point both survive as themselves"
    );
}

#[test]
fn an_atom_entry_takes_its_link_from_the_alternate_href_and_not_the_feeds_self_link() {
    let feed = source("arXiv cs.SE", ARXIV_URL);
    let items = fetch(&feed, &FakeHttp::answering(ARXIV_URL, ATOM_ARXIV)).unwrap();

    assert_eq!(
        items,
        vec![
            item(
                "arXiv cs.SE",
                "Do LLM agents reproduce the bugs they report?",
                "https://arxiv.org/abs/2601.00001"
            ),
            item(
                "arXiv cs.SE",
                "Mutation testing at scale",
                "https://arxiv.org/abs/2601.00002"
            ),
            item(
                "arXiv cs.SE",
                "A third paper with a bare link",
                "https://arxiv.org/abs/2601.00003"
            ),
        ],
        "`rel=\"self\"` is the feed pointing at itself; a bare `href` is the item's link"
    );
}

/// A headline with nowhere to go still has to point somewhere real.
#[test]
fn an_item_with_no_link_is_reported_against_its_source_rather_than_a_fabricated_url() {
    const FEED: &str = r#"<rss version="2.0"><channel>
      <item><title>A headline with no link</title></item>
      <item><title>A guid that is not a permalink</title><guid isPermaLink="false">urn:uuid:7f0e</guid></item>
    </channel></rss>"#;

    let feed = source("Quiet Blog", "https://example.com/rss");
    let items = fetch(&feed, &FakeHttp::answering(&feed.url, FEED)).unwrap();

    assert_eq!(
        items[0].url, feed.url,
        "the source's URL is a real place to look; a made-up link is not"
    );
    assert_eq!(
        items[1].url, "urn:uuid:7f0e",
        "the feed's own identifier beats the feed's URL: it is what the feed calls the item"
    );
}

// -------------------------------------------------------------------- ranking --

#[test]
fn a_stack_match_outranks_a_non_match_and_matched_names_the_terms_that_did_it() {
    let stack = strings(&["rust", "async", "tokio"]);
    let ranked = rank(
        vec![
            item(
                "Rust Blog",
                "Quarterly bake sale",
                "https://example.com/bake-sale",
            ),
            item(
                "Rust Blog",
                "Async closures land in Rust 1.87",
                "https://blog.rust-lang.org/async-closures.html",
            ),
            item(
                "arXiv cs.SE",
                "Async runtime internals",
                "https://arxiv.org/abs/2601.00003",
            ),
        ],
        &stack,
    );

    assert_eq!(
        titles(&ranked),
        strings(&[
            "Async closures land in Rust 1.87",
            "Async runtime internals",
            "Quarterly bake sale",
        ]),
        "impact orders the digest: two matches, then one, then none"
    );

    assert_eq!(ranked[0].impact, 2);
    assert_eq!(
        ranked[0].matched,
        strings(&["rust", "async"]),
        "the score is judged from `matched`, so it is reported in the order the user configured"
    );
    assert_eq!(ranked[1].impact, 1);
    assert_eq!(ranked[1].matched, strings(&["async"]));
    assert_eq!(ranked[2].impact, 0);
    assert!(ranked[2].matched.is_empty());
}

/// Determinism is what lets the digest be compared rather than re-read. Two calls in one
/// process cannot prove it across processes — the stable sort plus the input-order tie rule
/// is what makes it hold — but they do catch a comparator that reads something other than
/// the items.
#[test]
fn two_rankings_of_the_same_input_are_byte_identical() {
    let stack = strings(&["rust", "async", "tokio"]);
    let input = vec![
        item("Rust Blog", "Bake sale", "https://example.com/1"),
        item("Rust Blog", "Async closures", "https://example.com/2"),
        item(
            "Rust Blog",
            "Tokio 2.0 and the runtime",
            "https://example.com/3",
        ),
        item(
            "Rust Blog",
            "Rust 1.88 pre-release",
            "https://example.com/4",
        ),
        item("Rust Blog", "Async in the wild", "https://example.com/5"),
    ];

    let first = rank(input.clone(), &stack);
    let second = rank(input, &stack);

    assert_eq!(
        first, second,
        "the same input ranks the same way, byte for byte"
    );
    assert_eq!(
        rank(first.clone(), &stack),
        first,
        "ranking recomputes rather than accumulates, so it is idempotent"
    );
}

/// The real requirement, and not a degenerate case of the other tests: no stack means no
/// information to sort by, and a digest sorted by nothing is one the reader cannot read.
#[test]
fn an_empty_stack_leaves_every_impact_at_zero_and_keeps_the_input_order() {
    let input = vec![
        item("Rust Blog", "Bake sale", "https://example.com/1"),
        item("Rust Blog", "Async closures", "https://example.com/2"),
        item(
            "Rust Blog",
            "Tokio 2.0 and the runtime",
            "https://example.com/3",
        ),
    ];

    let ranked = rank(input.clone(), &[]);

    assert!(
        ranked
            .iter()
            .all(|item| item.impact == 0 && item.matched.is_empty()),
        "nothing matched, so nothing is scored: {ranked:?}"
    );
    assert_eq!(
        ranked, input,
        "an empty stack does not invent an order — the feed's own order is preserved"
    );
}

/// A stack the user has not filled in yet is the same thing as no stack. Matching a blank
/// term would score every headline equally instead of ranking none of them.
#[test]
fn a_blank_stack_is_an_empty_stack() {
    let stack = strings(&["", "   ", "\t"]);
    let input = vec![
        item("Rust Blog", "Bake sale", "https://example.com/1"),
        item("Rust Blog", "Async closures", "https://example.com/2"),
    ];

    assert_eq!(rank(input.clone(), &stack), input);

    // A stale score from an earlier ranking is recomputed, not kept: the same property
    // that makes `rank` idempotent is what stops a digest from carrying yesterday's
    // numbers after the stack changed.
    let scored = rank(input.clone(), &strings(&["async"]));
    assert_eq!(
        scored[0].title, "Async closures",
        "the match is ranked first"
    );
    assert_eq!(scored[0].impact, 1);

    let rescored = rank(scored.clone(), &stack);
    assert!(
        rescored
            .iter()
            .all(|item| item.impact == 0 && item.matched.is_empty()),
        "an empty stack rescores everything to zero: {rescored:?}"
    );
    assert_eq!(
        titles(&rescored),
        titles(&scored),
        "with nothing to sort by, the order it was given is the order it keeps"
    );
}

/// The matching rule, stated as a test because a substring rule is the plausible mistake:
/// `ai` is a real stack term and "said" is a real headline.
#[test]
fn a_term_matches_whole_words_so_ai_does_not_match_sailboat() {
    let stack = strings(&["AI", "go"]);
    let ranked = rank(
        vec![
            item(
                "HN",
                "Painted sailboat in the harbor",
                "https://example.com/1",
            ),
            item("HN", "Google ships a new compiler", "https://example.com/2"),
            item("HN", "Go 1.25 is released", "https://example.com/3"),
            item("HN", "Said the AI, quietly", "https://example.com/4"),
        ],
        &stack,
    );

    assert_eq!(
        titles(&ranked),
        strings(&[
            "Go 1.25 is released",
            "Said the AI, quietly",
            "Painted sailboat in the harbor",
            "Google ships a new compiler",
        ]),
        "the two hits come first — in the feed's order, since they tie — and the two near-misses keep theirs"
    );
    assert_eq!(
        ranked[0].matched,
        strings(&["go"]),
        "the term is reported with the user's own spelling, not the headline's"
    );
    assert_eq!(ranked[1].matched, strings(&["AI"]));
    assert!(
        ranked[2].matched.is_empty(),
        "`sailboat` is not the term `AI`"
    );
    assert!(
        ranked[3].matched.is_empty(),
        "`Google` is not the term `go`"
    );
}

#[test]
fn a_url_match_counts_once_even_when_the_title_names_the_term_too() {
    let ranked = rank(
        vec![item(
            "Tokio Blog",
            "Tokio 2.0 and the new runtime",
            "https://tokio.rs/blog/2026-02-tokio-2",
        )],
        &strings(&["tokio"]),
    );

    assert_eq!(
        ranked[0].impact, 1,
        "one distinct term is one point, however many times it appears"
    );
    assert_eq!(ranked[0].matched, strings(&["tokio"]));
}

/// The URL is part of the haystack because a project's name often lives only there. The
/// **source's** name is not: it says where the item came from, and matching it would score
/// every headline from a feed whose name happens to contain a stack term.
#[test]
fn a_source_name_alone_is_not_a_match() {
    let ranked = rank(
        vec![item(
            "arXiv cs.SE",
            "A paper about nothing in particular",
            "https://example.org/papers/1",
        )],
        &strings(&["arxiv"]),
    );

    assert_eq!(ranked[0].impact, 0);
    assert!(ranked[0].matched.is_empty());
}

/// The documented tie-break: equal impact comes out in the order the feed gave, which is
/// where the feed's own notion of what is new lives.
#[test]
fn items_with_equal_impact_come_out_in_the_order_the_feed_gave_them() {
    let input = vec![
        item("Blog", "Nothing to do with it", "https://example.com/1"),
        item("Blog", "Rust 1.87", "https://example.com/2"),
        item(
            "Blog",
            "A second unrelated headline",
            "https://example.com/3",
        ),
        item("Blog", "Rust 1.88", "https://example.com/4"),
        item("Blog", "Rust 1.89", "https://example.com/5"),
    ];

    let ranked = rank(input, &strings(&["rust"]));

    assert_eq!(
        titles(&ranked),
        strings(&[
            "Rust 1.87",
            "Rust 1.88",
            "Rust 1.89",
            "Nothing to do with it",
            "A second unrelated headline",
        ]),
        "the three hits keep the feed's order, and so do the two misses"
    );
}

// -------------------------------------------------------------- the failures --

/// `ACCEPTANCE.md` §16: an unreachable source is a stated error, never an empty digest. Both
/// the source and the URL are asserted, because naming only one of them still leaves the
/// reader unable to act.
#[test]
fn an_unreachable_source_is_an_error_naming_the_source_and_the_url() {
    let feed = source("Hacker News", "https://news.ycombinator.com/rss");
    let http = FakeHttp::refusing();

    let error = fetch(&feed, &http).unwrap_err();

    asserts_the_failure_names(&feed, &error);
    assert!(
        error.to_string().contains("refused"),
        "the cause reaches the reader, not only the URL the transport echoed: {error}"
    );
    assert_eq!(error.kind(), "not_reachable");
    assert_eq!(error.exit_code(), 1);
    assert_eq!(
        http.calls(),
        vec![feed.url.clone()],
        "a failed fetch is not retried behind the caller's back"
    );
}

/// A URL pie cannot use is not a network problem, so it keeps its own kind and exit code;
/// "check your connection" is the wrong advice for a typo in the scheme.
#[test]
fn a_bad_url_is_a_config_error_that_still_names_the_source_and_the_url() {
    let feed = source("Vendor changelog", "htp://vendor.example/changelog");
    let error = fetch(&feed, &FakeHttp::bad_url()).unwrap_err();

    asserts_the_failure_names(&feed, &error);
    assert_eq!(error.kind(), "config");
    assert_eq!(error.exit_code(), 2);
}

/// Four ways a body is not news, and none of them is an empty success.
#[test]
fn a_malformed_body_is_a_stated_error_rather_than_a_silently_empty_digest() {
    const HTML: &str = "<!DOCTYPE html>\n<html><head><title>502 Bad Gateway</title></head>\
        <body>nginx</body></html>";

    let cases = [
        ("", "empty body"),
        ("   \n\t ", "empty body"),
        (HTML, "did not answer with a feed"),
        (r#"{"error":"rate limited"}"#, "did not answer with a feed"),
        ("not markup at all", "contains no element at all"),
        ("<<<>>>", "contains no element at all"),
    ];

    let feed = source("Rust Blog", RUST_BLOG_URL);

    for (body, marker) in cases {
        let error = fetch(&feed, &FakeHttp::answering(RUST_BLOG_URL, body)).unwrap_err();
        let message = error.to_string();

        asserts_the_failure_names(&feed, &error);
        assert!(
            message.contains(marker),
            "`{body}` must say what was wrong: {message}"
        );
        assert_eq!(error.kind(), "protocol", "`{body}` -> {message}");
    }

    // The excerpt is the useful half of a not-a-feed failure: it is how the reader sees that
    // a proxy answered instead of the feed.
    let error = fetch(&feed, &FakeHttp::answering(RUST_BLOG_URL, HTML)).unwrap_err();
    assert!(
        error.to_string().contains("502 Bad Gateway"),
        "the body it got is quoted back: {error}"
    );
}

/// The judgement call, stated plainly: a feed that answers and yields no headline is
/// reported, not passed along as an empty digest — "silently nothing" is the outcome
/// `ACCEPTANCE.md` §16 rules out, and the count is what tells a quiet blog from a feed pie
/// cannot read.
#[test]
fn a_feed_with_no_items_is_a_stated_error_not_an_empty_digest() {
    let cases = [
        (
            r#"<rss version="2.0"><channel><title>Quiet blog</title></channel></rss>"#,
            "no <item> or <entry>",
        ),
        (
            r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>Quiet lab</title></feed>"#,
            "no <item> or <entry>",
        ),
        (
            r#"<rss version="2.0"><channel><item><description>a trailer</description></item><item><link>https://example.com/x</link></item></channel></rss>"#,
            "answered with 2 items, none of which carried a title",
        ),
        (
            r#"<rss version="2.0"><channel><item/><item/></channel></rss>"#,
            "answered with 2 items, none of which carried a title",
        ),
    ];

    let feed = source("Rust Blog", RUST_BLOG_URL);

    for (body, marker) in cases {
        let error = fetch(&feed, &FakeHttp::answering(RUST_BLOG_URL, body)).unwrap_err();
        let message = error.to_string();

        asserts_the_failure_names(&feed, &error);
        assert!(
            message.contains(marker),
            "`{body}` must say which shape it was: {message}"
        );
    }
}

// --------------------------------------------------- no panic, and no hang --

/// A multi-byte character at every offset a byte-indexed scanner would slice through:
/// before a tag, inside a headline, at the very end of a headline, and in a URL. Invariant
/// I6 makes a panic here a failure, and this is external input.
#[test]
fn a_multibyte_headline_survives_the_parser() {
    const FEED: &str = "<rss version=\"2.0\"><channel>\
        日本語<title>unused</title>\
        <item><title>日本語のタイトル 🚀</title><link>https://例え.jp/記事/1</link></item>\
        <item><title>Ünïcødé: ﬁne</title><link>https://example.com/ünïcode</link></item>\
        <item><title>日本語</title><link href=\"https://example.com/attr\"/></item>\
        </channel></rss>";

    let feed = source("Babel", "https://example.com/babel");
    let items = fetch(&feed, &FakeHttp::answering(&feed.url, FEED)).unwrap();

    assert_eq!(
        items.len(),
        3,
        "the channel's own title is not a headline: {items:?}"
    );
    assert_eq!(items[0].title, "日本語のタイトル 🚀");
    assert_eq!(items[0].url, "https://例え.jp/記事/1");
    assert_eq!(items[1].title, "Ünïcødé: ﬁne");
    assert_eq!(items[2].title, "日本語");
}

/// Every truncation here cuts a tag, an entity or a character in half. None may panic, and
/// none may come back as a successful empty digest — a body that stopped arriving is a
/// failure, and one item that survived is still news.
#[test]
fn a_body_cut_off_mid_character_or_mid_entity_is_an_error_and_never_an_empty_success() {
    let bodies = [
        "<rss version=\"2.0\"><channel><item><title>日本語",
        "<rss version=\"2.0\"><channel><item><title>abc</ti",
        "<rss version=\"2.0\"><channel><item><title>日",
        "<rss",
        "<title>日本語</",
        "<item title=\"日本語\"",
        "<!-- 日本語",
        "<![CDATA[日本語",
        "&",
        "&#",
        "&#x",
        "&#x110000;",
        "&#a日日日日日日;",
        "<item><title>&#x110000;</title>",
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><rss version=\"2.0\"><channel>\
         <item><title>whole</title><link>https://example.com/1</link></item></channel></rss>",
    ];

    let feed = source("Truncated", "https://example.com/truncated");

    for body in bodies {
        match fetch(&feed, &FakeHttp::answering(&feed.url, body)) {
            Ok(items) => assert!(
                !items.is_empty(),
                "`{body}` came back as a successful empty digest"
            ),
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains(&feed.url),
                    "`{body}` must name the URL: {message}"
                );
                assert!(!message.trim().is_empty());
            }
        }
    }

    // The one whole document in that list parses, which is what makes the rest meaningful:
    // these bodies fail because they are cut off, not because the parser refuses everything.
    let whole = source("Whole", "https://example.com/whole");
    let survived = fetch(
        &whole,
        &FakeHttp::answering(
            &whole.url,
            "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><item><title>whole</title>\
             <link>https://example.com/1</link></item></channel></rss>",
        ),
    )
    .unwrap();
    assert_eq!(survived.len(), 1);
    assert_eq!(survived[0].title, "whole");
}

/// The adversarial version of the test above: rather than hand-picking awkward offsets, cut
/// the fixture at **every** character boundary, and separately insert a multi-byte character
/// at every boundary. Every prefix is a body a proxy or a dropped connection could hand us,
/// and the assertion is the same one that matters — no panic, and never a successful empty
/// digest. Hand-picked cases only cover the offsets someone thought of.
#[test]
fn every_truncation_of_a_real_feed_is_survived_and_none_of_them_is_an_empty_success() {
    let feed = source("Rust Blog", RUST_BLOG_URL);

    for body in [RSS_BLOG, ATOM_ARXIV] {
        let cuts = body
            .char_indices()
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert!(
            cuts.len() > 100,
            "the fixture must be big enough to sweep: {}",
            cuts.len()
        );

        for cut in cuts {
            for candidate in [body[..cut].to_string(), insert_accent(body, cut)] {
                match fetch(&feed, &FakeHttp::answering(RUST_BLOG_URL, &candidate)) {
                    Ok(items) => assert!(
                        !items.is_empty(),
                        "a cut of {cut} bytes came back as a successful empty digest"
                    ),
                    Err(error) => assert!(
                        error.to_string().contains(RUST_BLOG_URL),
                        "a cut of {cut} bytes failed without naming the URL: {error}"
                    ),
                }
            }
        }
    }
}

/// `body` with `日` inserted at character boundary `cut`.
fn insert_accent(body: &str, cut: usize) -> String {
    let (head, tail) = body.split_at(cut);
    format!("{head}日{tail}")
}

/// Bounded work on hostile input: an unterminated `&#` must not send the entity decoder to
/// the end of the body once per occurrence, which would make a large body quadratic.
#[test]
fn a_body_full_of_unterminated_entities_neither_hangs_nor_panics() {
    let body = format!("{}<title>end</title>", "&#x".repeat(20_000));
    let feed = source("Junk", "https://example.com/junk");

    let error = fetch(&feed, &FakeHttp::answering(&feed.url, &body)).unwrap_err();
    assert!(error.to_string().contains(&feed.url));
}

#[test]
fn a_large_feed_is_read_without_a_cap_or_a_hang() {
    let mut body = String::from("<rss version=\"2.0\"><channel>");
    for index in 0..500 {
        body.push_str(&format!(
            "<item><title>Headline {index} about rust</title><link>https://example.com/{index}</link></item>"
        ));
    }
    body.push_str("</channel></rss>");

    let feed = source("Big Feed", "https://example.com/big");
    let items = fetch(&feed, &FakeHttp::answering(&feed.url, &body)).unwrap();

    assert_eq!(
        items.len(),
        500,
        "every headline is kept: deciding what matters is `rank`'s job, not the parser's"
    );

    let ranked = rank(items, &strings(&["rust"]));
    assert_eq!(ranked.len(), 500);
    assert!(ranked.iter().all(|item| item.impact == 1));
}

// ------------------------------------------------------ no transport, no spawn --

/// The claim that makes a *scheduled* digest safe: this module reaches the network only
/// through the injected `Http`, and spawns nothing — so it cannot spend model tokens, and a
/// gate that runs it can never make a live request. Static, because the property is about
/// what the source can do at all, not about what one test path happened to do.
#[test]
fn the_news_module_has_no_transport_of_its_own_and_spawns_nothing() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("watch")
        .join("news.rs");
    let text = std::fs::read_to_string(&path).expect("read src/watch/news.rs");

    let forbidden = [
        "reqwest",
        "Command::new",
        "std::net",
        "TcpStream",
        "process::stream",
        "process::run",
        ".spawn(",
    ];

    let mut offenders = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for token in forbidden {
            if line.contains(token) {
                offenders.push(format!("{}:{} {token}", index + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "src/watch/news.rs must fetch only through `Http` and spawn nothing, but it names: \
         {offenders:?}"
    );
}
