//! `pie mcp ocr` — the MCP face of the OCR capability.
//!
//! Thin on purpose. Everything that decides anything lives in [`crate::mcp`], and
//! everything that talks to itsmini lives in [`crate::ocr`]; this file exists so
//! the CLI surface has the same one-file-per-command shape as the rest of
//! `commands/`.
//!
//! Two things it must not do. It must not spawn a vendor CLI — the whole point of
//! the server is that the *vendor* calls *pie*, not the other way round — and it
//! must not print to stdout: from the moment the server starts, stdout carries
//! JSON-RPC frames and nothing else (`DESIGN.md` §6, `DESIGN.md` §11).

use crate::error::Result;
use crate::mcp;

/// `pie mcp <CAPABILITY>` — run the named capability's server on stdin/stdout.
///
/// The capability is validated by [`mcp::serve`], so an unknown one is a config
/// error (exit 2) naming what pie does host rather than a server that waits
/// forever on a client that will never get an answer.
pub async fn run(capability: &str) -> Result<()> {
    mcp::serve(capability).await
}
