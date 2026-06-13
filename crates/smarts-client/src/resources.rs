//! API methods grouped by resource. Implemented directly on [`SmartsClient`].
//!
//! Methods that we render as tables return typed [`crate::models`] structs;
//! the rest return raw `serde_json::Value` (passed straight through from the
//! agent / process-manager / backend), which the CLI formats or prints as JSON.

use std::path::Path;

use serde_json::{json, Value};

use crate::client::SmartsClient;
use crate::error::{Error, Result};
use crate::models::{FileItem, ToolInfo, Workspace};

/// Direct multipart upload ceiling; larger files use the presigned S3 flow.
/// Mirrors the gateway's `DIRECT_UPLOAD_LIMIT`.
pub const DIRECT_UPLOAD_LIMIT: u64 = 10 * 1024 * 1024;

impl SmartsClient {
    /// Authenticated user's profile (`GET /v1/user/profile`, JWT login only).
    pub async fn user_profile(&self) -> Result<Value> {
        self.get("/v1/user/profile", &[]).await
    }

    // ---- Workspaces -------------------------------------------------------

    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let body = self.get("/v1/workspaces", &[]).await?;
        let data = body.get("data").cloned().unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value(data)?)
    }

    // ---- Query ------------------------------------------------------------

    /// Synchronous agent query (`POST /v1/query`).
    pub async fn query(
        &self,
        prompt: &str,
        workspace_id: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Result<Value> {
        let mut body = json!({ "prompt": prompt });
        if let Some(w) = workspace_id {
            body["workspace_id"] = w.into();
        }
        if let Some(c) = conversation_id {
            body["conversation_id"] = c.into();
        }
        self.post_json("/v1/query", body).await
    }

    /// Streaming agent query (`POST /v1/query/stream`). `on_event` fires per SSE frame.
    pub async fn query_stream<F>(
        &self,
        prompt: &str,
        workspace_id: Option<&str>,
        conversation_id: Option<&str>,
        on_event: F,
    ) -> Result<()>
    where
        F: FnMut(Value),
    {
        let mut body = json!({ "prompt": prompt });
        if let Some(w) = workspace_id {
            body["workspace_id"] = w.into();
        }
        if let Some(c) = conversation_id {
            body["conversation_id"] = c.into();
        }
        self.stream_sse("/v1/query/stream", body, on_event).await
    }

    /// Stop an active streaming session (`POST /v1/query/stop`).
    pub async fn stop_query(&self, conversation_id: &str) -> Result<Value> {
        self.post_json(
            "/v1/query/stop",
            json!({ "conversation_id": conversation_id }),
        )
        .await
    }

    // ---- Tools ------------------------------------------------------------

    pub async fn list_tools(&self, category: Option<&str>) -> Result<Vec<ToolInfo>> {
        let mut query = Vec::new();
        if let Some(c) = category {
            query.push(("category", c.to_string()));
        }
        let body = self.get("/v1/tools", &query).await?;
        let tools = body.get("tools").cloned().unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value(tools)?)
    }

    /// Run a single tool directly (`POST /v1/tools/:id/run`).
    pub async fn run_tool(
        &self,
        tool_id: &str,
        workspace_id: Option<&str>,
        input: Value,
    ) -> Result<Value> {
        let mut body = json!({ "input": input });
        if let Some(w) = workspace_id {
            body["workspace_id"] = w.into();
        }
        self.post_json(&format!("/v1/tools/{tool_id}/run"), body)
            .await
    }

    // ---- Pipelines (definitions) & Runs (executions) ----------------------

    /// Available pipeline definitions (`GET /v1/catalog/pipelines`, public).
    pub async fn list_pipeline_defs(&self) -> Result<Value> {
        self.get("/v1/catalog/pipelines", &[]).await
    }

    /// Start a pipeline run (`POST /v1/pipelines`); returns the run record.
    pub async fn run_pipeline(
        &self,
        pipeline_id: &str,
        workspace_id: &str,
        input: Value,
    ) -> Result<Value> {
        self.post_json(
            "/v1/pipelines",
            json!({ "tool_id": pipeline_id, "workspace_id": workspace_id, "input": input }),
        )
        .await
    }

    /// List executions in a workspace (`GET /v1/pipelines`).
    pub async fn list_runs(&self, workspace_id: &str, status: Option<&str>) -> Result<Value> {
        let mut query = vec![("workspace_id", workspace_id.to_string())];
        if let Some(s) = status {
            query.push(("status", s.to_string()));
        }
        self.get("/v1/pipelines", &query).await
    }

    pub async fn run_status(&self, run_id: &str, workspace_id: &str) -> Result<Value> {
        self.get(
            &format!("/v1/pipelines/{run_id}"),
            &[("workspace_id", workspace_id.to_string())],
        )
        .await
    }

    pub async fn cancel_run(&self, run_id: &str, workspace_id: &str) -> Result<Value> {
        let rb = self
            .request(reqwest::Method::DELETE, &format!("/v1/pipelines/{run_id}"))?
            .query(&[("workspace_id", workspace_id)]);
        self.send(rb).await
    }

    // ---- Conversations ----------------------------------------------------

    pub async fn list_conversations(&self) -> Result<Value> {
        self.get("/v1/conversations", &[]).await
    }

    pub async fn get_conversation(&self, id: &str) -> Result<Value> {
        self.get(&format!("/v1/conversations/{id}"), &[]).await
    }

    // ---- Files ------------------------------------------------------------

    /// List items under a workspace-relative folder (`GET /v1/files`).
    pub async fn list_files(&self, workspace_id: &str, path: &str) -> Result<Vec<FileItem>> {
        let mut query = vec![("workspace_id", workspace_id.to_string())];
        if !path.is_empty() {
            query.push(("path", path.to_string()));
        }
        let body = self.get("/v1/files", &query).await?;
        let files = body
            .pointer("/data/files")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value(files)?)
    }

    /// Resolve a time-limited download URL for a file key (`GET /v1/files/download`).
    pub async fn download_url(&self, workspace_id: &str, key: &str) -> Result<String> {
        let body = self
            .get(
                "/v1/files/download",
                &[
                    ("workspace_id", workspace_id.to_string()),
                    ("key", key.to_string()),
                ],
            )
            .await?;
        body.pointer("/data/downloadUrl")
            .or_else(|| body.get("downloadUrl"))
            .or_else(|| body.pointer("/data/download_url"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Other("no download URL in gateway response".into()))
    }

    /// Resolve a download URL and fetch the file bytes (presigned S3 GET, no auth).
    pub async fn download_bytes(&self, workspace_id: &str, key: &str) -> Result<Vec<u8>> {
        let url = self.download_url(workspace_id, key).await?;
        let resp = self.raw().get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "download failed with status {}",
                resp.status()
            )));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Generate a shareable view.smarts.bio URL (`POST /v1/visualizations/viewer-url`).
    pub async fn viewer_url(&self, workspace_id: &str, key: &str) -> Result<String> {
        let body = self
            .post_json(
                "/v1/visualizations/viewer-url",
                json!({ "file_key": key, "workspace_id": workspace_id }),
            )
            .await?;
        body.get("viewer_url")
            .or_else(|| body.pointer("/data/viewer_url"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Other("no viewer_url in gateway response".into()))
    }

    pub async fn create_folder(&self, workspace_id: &str, name: &str, path: &str) -> Result<Value> {
        self.post_json(
            "/v1/files/folder",
            json!({ "workspace_id": workspace_id, "name": name, "path": path }),
        )
        .await
    }

    pub async fn move_file(
        &self,
        workspace_id: &str,
        file_key: &str,
        destination_path: &str,
    ) -> Result<Value> {
        self.put_json(
            "/v1/files/move",
            json!({
                "workspace_id": workspace_id,
                "fileKey": file_key,
                "destinationPath": destination_path,
            }),
        )
        .await
    }

    pub async fn delete_file(&self, workspace_id: &str, key: &str) -> Result<Value> {
        let rb = self
            .request(reqwest::Method::DELETE, "/v1/files")?
            .query(&[("workspace_id", workspace_id), ("key", key)]);
        self.send(rb).await
    }

    /// Upload a local file to `dest_path` within the workspace. Uses direct
    /// multipart for files up to [`DIRECT_UPLOAD_LIMIT`], otherwise the
    /// presigned S3 three-step flow — identical to the Studio / extension flow.
    pub async fn upload_file(
        &self,
        workspace_id: &str,
        local_path: &Path,
        dest_path: &str,
    ) -> Result<Value> {
        let size = std::fs::metadata(local_path)?.len();
        let filename = local_path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::Other("invalid file name".into()))?
            .to_string();
        let content_type = mime_guess::from_path(local_path)
            .first_or_octet_stream()
            .to_string();
        let bytes = tokio::fs::read(local_path).await?;

        if size <= DIRECT_UPLOAD_LIMIT {
            let part = reqwest::multipart::Part::bytes(bytes)
                .file_name(filename)
                .mime_str(&content_type)?;
            let mut form = reqwest::multipart::Form::new()
                .text("workspace_id", workspace_id.to_string())
                .part("file", part);
            if !dest_path.is_empty() {
                form = form.text("path", dest_path.to_string());
            }
            let rb = self
                .request(reqwest::Method::POST, "/v1/files/upload")?
                .multipart(form);
            return self.send(rb).await;
        }

        // Presigned flow: request URL → PUT to S3 (no auth) → confirm.
        let presign = self
            .post_json(
                "/v1/files/upload-url",
                json!({
                    "workspace_id": workspace_id,
                    "filename": filename,
                    "contentType": content_type,
                    "size": size,
                    "path": dest_path,
                }),
            )
            .await?;
        let upload_url = presign
            .pointer("/data/uploadUrl")
            .or_else(|| presign.get("uploadUrl"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Other("no uploadUrl in presign response".into()))?
            .to_string();
        let file_key = presign
            .pointer("/data/fileKey")
            .or_else(|| presign.get("fileKey"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Other("no fileKey in presign response".into()))?
            .to_string();

        let put = self
            .raw()
            .put(&upload_url)
            .header("Content-Type", &content_type)
            .body(bytes)
            .send()
            .await?;
        if !put.status().is_success() {
            return Err(Error::Other(format!(
                "S3 upload failed with status {}",
                put.status()
            )));
        }

        self.post_json(
            "/v1/files/upload-confirm",
            json!({
                "workspace_id": workspace_id,
                "fileKey": file_key,
                "filename": filename,
                "size": size,
                "contentType": content_type,
            }),
        )
        .await
    }
}
