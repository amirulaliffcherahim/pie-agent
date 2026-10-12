//! `pie models` — the model catalog for one backend or all of them.
//!
//! Catalogs come from the CLI's own listing command and are cached on disk
//! (`cache.rs`), so `--refresh` is the only path that spends a subprocess.

use std::path::PathBuf;

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::paths;
use crate::provider::{self, Provider};

/// `pie models [--backend B] [--refresh]`
pub async fn run(cfg: &Config, backend: Option<&str>, refresh: bool) -> Result<()> {
    let targets = select(cfg, backend)?;

    if refresh {
        let evicted = evict_model_caches(&targets);
        if evicted.is_empty() {
            println!("note: nothing cached to refresh for this selection.");
        } else {
            for path in evicted {
                println!("refreshed: {path}");
            }
        }
    }

    for p in targets {
        let id = p.id();

        match p.list_models(cfg).await {
            Ok(models) if models.is_empty() => println!("{id}: no models reported"),
            Ok(models) => {
                println!("{id}:");
                for model in models {
                    let efforts = if model.efforts.is_empty() {
                        String::new()
                    } else {
                        format!("  efforts: {}", model.efforts.join("|"))
                    };
                    println!("  {:<28} {}{}", model.id, model.display, efforts);
                }
            }
            // A backend that has not implemented its catalog yet says so
            // instead of failing the whole command.
            Err(PieError::Unsupported { feature, .. }) => {
                println!("{id}: not implemented yet — {feature}");
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Cache files a `--refresh` should invalidate, per backend id.
///
/// A backend with no cache (claude, whose catalog is static) contributes
/// nothing, so `--refresh` is simply a no-op for it rather than an error.
fn model_cache_paths(id: &str) -> Vec<PathBuf> {
    match id {
        "agy" => vec![paths::agy_model_cache()],
        "codex" => vec![paths::codex_model_cache()],
        _ => Vec::new(),
    }
}

/// `--refresh` is implemented by **evicting the on-disk cache**, not by adding a
/// parameter to `Provider::list_models`.
///
/// The provider's next call then misses and refetches. This keeps the trait free
/// of CLI concerns (`--refresh` is a flag, not a capability), and it means a
/// backend gains correct `--refresh` behaviour for free as soon as it caches
/// anything at the path named above.
///
/// Returns the paths actually removed, so the user can see what was refreshed
/// instead of having to trust that something happened.
fn evict_model_caches(targets: &[Box<dyn Provider>]) -> Vec<String> {
    let mut evicted = Vec::new();

    for p in targets {
        for path in model_cache_paths(p.id()) {
            match std::fs::remove_file(&path) {
                Ok(()) => evicted.push(path.display().to_string()),
                // Nothing cached is the normal case on a first run.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // A cache we cannot evict must not fail the command: results
                // just come from the stale cache, which is what was already
                // going to happen.
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "could not evict model cache")
                }
            }
        }
    }

    evicted
}

/// The backends to list, or an `UnknownBackend` error naming the valid ids.
fn select(_cfg: &Config, backend: Option<&str>) -> Result<Vec<Box<dyn Provider>>> {
    match backend {
        None => Ok(provider::registry()),
        Some(id) => {
            let provider = provider::by_id(id).ok_or_else(|| PieError::UnknownBackend {
                backend: id.to_string(),
                known: known_backends(),
            })?;
            Ok(vec![provider])
        }
    }
}

/// Known backend ids, for error messages.
///
/// [`provider::backend_ids`] rather than `BACKEND_IDS`: an error that lists the known
/// backends must list the ones a user can actually name, and since M10 that includes the
/// two API-key backends. The CLI-only constant is for code that means "the subprocess
/// backends", which this is not.
pub(crate) fn known_backends() -> Vec<String> {
    provider::backend_ids()
        .iter()
        .map(|id| (*id).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn no_backend_means_all_of_them() {
        let cfg = Config::default();
        // Five since M10: the three CLI backends plus OpenRouter and DeepSeek. Derived from
        // the registry rather than hardcoded, so a sixth backend does not have to be
        // remembered here as well.
        assert_eq!(
            select(&cfg, None).unwrap().len(),
            crate::provider::backend_ids().len()
        );
        assert_eq!(select(&cfg, None).unwrap().len(), 5);
    }

    #[test]
    fn an_unknown_backend_names_the_valid_ones() {
        let cfg = Config::default();

        // `Box<dyn Provider>` is not Debug, so match instead of `unwrap_err`.
        match select(&cfg, Some("gemini")) {
            Err(PieError::UnknownBackend { backend, known }) => {
                assert_eq!(backend, "gemini");
                assert!(known.contains(&"agy".to_string()));
            }
            Err(other) => panic!("wrong error: {other:?}"),
            Ok(_) => panic!("an unknown backend must not resolve"),
        }
    }

    #[test]
    fn cache_paths_are_named_for_backends_that_cache() {
        assert_eq!(model_cache_paths("agy").len(), 1);
        assert_eq!(model_cache_paths("codex").len(), 1);
        // claude's catalog is static, so it has nothing to refresh.
        assert!(model_cache_paths("claude").is_empty());
    }

    /// `--refresh` must actually make the next `list_models` miss the cache,
    /// and must be a harmless no-op when nothing is cached yet.
    #[test]
    fn refresh_evicts_the_cache_and_tolerates_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let _env = crate::testing::EnvGuard::set(&[("PIE_HOME", Some(home.as_str()))]);

        let cache = paths::agy_model_cache();
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, "{\"fetched_at\":1,\"value\":[]}").unwrap();
        assert!(cache.is_file());

        // Missing cache first: must not error, must report nothing.
        let empty = evict_model_caches(&select(&Config::default(), Some("claude")).unwrap());
        assert!(empty.is_empty(), "claude has no cache: {empty:?}");

        let evicted = evict_model_caches(&select(&Config::default(), Some("agy")).unwrap());
        assert_eq!(evicted.len(), 1, "expected the agy cache to be evicted");
        assert!(!cache.exists(), "the cache file should be gone");

        // A second refresh finds nothing, and still does not error.
        let again = evict_model_caches(&select(&Config::default(), Some("agy")).unwrap());
        assert!(again.is_empty());
    }
}
