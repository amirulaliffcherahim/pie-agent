//! `pie ocr <file>` — the manual form of the OCR capability.
//!
//! Local I/O plus one HTTP request to itsmini. No vendor CLI is spawned, no model
//! quota is spent, and no session row is written: there is nothing here that could
//! do any of the three, and `tests/ocr.rs` asserts it — including a static check
//! that this file has no path to a spawn and no path to a transcript write.
//!
//! stdout stays a protocol channel (`logging.rs`): human mode prints the text and
//! nothing else, so `pie ocr scan.png > scan.txt` is the whole story and no note
//! can be piped into the file by accident.

use crate::out::{outfrag, outln};
use std::path::Path;

use crate::error::Result;
use crate::ocr::{self, Request};

/// `pie ocr <file> [--lang CODE] [--output FORMAT] [--refresh]`
///
/// `--json` is the top-level flag, so the JSON form is `pie --json ocr <file>`:
/// the structured result includes whether the answer came from the cache.
pub async fn run(
    file: &Path,
    lang: Option<&str>,
    output: Option<&str>,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let result = ocr::convert(Request {
        file,
        lang,
        output,
        refresh,
        // Deliberate: the base URL is read from the environment at call time and
        // the cache lives under `$PIE_HOME/ocr-cache/`. `pie ocr` overrides
        // neither — those overrides exist so tests need not mutate the process
        // environment.
        base_url: None,
        cache_dir: None,
    })
    .await?;

    if json {
        // One JSON object per line, like every other `--json` path (`DESIGN.md`
        // §12), so a caller can parse the result without a second format.
        outln!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }

    outfrag!("{}", result.text);
    if !result.text.ends_with('\n') {
        outln!();
    }
    Ok(())
}
