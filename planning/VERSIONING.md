# Versioning

pie uses Semantic Versioning, starting at **0.0.0**.

| Change | Bump | Example |
|---|---|---|
| Bug fix | PATCH | 0.0.0 → 0.0.1 |
| New feature (backward-compatible) | MINOR | 0.0.1 → 0.1.0 |
| Breaking change / stable release | MAJOR | 0.x → 1.0.0 |

Rules:

- `Cargo.toml` starts at `version = "0.0.0"` at scaffold.
- `fix:` commit → PATCH · `feat:` commit → MINOR.
- **Never go to 1.0.0 without explicit consent.** 1.0.0 is a deliberate gate, not
  an automatic bump.
- During 0.x, breaking changes may still bump MINOR (pre-release semantics); a
  jump to 1.0.0 is the only change that requires approval.
- Maintain `CHANGELOG.md` (Keep a Changelog) — one `[Unreleased]` block updated
  as work lands, promoted on each tagged release.
- Tag each release `v0.0.0`, `v0.0.1`, … with the bump recorded in the changelog.
