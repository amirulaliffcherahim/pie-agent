# Project Guidelines

## Git & Formatting
- Always run `cargo fmt` before committing any code changes.

## Code Quality & Safety
- Never write `.unwrap()` in production code. Handle errors gracefully using `?`, pattern matching, or fallbacks (`unwrap_or`, `unwrap_or_else`, etc.).
- Always run `cargo clippy` (e.g. `cargo clippy --all-targets`) to ensure clean and lint-free code.
