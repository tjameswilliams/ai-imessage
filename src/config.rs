//! TOML configuration with full defaults — the tool must work with no
//! config file at all.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub source: SourceConfig,
    pub index: IndexConfig,
    pub embeddings: EmbeddingsConfig,
    pub service: ServiceConfig,
    pub retrieval: RetrievalConfig,
    pub privacy: PrivacyConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceConfig {
    pub database_path: String,
    pub recent_overlap_rows: u32,
    /// macOS Contacts store used to resolve handles to names. Set to ""
    /// to disable contact-name resolution entirely.
    pub contacts_path: String,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            database_path: "~/Library/Messages/chat.db".into(),
            recent_overlap_rows: 5000,
            contacts_path: "~/Library/Application Support/AddressBook".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    pub database_path: String,
    pub chunk_gap_minutes: u32,
    pub chunk_target_tokens: u32,
    pub chunk_overlap_messages: u32,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            database_path: "~/Library/Application Support/ai-imessage/index.sqlite".into(),
            chunk_gap_minutes: 45,
            chunk_target_tokens: 750,
            chunk_overlap_messages: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsConfig {
    /// "embedded" (local ONNX model) or "openai-compatible".
    pub provider: String,
    pub model: String,
    pub batch_size: u32,
    /// Only used by the openai-compatible provider.
    pub base_url: Option<String>,
    /// Only used by the openai-compatible provider. Redacted in `config show`.
    pub api_key: Option<String>,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            provider: "embedded".into(),
            model: "bge-small-en-v1.5".into(),
            batch_size: 32,
            base_url: None,
            api_key: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceConfig {
    pub interval_seconds: u32,
    /// Bearer token required by `serve --http`. When unset, a random token
    /// is generated on first use and stored next to the index. Redacted in
    /// `config show`.
    pub http_token: Option<String>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            interval_seconds: 300,
            http_token: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetrievalConfig {
    pub fts_candidates: u32,
    pub vector_candidates: u32,
    pub result_limit: u32,
    pub context_before: u32,
    pub context_after: u32,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            fts_candidates: 30,
            vector_candidates: 30,
            result_limit: 10,
            context_before: 5,
            context_after: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacyConfig {
    /// Both privacy toggles default to off: nothing leaves the machine and
    /// attachment contents stay unindexed unless explicitly enabled.
    pub allow_remote_embedding_endpoint: bool,
    pub index_attachment_contents: bool,
}

impl Config {
    /// Absolute path to the Apple Messages database, with `~` expanded.
    pub fn source_db_path(&self) -> Result<PathBuf> {
        paths::expand_tilde(&self.source.database_path)
    }

    /// Absolute path to the Contacts store, with `~` expanded. `None`
    /// when contact-name resolution is disabled (empty path).
    pub fn contacts_path(&self) -> Result<Option<PathBuf>> {
        if self.source.contacts_path.trim().is_empty() {
            return Ok(None);
        }
        paths::expand_tilde(&self.source.contacts_path).map(Some)
    }

    /// Absolute path to the destination index database, with `~` expanded.
    pub fn index_db_path(&self) -> Result<PathBuf> {
        paths::expand_tilde(&self.index.database_path)
    }

    /// Directory that will hold the destination index.
    pub fn index_dir(&self) -> Result<PathBuf> {
        let p = self.index_db_path()?;
        Ok(p.parent().map(Path::to_path_buf).unwrap_or(p))
    }

    /// A copy safe for display: secrets are replaced, never printed.
    pub fn redacted(&self) -> Config {
        let mut c = self.clone();
        if c.embeddings.api_key.is_some() {
            c.embeddings.api_key = Some("<redacted>".into());
        }
        if c.service.http_token.is_some() {
            c.service.http_token = Some("<redacted>".into());
        }
        c
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("could not serialize configuration")
    }
}

/// A configuration plus where it came from.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub path: PathBuf,
    pub from_file: bool,
}

/// Load configuration.
///
/// An explicitly passed path must exist. The default path is optional:
/// missing means "use built-in defaults".
pub fn load(explicit: Option<&Path>) -> Result<LoadedConfig> {
    match explicit {
        Some(p) => {
            let raw = fs::read_to_string(p)
                .with_context(|| format!("could not read config file {}", p.display()))?;
            Ok(LoadedConfig {
                config: parse(&raw).with_context(|| format!("in config file {}", p.display()))?,
                path: p.to_path_buf(),
                from_file: true,
            })
        }
        None => {
            let p = paths::default_config_path()?;
            if p.exists() {
                let raw = fs::read_to_string(&p)
                    .with_context(|| format!("could not read config file {}", p.display()))?;
                Ok(LoadedConfig {
                    config: parse(&raw)
                        .with_context(|| format!("in config file {}", p.display()))?,
                    path: p,
                    from_file: true,
                })
            } else {
                Ok(LoadedConfig {
                    config: Config::default(),
                    path: p,
                    from_file: false,
                })
            }
        }
    }
}

pub fn parse(raw: &str) -> Result<Config> {
    toml::from_str(raw).context("config file is not valid TOML for ai-imessage")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!(c.source.database_path, "~/Library/Messages/chat.db");
        assert_eq!(c.source.recent_overlap_rows, 5000);
        assert_eq!(
            c.source.contacts_path,
            "~/Library/Application Support/AddressBook"
        );
        assert_eq!(c.index.chunk_gap_minutes, 45);
        assert_eq!(c.index.chunk_target_tokens, 750);
        assert_eq!(c.index.chunk_overlap_messages, 3);
        assert_eq!(c.embeddings.provider, "embedded");
        assert_eq!(c.embeddings.batch_size, 32);
        assert_eq!(c.service.interval_seconds, 300);
        assert_eq!(c.retrieval.fts_candidates, 30);
        assert_eq!(c.retrieval.vector_candidates, 30);
        assert_eq!(c.retrieval.result_limit, 10);
        assert!(!c.privacy.allow_remote_embedding_endpoint);
        assert!(!c.privacy.index_attachment_contents);
    }

    #[test]
    fn empty_file_yields_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_file_overrides_only_named_keys() {
        let c = parse("[index]\nchunk_gap_minutes = 60\n").unwrap();
        assert_eq!(c.index.chunk_gap_minutes, 60);
        // Sibling key in the same section keeps its default.
        assert_eq!(c.index.chunk_target_tokens, 750);
        // Other sections keep their defaults.
        assert_eq!(c.source.recent_overlap_rows, 5000);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // Typos must fail loudly, not be silently ignored.
        assert!(parse("[index]\nchunk_gap_minutess = 60\n").is_err());
        assert!(parse("[indexx]\nchunk_gap_minutes = 60\n").is_err());
    }

    #[test]
    fn invalid_types_are_rejected() {
        assert!(parse("[index]\nchunk_gap_minutes = \"soon\"\n").is_err());
        assert!(parse("[service]\ninterval_seconds = -1\n").is_err());
    }

    #[test]
    fn serialization_roundtrips() {
        let mut c = Config::default();
        c.embeddings.base_url = Some("http://127.0.0.1:1234/v1".into());
        let raw = c.to_toml().unwrap();
        assert_eq!(parse(&raw).unwrap(), c);
    }

    #[test]
    fn api_key_is_redacted() {
        let mut c = Config::default();
        c.embeddings.api_key = Some("super-secret".into());
        let shown = c.redacted().to_toml().unwrap();
        assert!(!shown.contains("super-secret"));
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn http_token_is_redacted() {
        let mut c = Config::default();
        c.service.http_token = Some("token-secret".into());
        let shown = c.redacted().to_toml().unwrap();
        assert!(!shown.contains("token-secret"));
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn redaction_without_key_keeps_config_identical() {
        let c = Config::default();
        assert_eq!(c.redacted(), c);
    }

    #[test]
    fn source_path_expands_tilde() {
        let c = Config::default();
        let p = c.source_db_path().unwrap();
        assert!(p.is_absolute());
        assert!(p.ends_with("Library/Messages/chat.db"));
        assert!(!p.to_string_lossy().contains('~'));
    }

    #[test]
    fn index_dir_is_parent_of_index_db() {
        let c = Config::default();
        assert_eq!(
            c.index_dir().unwrap(),
            c.index_db_path().unwrap().parent().unwrap()
        );
    }

    #[test]
    fn explicit_missing_path_is_an_error() {
        assert!(load(Some(Path::new("/nonexistent/config.toml"))).is_err());
    }
}
