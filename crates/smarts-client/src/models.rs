//! Typed views over the gateway responses we render as tables. Fields are all
//! optional/tolerant so upstream shape drift degrades gracefully rather than
//! failing the whole command.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Workspace {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "createdAt")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolInfo {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    /// Parameter schema, kept raw for `tool show`.
    #[serde(default)]
    pub parameters: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileItem {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// "file" | "folder".
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default, rename = "lastModified")]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

impl FileItem {
    pub fn is_folder(&self) -> bool {
        self.kind.as_deref() == Some("folder")
    }
}
