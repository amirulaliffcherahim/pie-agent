# Deferred backends — OpenRouter and DeepSeek (M10)

`PLAN.md` §1 keeps an API-key fallback pair in the design. They are **not**
implemented yet; this document is where their design lives.

They are deliberately **not** shipped as commented-out `.rs` files. A source file
containing nothing but comments is not compiled, not linted, not tested, and
rots silently — a reviewer cannot tell whether it is current. Keeping the design
in a document makes re-enabling an explicit, reviewable act.

## Why these two are different from agy / codex / claude

| | agy · codex · claude | openrouter · deepseek |
|---|---|---|
| Credential | the CLI's own subscription login | plain API key from the environment |
| Env scrub | key variables **stripped** from the child | key variable **read** by pie itself |
| API-key-only credential | refused (`NoSubscription`) | expected |
| Tool loop | no — delegate mode only | yes — `tool_calls` over SSE |
| Added in | M0–M2 / M9 | M10 |

So these are the only backends that may read an API key, and they are the only
reason `src/error.rs` still carries a `Http` variant behind the `http` feature.

## Re-enabling, step by step

1. Write `src/provider/openrouter.rs` (and/or `deepseek.rs`) using the skeleton
   below. Implement `Provider` exactly as the other backends do.
2. Register it in `src/provider.rs`:
   - add `pub mod openrouter;` next to the other `pub mod` lines,
   - add `Box::new(openrouter::OpenRouter),` to `registry()`,
   - add its id to `BACKEND_IDS`.
3. Add `[[provider]]`-style config in `src/config.rs` if it needs settings, and
   update `planning/DESIGN.md` §8.
4. Extend `planning/ACCEPTANCE.md` with an M10 section **before** writing code.
5. Keep the key out of the scrub deny-list — but add a test proving the key is
   never logged. `tests/no_secrets.rs` covers that automatically for every
   backend, because it scans the whole tree.

## Skeleton

OpenAI-compatible chat completions over SSE, with the tool loop that
`RISKS.md` §8 flags as the part delegate-mode CLIs never exercise.

```rust
use std::path::PathBuf;

use async_trait::async_trait;

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::provider::{
    AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, TurnOutcome, UsageReport,
};

/// Environment variable holding this backend's API key.
pub const KEY_ENV: &str = "OPENROUTER_API_KEY";

/// OpenAI-compatible base URL.
pub const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter backend. DeepSeek is identical apart from the two constants.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenRouter;

#[async_trait]
impl Provider for OpenRouter {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // The one place `tool_loop` is legitimately true.
            tool_loop: true,
            resume: false,
            stdin_prompt: false,
            max_prompt_argv: 0,
        }
    }

    fn binary(&self, _cfg: &Config) -> Option<PathBuf> {
        // Not a subprocess backend.
        None
    }

    async fn auth_status(&self, _cfg: &Config) -> AuthState {
        if std::env::var(KEY_ENV).is_ok_and(|v| !v.trim().is_empty()) {
            AuthState::Ready
        } else {
            AuthState::NeedsLogin {
                hint: format!("set {KEY_ENV}"),
            }
        }
    }

    async fn list_models(&self, _cfg: &Config) -> Result<Vec<Model>> {
        // GET {BASE_URL}/models
        Err(PieError::Unsupported {
            backend: "openrouter".into(),
            feature: "model catalog (M10)".into(),
        })
    }

    async fn stream(
        &self,
        _cfg: &Config,
        _req: &ChatRequest,
        _sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome> {
        // POST {BASE_URL}/chat/completions with `stream: true`, then parse SSE
        // `data:` frames and map `choices[].delta` + `tool_calls` into
        // StreamEvent. Reuse `crate::provider::driver` for the argv cap only —
        // this backend does not spawn anything.
        Err(PieError::Unsupported {
            backend: "openrouter".into(),
            feature: "SSE streaming and the tool loop (M10)".into(),
        })
    }

    async fn usage(&self, _cfg: &Config) -> Result<UsageReport> {
        Err(PieError::Unsupported {
            backend: "openrouter".into(),
            feature: "quota probe (M10)".into(),
        })
    }
}
```

## Non-negotiables when this is built

- The key is read from the environment **at call time**, never from
  `config.toml`, never written to `~/.pie`, never logged.
- `usage.jsonl` rows for these backends carry non-zero `cost_usd_micros`, unlike
  the subscription backends, which always report `0`.
- `tests/no_secrets.rs` must still pass: no key material in the tree, and no key
  material in any output or file that pie writes.
