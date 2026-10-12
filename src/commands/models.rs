//! `pie models` — the model catalog for one backend or all of them.
//!
//! Catalogs come from the CLI's own listing command and are cached on disk
//! (`cache.rs`), so `--refresh` is the only path that spends a subprocess.

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::provider::{self, Provider};

/// `pie models [--backend B] [--refresh]`
pub async fn run(cfg: &Config, backend: Option<&str>, refresh: bool) -> Result<()> {
    let targets = select(cfg, backend)?;

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

    if refresh {
        println!("note: --refresh applies to backends with a cached catalog.");
    }

    let _ = cfg;
    Ok(())
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
pub(crate) fn known_backends() -> Vec<String> {
    provider::BACKEND_IDS
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
        assert_eq!(select(&cfg, None).unwrap().len(), 3);
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
}
