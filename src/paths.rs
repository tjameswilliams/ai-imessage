//! Filesystem locations used by ai-imessage, plus `~` expansion.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Directory name under `~/Library/Application Support`.
pub const APP_DIR_NAME: &str = "ai-imessage";

pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine the current user's home directory")
}

/// Expand a leading `~` or `~/` to the current user's home directory.
///
/// `~user` forms are not expanded; they are returned verbatim.
pub fn expand_tilde(input: &str) -> Result<PathBuf> {
    Ok(expand_tilde_with_home(input, &home_dir()?))
}

fn expand_tilde_with_home(input: &str, home: &Path) -> PathBuf {
    if input == "~" {
        home.to_path_buf()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(input)
    }
}

/// `~/Library/Application Support/ai-imessage`
pub fn app_support_dir() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("Application Support")
        .join(APP_DIR_NAME))
}

/// Default location of the config file.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(app_support_dir()?.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/testuser")
    }

    #[test]
    fn bare_tilde_expands_to_home() {
        assert_eq!(expand_tilde_with_home("~", &home()), home());
    }

    #[test]
    fn tilde_slash_prefix_expands() {
        assert_eq!(
            expand_tilde_with_home("~/Library/Messages/chat.db", &home()),
            PathBuf::from("/Users/testuser/Library/Messages/chat.db")
        );
    }

    #[test]
    fn absolute_paths_pass_through() {
        assert_eq!(
            expand_tilde_with_home("/var/tmp/x", &home()),
            PathBuf::from("/var/tmp/x")
        );
    }

    #[test]
    fn relative_paths_pass_through() {
        assert_eq!(
            expand_tilde_with_home("data/chat.db", &home()),
            PathBuf::from("data/chat.db")
        );
    }

    #[test]
    fn tilde_user_forms_are_not_expanded() {
        assert_eq!(
            expand_tilde_with_home("~other/x", &home()),
            PathBuf::from("~other/x")
        );
    }

    #[test]
    fn tilde_in_the_middle_is_not_expanded() {
        assert_eq!(
            expand_tilde_with_home("/a/~/b", &home()),
            PathBuf::from("/a/~/b")
        );
    }

    #[test]
    fn app_support_dir_is_under_home() {
        let dir = app_support_dir().unwrap();
        assert!(dir.ends_with("Library/Application Support/ai-imessage"));
    }

    #[test]
    fn default_config_path_is_in_app_dir() {
        let p = default_config_path().unwrap();
        assert!(p.ends_with("ai-imessage/config.toml"));
    }
}
