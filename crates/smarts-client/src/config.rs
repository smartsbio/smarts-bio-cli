use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Default gateway base URL. Override with `SMARTSBIO_BASE_URL` or `config.base_url`.
pub const DEFAULT_BASE_URL: &str = "https://api.smarts.bio";

/// Persisted, non-secret CLI preferences stored at `~/.config/smarts/config.toml`.
///
/// Secrets (the `sk_live_` API key, and later OAuth tokens) live in the OS
/// keychain via [`crate::credentials`], never in this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Gateway base URL override. `None` falls back to env then [`DEFAULT_BASE_URL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Workspace used when `--workspace` is not passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_workspace: Option<String>,

    /// Per-workspace current directory for the shell-like `file` commands,
    /// keyed by workspace id. Paths are workspace-relative (no leading slash).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub cwd: HashMap<String, String>,
}

impl Config {
    fn dir() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("bio", "smarts", "smarts")
            .ok_or_else(|| Error::Config("could not determine a config directory".into()))?;
        Ok(dirs.config_dir().to_path_buf())
    }

    /// Absolute path to `config.toml`.
    pub fn path() -> Result<PathBuf> {
        Ok(Self::dir()?.join("config.toml"))
    }

    /// Load config, returning defaults if the file does not exist yet.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        toml::from_str(&text).map_err(|e| Error::Config(e.to_string()))
    }

    /// Persist the config, creating the directory if needed.
    pub fn save(&self) -> Result<()> {
        let dir = Self::dir()?;
        std::fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        std::fs::write(Self::path()?, text)?;
        Ok(())
    }

    /// Resolved base URL: `SMARTSBIO_BASE_URL` > `config.base_url` > default.
    pub fn resolved_base_url(&self) -> String {
        std::env::var("SMARTSBIO_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| self.base_url.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
    }

    /// Current directory for a workspace (empty string == workspace root).
    pub fn cwd_for(&self, workspace_id: &str) -> String {
        self.cwd.get(workspace_id).cloned().unwrap_or_default()
    }

    /// Set (or clear, when empty) the current directory for a workspace.
    pub fn set_cwd(&mut self, workspace_id: &str, path: &str) {
        if path.is_empty() {
            self.cwd.remove(workspace_id);
        } else {
            self.cwd.insert(workspace_id.to_string(), path.to_string());
        }
    }
}

/// Resolve a `cd`-style target against a workspace-relative current directory.
///
/// - a leading `/` resets to workspace root
/// - `.` is a no-op, `..` pops one segment (clamped at root — never escapes)
/// - the result is always workspace-relative with no leading/trailing slash
pub fn resolve_path(cwd: &str, target: &str) -> String {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        cwd.split('/').filter(|s| !s.is_empty()).collect()
    };

    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::resolve_path;

    #[test]
    fn joins_relative_segments() {
        assert_eq!(resolve_path("a/b", "c"), "a/b/c");
        assert_eq!(resolve_path("", "results"), "results");
    }

    #[test]
    fn dotdot_clamps_at_root() {
        assert_eq!(resolve_path("a", ".."), "");
        // cannot escape above the workspace root no matter how many ".."
        assert_eq!(resolve_path("a/b", "../../../.."), "");
    }

    #[test]
    fn leading_slash_resets_to_root() {
        assert_eq!(resolve_path("a/b/c", "/x"), "x");
        assert_eq!(resolve_path("deep/nested", "/"), "");
    }

    #[test]
    fn dot_is_noop_and_trailing_slashes_ignored() {
        assert_eq!(resolve_path("a", "./b/"), "a/b");
        assert_eq!(resolve_path("a//b", "c"), "a/b/c");
    }
}
