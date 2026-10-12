//! The profile projection: one persona across all three CLIs (M3).
//!
//! `PLAN.md` §8 and `DESIGN.md` §10: `$PIE_HOME/profile.toml` + `AGENTS.md` +
//! `memory/*.md` are the single source of truth, and each backend is handed the
//! *same bytes*. The vendors' own configuration files — and their own
//! `AGENTS.md`/`CLAUDE.md` conventions — are deliberately never read and never
//! written: teaching one CLI a persona outside pie is exactly what "no per-CLI
//! config" forbids. `tests/profile.rs` enforces that with a static scan.
//!
//! Two properties are load-bearing, and both are enforced by tests:
//!
//! * **Determinism.** The projection is assembled in a fixed order and contains
//!   no volatile value — no timestamp, session id, cwd, model id or quota
//!   figure. This is a caching requirement rather than tidiness
//!   (`TOKEN-ECONOMICS.md` §4): a volatile head invalidates whatever stable
//!   prefix the vendor caches, on every single invocation.
//! * **Completeness or failure.** A projection is never truncated to fit an
//!   argv cap. A persona that does not fit is an error naming the projection
//!   (`DESIGN.md` §10), because silently dropping half a persona changes what
//!   the model was told while looking like a successful run.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{PieError, Result};
use crate::paths;

/// Blank line between two projection sections.
pub const SECTION_SEPARATOR: &str = "\n\n";

/// Separator between the projection and the caller's task text.
///
/// This is part of the payload contract, not decoration: agy and codex have no
/// usable system-prompt flag, so both receive the persona **folded in front of**
/// the prompt (`DESIGN.md` §10) — and both must fold with [`fold`], or the two
/// payloads stop being byte-identical.
pub const TASK_SEPARATOR: &str = "\n---\n";

/// Heading of the structured persona rendered from `profile.toml`.
pub const PERSONA_HEADING: &str = "# Persona";

/// Heading of the `rules` list.
pub const RULES_HEADING: &str = "# Rules";

/// Heading of the output conventions list.
pub const OUTPUT_HEADING: &str = "# Output";

/// File extension a memory file must have to be included.
pub const MEMORY_EXTENSION: &str = "md";

/// The structured persona from `profile.toml`.
///
/// Every field is optional: a partial file contributes the parts it sets, and
/// an unknown key is ignored rather than fatal, so M7's "know me" job can add
/// fields without breaking an older pie.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Persona {
    /// Persona name, e.g. `pie`.
    pub name: Option<String>,
    /// Tone instruction, e.g. `terse`.
    pub tone: Option<String>,
    /// Behavioural rules, one per entry.
    pub rules: Vec<String>,
    /// Output conventions, one per entry.
    pub output: Vec<String>,
}

impl Persona {
    /// Render to the fixed projection text. Empty when nothing is set.
    ///
    /// Blank entries are dropped, so a stray `""` in the list cannot turn into
    /// an empty bullet that changes the bytes on every edit.
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        let mut head: Vec<String> = Vec::new();
        if let Some(name) = non_empty(self.name.as_deref()) {
            head.push(format!("name: {name}"));
        }
        if let Some(tone) = non_empty(self.tone.as_deref()) {
            head.push(format!("tone: {tone}"));
        }
        if !head.is_empty() {
            parts.push(format!("{PERSONA_HEADING}\n{}", head.join("\n")));
        }

        for (heading, items) in [(RULES_HEADING, &self.rules), (OUTPUT_HEADING, &self.output)] {
            if let Some(block) = bullets(heading, items) {
                parts.push(block);
            }
        }

        parts.join(SECTION_SEPARATOR)
    }

    /// Whether this persona contributes nothing at all.
    pub fn is_empty(&self) -> bool {
        self.render().is_empty()
    }
}

/// Which kind of source a contribution came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// `profile.toml`, the structured persona.
    Profile,
    /// `AGENTS.md`, freeform rules.
    Agents,
    /// One file from `memory/`.
    Memory,
}

impl SourceKind {
    /// Short label for the provenance table in `pie profile`.
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Profile => paths::PROFILE_FILE,
            SourceKind::Agents => paths::AGENTS_FILE,
            SourceKind::Memory => paths::MEMORY_DIR,
        }
    }
}

/// Where one part of the projection came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contribution {
    /// Which of the three source kinds this is.
    pub kind: SourceKind,
    /// The file it was read from.
    pub source: PathBuf,
    /// Bytes this source contributed to the projection, after normalisation.
    /// (Not the file's size: `profile.toml` is rendered, not copied.)
    pub bytes: usize,
}

/// The assembled persona projection, plus the provenance of every part.
///
/// `root` is a parameter rather than a hidden call to [`paths::pie_home`] so a
/// test can build a profile in a temp directory, and so a cwd can never leak
/// into the projection.
#[derive(Debug, Clone)]
pub struct Profile {
    root: PathBuf,
    projection: String,
    contributions: Vec<Contribution>,
}

impl Profile {
    /// Load and assemble every source under `root`.
    ///
    /// All three sources are optional and a missing file or directory is an
    /// empty contribution, never an error: a fresh install with no
    /// configuration at all must work (`DESIGN.md` §10). A *malformed* file is
    /// a config error (exit 2), because a persona pie silently ignores is worse
    /// than one that refuses to load.
    pub fn load(root: &Path) -> Result<Self> {
        let mut contributions: Vec<Contribution> = Vec::new();
        let mut sections: Vec<String> = Vec::new();

        // 1. the structured persona
        let persona_path = root.join(paths::PROFILE_FILE);
        if let Some(text) = read_optional(&persona_path)? {
            let persona: Persona = toml::from_str(&text)
                .map_err(|e| PieError::Config(format!("{}: {e}", persona_path.display())))?;
            push(
                &mut sections,
                &mut contributions,
                SourceKind::Profile,
                &persona_path,
                &persona.render(),
            );
        }

        // 2. AGENTS.md, verbatim
        let agents_path = root.join(paths::AGENTS_FILE);
        if let Some(text) = read_optional(&agents_path)? {
            push(
                &mut sections,
                &mut contributions,
                SourceKind::Agents,
                &agents_path,
                &text,
            );
        }

        // 3. memory/*.md, in filename order
        for path in memory_files(root)? {
            if let Some(text) = read_optional(&path)? {
                push(
                    &mut sections,
                    &mut contributions,
                    SourceKind::Memory,
                    &path,
                    &text,
                );
            }
        }

        Ok(Self {
            root: root.to_path_buf(),
            projection: sections.join(SECTION_SEPARATOR),
            contributions,
        })
    }

    /// Load from `$PIE_HOME`.
    pub fn from_home() -> Result<Self> {
        Self::load(&paths::pie_home())
    }

    /// The directory the sources were loaded from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The assembled projection, or `""` when nothing is configured.
    pub fn projection(&self) -> &str {
        &self.projection
    }

    /// Projection size in bytes.
    pub fn bytes(&self) -> usize {
        self.projection.len()
    }

    /// Whether the user configured no persona at all.
    pub fn is_empty(&self) -> bool {
        self.projection.is_empty()
    }

    /// The persona to send to a backend, or `None` when there is none.
    ///
    /// `None` rather than `Some("")` on purpose: no backend may receive a
    /// persona the user did not configure, and an empty string would still be
    /// folded into the prompt and change the payload.
    pub fn persona(&self) -> Option<&str> {
        if self.is_empty() {
            None
        } else {
            Some(&self.projection)
        }
    }

    /// Provenance of each contribution, in projection order.
    pub fn contributions(&self) -> &[Contribution] {
        &self.contributions
    }

    /// Fold this projection in front of a task text (agy and codex).
    pub fn fold(&self, task: &str) -> String {
        fold(self.persona(), task)
    }
}

/// Fold a projection in front of the task text.
///
/// The single implementation of the folding rule, so agy's payload and codex's
/// payload cannot drift apart (`DESIGN.md` §10). A `None` or blank projection
/// leaves the task text untouched.
pub fn fold(persona: Option<&str>, task: &str) -> String {
    match persona.filter(|p| !p.trim().is_empty()) {
        Some(persona) => format!("{persona}{TASK_SEPARATOR}{task}"),
        None => task.to_string(),
    }
}

/// Whether a projection would be folded into a prompt for this persona.
pub fn has_persona(persona: Option<&str>) -> bool {
    persona.is_some_and(|p| !p.trim().is_empty())
}

/// Render a bullet list under `heading`, or `None` when it has no entries.
fn bullets(heading: &str, items: &[String]) -> Option<String> {
    let lines: Vec<String> = items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| format!("- {item}"))
        .collect();

    if lines.is_empty() {
        None
    } else {
        Some(format!("{heading}\n{}", lines.join("\n")))
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Append one non-empty section, recording its provenance.
///
/// The section is normalised (line endings) and right-trimmed so that joining
/// is stable: a file with a trailing newline and the same file without one must
/// not produce two different projections.
fn push(
    sections: &mut Vec<String>,
    contributions: &mut Vec<Contribution>,
    kind: SourceKind,
    source: &Path,
    text: &str,
) {
    let normalized = normalize(text);
    let trimmed = normalized.trim_end();
    if trimmed.is_empty() {
        return;
    }

    contributions.push(Contribution {
        kind,
        source: source.to_path_buf(),
        bytes: trimmed.len(),
    });
    sections.push(trimmed.to_string());
}

/// CRLF is a checkout artefact, not content.
///
/// Normalising keeps the projection byte-identical whether the files were
/// written on Windows or checked out on Linux, which is what the vendor prefix
/// cache needs (`TOKEN-ECONOMICS.md` §4).
fn normalize(text: &str) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_string()
    }
}

/// `memory/*.md`, sorted by **file name**.
///
/// `read_dir` order is filesystem-dependent, so concatenating in enumeration
/// order would make the projection non-deterministic — and a non-deterministic
/// prefix is exactly what destroys the vendor's cache. Creation order is
/// deliberately irrelevant: the name is the only ordering that survives a
/// copy, a restore, or a different filesystem.
fn memory_files(root: &Path) -> Result<Vec<PathBuf>> {
    let dir = root.join(paths::MEMORY_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // A missing `memory/` is an empty contribution, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(unreadable(&dir, &e)),
    };

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| unreadable(&dir, &e))?;
        let path = entry.path();
        if !path.is_file() || !is_markdown(&path) {
            continue;
        }
        files.push((entry.file_name().to_string_lossy().to_string(), path));
    }

    files.sort();
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case(MEMORY_EXTENSION))
}

/// Read a file that is allowed not to exist.
fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(unreadable(path, &e)),
    }
}

fn unreadable(path: &Path, error: &std::io::Error) -> PieError {
    PieError::Config(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn persona_renders_in_a_fixed_order() {
        let persona = Persona {
            name: Some("  pie ".into()),
            tone: Some("terse".into()),
            rules: vec!["one".into(), "  ".into(), " two ".into()],
            output: vec!["plain text".into()],
        };

        assert_eq!(
            persona.render(),
            "# Persona\nname: pie\ntone: terse\n\n# Rules\n- one\n- two\n\n# Output\n- plain text"
        );
    }

    #[test]
    fn an_empty_persona_renders_to_nothing() {
        assert!(Persona::default().render().is_empty());
        assert!(Persona {
            name: Some("  ".into()),
            rules: vec![String::new()],
            ..Persona::default()
        }
        .is_empty());
    }

    #[test]
    fn a_missing_root_is_an_empty_projection() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let profile = Profile::load(&missing).unwrap();
        assert!(profile.is_empty());
        assert_eq!(profile.persona(), None);
        assert!(profile.contributions().is_empty());
        assert_eq!(profile.bytes(), 0);
    }

    #[test]
    fn an_empty_profile_toml_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), paths::PROFILE_FILE, "");
        let profile = Profile::load(dir.path()).unwrap();
        assert!(profile.is_empty());
        assert!(profile.contributions().is_empty());
    }

    #[test]
    fn malformed_persona_toml_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), paths::PROFILE_FILE, "name = = ");
        let err = Profile::load(dir.path()).unwrap_err();
        assert!(matches!(err, PieError::Config(_)));
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("profile.toml"));
    }

    #[test]
    fn memory_order_is_filename_order_regardless_of_creation_order() {
        let dir = tempfile::tempdir().unwrap();
        // Created in the opposite order to the sorted one on purpose: the test
        // must fail if enumeration order is ever used instead of the name.
        write(dir.path(), "memory/zeta.md", "ZETA");
        write(dir.path(), "memory/alpha.md", "ALPHA");
        write(dir.path(), "memory/middle.md", "MIDDLE");
        // Not memory: wrong extension, and a directory.
        write(dir.path(), "memory/notes.txt", "TXT");
        std::fs::create_dir_all(dir.path().join("memory").join("nested")).unwrap();

        let profile = Profile::load(dir.path()).unwrap();
        let projection = profile.projection();
        let alpha = projection.find("ALPHA").unwrap();
        let middle = projection.find("MIDDLE").unwrap();
        let zeta = projection.find("ZETA").unwrap();

        assert!(alpha < middle && middle < zeta, "{projection}");
        assert!(!projection.contains("TXT"), "only *.md files are memory");
        assert_eq!(profile.contributions().len(), 3);
        assert!(profile
            .contributions()
            .iter()
            .all(|c| c.kind == SourceKind::Memory));
    }

    #[test]
    fn a_section_is_recorded_only_when_it_contributes_text() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), paths::PROFILE_FILE, "name = \"pie\"\n");
        write(dir.path(), paths::AGENTS_FILE, "\n\n");
        write(dir.path(), "memory/blank.md", "  \n");

        let profile = Profile::load(dir.path()).unwrap();
        assert_eq!(profile.contributions().len(), 1);
        assert_eq!(profile.contributions()[0].kind, SourceKind::Profile);
        assert_eq!(profile.contributions()[0].bytes, profile.bytes());
        assert_eq!(
            profile.contributions()[0].source,
            dir.path().join("profile.toml")
        );
    }

    #[test]
    fn crlf_and_trailing_newlines_do_not_change_the_bytes() {
        let unix = tempfile::tempdir().unwrap();
        write(unix.path(), paths::AGENTS_FILE, "rule one\nrule two\n");

        let windows = tempfile::tempdir().unwrap();
        write(
            windows.path(),
            paths::AGENTS_FILE,
            "rule one\r\nrule two\r\n\r\n",
        );

        assert_eq!(
            Profile::load(unix.path()).unwrap().projection(),
            Profile::load(windows.path()).unwrap().projection()
        );
    }

    #[test]
    fn the_projection_never_contains_a_filesystem_path() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), paths::PROFILE_FILE, "tone = \"terse\"\n");
        let profile = Profile::load(dir.path()).unwrap();

        // The root is a real temp path, so this is a meaningful assertion even
        // though it is cheap: nothing but the sources' *content* is projected.
        let root = dir.path().display().to_string();
        assert!(!profile.projection().contains(&root));
        assert_eq!(profile.projection(), "# Persona\ntone: terse");
    }

    #[test]
    fn fold_is_the_single_folding_rule() {
        let task = "summarise this repo";
        assert_eq!(
            fold(Some("# Persona\ntone: terse"), task),
            "# Persona\ntone: terse\n---\nsummarise this repo"
        );
        assert_eq!(fold(None, task), task);
        assert_eq!(fold(Some("   "), task), task);
        assert!(!has_persona(None));
        assert!(!has_persona(Some("\n")));
        assert!(has_persona(Some("x")));
    }
}
