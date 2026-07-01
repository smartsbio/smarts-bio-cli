//! MCP server exposing smarts.bio over the Model Context Protocol.
//!
//! A focused tool layer over [`smarts_client::SmartsClient`] — the same `/v1`
//! surface the CLI uses — so a chat agent (Claude Desktop, Cursor, …) can drive
//! the bioinformatics agent, tools, pipelines, and workspace files natively.
//!
//! Currently runs over stdio ([`serve_stdio`]); the same tool layer can later be
//! hosted over Streamable HTTP.

use std::sync::Arc;

use base64::Engine;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use smarts_client::SmartsClient;

/// Max bytes returned by `smarts_read_file` (keeps responses sane for big files).
const MAX_READ_BYTES: usize = 100 * 1024;

/// Max bytes of an image embedded inline in a query answer (~1 MB of base64);
/// larger images keep just their signed URL to keep the tool payload bounded.
const MAX_INLINE_IMAGE_BYTES: usize = 750 * 1024;
/// Max images inlined per query answer.
const MAX_INLINE_IMAGES: usize = 4;

#[derive(Clone)]
pub struct SmartsMcp {
    client: Arc<SmartsClient>,
    default_workspace: Option<String>,
    tool_router: ToolRouter<Self>,
}

// ---- Tool argument schemas -----------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct QueryArgs {
    /// The question or instruction for the bioinformatics agent.
    prompt: String,
    /// Workspace id (defaults to the CLI's saved default workspace).
    #[serde(default)]
    workspace_id: Option<String>,
    /// Continue an existing conversation by id.
    #[serde(default)]
    conversation_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListToolsArgs {
    /// Optional category filter (e.g. "database", "algorithm").
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RunToolArgs {
    /// Tool id (from smarts_list_tools), e.g. "ncbi-blast".
    tool_id: String,
    /// Tool input parameters as a JSON object.
    #[serde(default)]
    input: Map<String, Value>,
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RunPipelineArgs {
    /// Pipeline id (from smarts_list_pipelines), e.g. "quality-control".
    pipeline_id: String,
    /// Pipeline input parameters as a JSON object.
    #[serde(default)]
    input: Map<String, Value>,
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RunStatusArgs {
    /// The run id returned by smarts_run_pipeline.
    run_id: String,
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListFilesArgs {
    /// Folder path within the workspace (empty = root).
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct FileKeyArgs {
    /// Full storage key of the file.
    key: String,
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ViewImageArgs {
    /// Storage key of the file to render.
    key: String,
    #[serde(default)]
    workspace_id: Option<String>,
    /// Image format: "png" (default) or "svg".
    #[serde(default)]
    format: Option<String>,
    /// Optional region — sequence "start-end" or genomic "chrom:start-end".
    #[serde(default)]
    region: Option<String>,
    /// For CSV/TSV files only: chart type (bar-v, bar-stacked, line, scatter, pie,
    /// donut, heatmap-2d, boxplot, violin, hist, …). Omit to auto-pick.
    #[serde(default)]
    chart_type: Option<String>,
}

// ---- Tools ----------------------------------------------------------------

#[tool_router]
impl SmartsMcp {
    pub fn new(client: SmartsClient, default_workspace: Option<String>) -> Self {
        Self {
            client: Arc::new(client),
            default_workspace,
            tool_router: Self::tool_router(),
        }
    }

    fn workspace(&self, given: Option<String>) -> Option<String> {
        given.or_else(|| self.default_workspace.clone())
    }

    fn require_workspace(&self, given: Option<String>) -> Result<String, McpError> {
        self.workspace(given).ok_or_else(|| {
            McpError::invalid_params(
                "no workspace — pass workspace_id, or set a default with `smarts workspace use`",
                None,
            )
        })
    }

    #[tool(
        description = "Ask the smarts.bio bioinformatics agent a question. It can search biological databases, run tools, design pipelines, and reason over results. Use this for most open-ended bioinformatics tasks; returns the agent's answer."
    )]
    async fn smarts_query(&self, params: Parameters<QueryArgs>) -> Result<CallToolResult, McpError> {
        let a = params.0;
        let ws = self.workspace(a.workspace_id);
        let resp = self
            .client
            .query(&a.prompt, ws.as_deref(), a.conversation_id.as_deref())
            .await
            .map_err(to_mcp_err)?;
        let answer = resp
            .get("result")
            .and_then(Value::as_str)
            .or_else(|| resp.pointer("/data/result").and_then(Value::as_str))
            .or_else(|| resp.get("response").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string_pretty(&resp).unwrap_or_default());
        let content = self.enrich_answer(&answer, ws.as_deref()).await;
        Ok(CallToolResult::success(content))
    }

    /// Turn a raw agent answer into MCP content. The agent embeds rendered
    /// visualizations as markdown pointing at an `s3://…` storage key —
    /// `![alt](s3://…)` for images, `[text](s3://…)` for files. A remote host
    /// can't resolve a raw `s3://` reference, so we rewrite every `s3://` target
    /// to a signed https URL and, for image refs, also attach an inline image
    /// block (bounded by size + count) so the figure renders in the chat.
    async fn enrich_answer(&self, answer: &str, workspace: Option<&str>) -> Vec<Content> {
        let mut text_out = String::new();
        let mut images: Vec<Content> = Vec::new();
        let mut rest = answer;

        while let Some(open) = rest.find('[') {
            // A markdown image is a link preceded by '!'.
            let is_image = open > 0 && rest.as_bytes()[open - 1] == b'!';
            let after = &rest[open..]; // starts at '['

            // Parse `[text](target)`.
            let parsed = after.find("](").and_then(|text_end| {
                let url_start = text_end + 2;
                after[url_start..].find(')').map(|rel| {
                    (
                        &after[1..text_end],
                        &after[url_start..url_start + rel],
                        url_start + rel + 1,
                    )
                })
            });

            match parsed {
                Some((label, target, consumed)) => {
                    let prefix_end = if is_image { open - 1 } else { open };
                    text_out.push_str(&rest[..prefix_end]);

                    // Rewrite s3:// → signed URL (images and links alike); leave
                    // every other target exactly as the agent wrote it.
                    let signed = self.sign_s3(workspace, target).await;
                    let shown = signed.as_deref().unwrap_or(target);
                    let prefix = if is_image { "!" } else { "" };
                    text_out.push_str(&format!("{prefix}[{label}]({shown})"));

                    // For images, also inline the bytes (bounded).
                    if is_image && images.len() < MAX_INLINE_IMAGES {
                        let fetch = if target.starts_with("s3://") {
                            signed
                        } else if target.starts_with("http://") || target.starts_with("https://") {
                            Some(target.to_string())
                        } else {
                            None
                        };
                        if let Some(url) = fetch {
                            if let Some(c) = self.inline_image(&url, target).await {
                                images.push(c);
                            }
                        }
                    }
                    rest = &after[consumed..];
                }
                None => {
                    // Not a complete token — emit up to and including '[' and
                    // continue so the scan always advances (no infinite loop).
                    text_out.push_str(&rest[..open + 1]);
                    rest = &after[1..];
                }
            }
        }
        text_out.push_str(rest);

        let mut content = vec![Content::text(text_out)];
        content.extend(images);
        content
    }

    /// Resolve an `s3://<key>` target to a signed https URL. `None` when the
    /// target isn't an `s3://` ref or the key can't be resolved.
    async fn sign_s3(&self, workspace: Option<&str>, target: &str) -> Option<String> {
        let key = target.strip_prefix("s3://")?;
        let ws = workspace
            .map(str::to_string)
            .or_else(|| parse_workspace_from_key(key))?;
        self.client.download_url(&ws, key).await.ok()
    }

    /// Fetch image bytes from a URL and build a bounded inline image block.
    /// `None` if the fetch fails or the image exceeds the size cap.
    async fn inline_image(&self, url: &str, name_hint: &str) -> Option<Content> {
        let bytes = self.client.fetch_url_bytes(url).await.ok()?;
        if bytes.is_empty() || bytes.len() > MAX_INLINE_IMAGE_BYTES {
            return None;
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Some(Content::image(data, guess_image_mime(name_hint)))
    }

    #[tool(description = "List the available bioinformatics tools (BLAST, GATK, NCBI lookups, etc.), optionally filtered by category.")]
    async fn smarts_list_tools(
        &self,
        params: Parameters<ListToolsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tools = self
            .client
            .list_tools(params.0.category.as_deref())
            .await
            .map_err(to_mcp_err)?;
        json_result(&tools)
    }

    #[tool(description = "Run a single bioinformatics tool directly with the given input parameters. Use smarts_list_tools to discover tool ids and their parameters.")]
    async fn smarts_run_tool(
        &self,
        params: Parameters<RunToolArgs>,
    ) -> Result<CallToolResult, McpError> {
        let a = params.0;
        let ws = self.workspace(a.workspace_id);
        let result = self
            .client
            .run_tool(&a.tool_id, ws.as_deref(), Value::Object(a.input))
            .await
            .map_err(to_mcp_err)?;
        json_result(&result)
    }

    #[tool(description = "List the available multi-step pipeline definitions (QC, alignment, RNA-seq, variant calling, etc.).")]
    async fn smarts_list_pipelines(&self) -> Result<CallToolResult, McpError> {
        let defs = self.client.list_pipeline_defs().await.map_err(to_mcp_err)?;
        json_result(&defs)
    }

    #[tool(description = "Start a pipeline run with the given input parameters. Returns a run id; poll it with smarts_run_status.")]
    async fn smarts_run_pipeline(
        &self,
        params: Parameters<RunPipelineArgs>,
    ) -> Result<CallToolResult, McpError> {
        let a = params.0;
        let ws = self.require_workspace(a.workspace_id)?;
        let result = self
            .client
            .run_pipeline(&a.pipeline_id, &ws, Value::Object(a.input))
            .await
            .map_err(to_mcp_err)?;
        json_result(&result)
    }

    #[tool(description = "Get the status of a pipeline run by id.")]
    async fn smarts_run_status(
        &self,
        params: Parameters<RunStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let a = params.0;
        let ws = self.require_workspace(a.workspace_id)?;
        let status = self
            .client
            .run_status(&a.run_id, &ws)
            .await
            .map_err(to_mcp_err)?;
        json_result(&status)
    }

    #[tool(description = "List files and folders in a workspace directory.")]
    async fn smarts_list_files(
        &self,
        params: Parameters<ListFilesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let a = params.0;
        let ws = self.require_workspace(a.workspace_id)?;
        let files = self
            .client
            .list_files(&ws, a.path.as_deref().unwrap_or(""))
            .await
            .map_err(to_mcp_err)?;
        json_result(&files)
    }

    #[tool(description = "Read the contents of a workspace file by its storage key (text files; truncated if large).")]
    async fn smarts_read_file(
        &self,
        params: Parameters<FileKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let a = params.0;
        let ws = self.require_workspace(a.workspace_id)?;
        let bytes = self
            .client
            .download_bytes(&ws, &a.key)
            .await
            .map_err(to_mcp_err)?;
        let truncated = bytes.len() > MAX_READ_BYTES;
        let slice = &bytes[..bytes.len().min(MAX_READ_BYTES)];
        let mut text = String::from_utf8_lossy(slice).into_owned();
        if truncated {
            text.push_str("\n…[truncated]");
        }
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Get a shareable browser viewer URL for a workspace file (FASTA, BAM, VCF, PDB, CSV, …).")]
    async fn smarts_open_file(
        &self,
        params: Parameters<FileKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let a = params.0;
        let ws = self.require_workspace(a.workspace_id)?;
        let url = self
            .client
            .viewer_url(&ws, &a.key)
            .await
            .map_err(to_mcp_err)?;
        Ok(CallToolResult::success(vec![Content::text(url)]))
    }

    #[tool(
        description = "Render a workspace file as a static image (PNG/SVG) that looks exactly like the interactive viewer — a heatmap/chart from a CSV, a FASTA region, a PDB structure, BAM coverage, VCF variants. Returns the image inline so you can show it to the user directly, plus a full-resolution URL. Use smarts_open_file for an interactive link instead."
    )]
    async fn smarts_view_as_image(
        &self,
        params: Parameters<ViewImageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let a = params.0;
        let ws = self.require_workspace(a.workspace_id)?;
        let resp = self
            .client
            .render_view(&ws, &a.key, a.format.as_deref(), a.region.as_deref(), a.chart_type.as_deref())
            .await
            .map_err(to_mcp_err)?;
        let data = resp
            .get("thumbnail_base64")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mime = resp
            .get("thumbnail_mime_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();
        let mut content = vec![Content::image(data, mime)];
        if let Some(url) = resp.get("image_url").and_then(Value::as_str) {
            content.push(Content::text(format!("Full-resolution image: {url}")));
        }
        Ok(CallToolResult::success(content))
    }

    #[tool(description = "List the workspaces the authenticated user can access.")]
    async fn smarts_list_workspaces(&self) -> Result<CallToolResult, McpError> {
        let workspaces = self.client.list_workspaces().await.map_err(to_mcp_err)?;
        json_result(&workspaces)
    }
}

#[tool_handler]
impl ServerHandler for SmartsMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "smarts".to_string(),
                title: Some("smarts.bio".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                website_url: Some("https://smarts.bio".to_string()),
                ..Implementation::from_build_env()
            },
            instructions: Some(
                "smarts.bio — AI bioinformatics. Prefer `smarts_query` for open-ended tasks; \
                 use the specific tools to list/run tools and pipelines or browse workspace files."
                    .into(),
            ),
        }
    }
}

/// Run the MCP server over stdio until the client disconnects.
pub async fn serve_stdio(
    client: SmartsClient,
    default_workspace: Option<String>,
) -> anyhow::Result<()> {
    let server = SmartsMcp::new(client, default_workspace);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

// ---- helpers --------------------------------------------------------------

fn to_mcp_err(e: smarts_client::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// Extract the workspace id from a storage key like
/// `organizations/<org>/workspaces/<ws>/files/…`.
fn parse_workspace_from_key(key: &str) -> Option<String> {
    let mut segs = key.split('/');
    while let Some(seg) = segs.next() {
        if seg == "workspaces" {
            return segs.next().map(str::to_string);
        }
    }
    None
}

/// Best-effort image MIME type from a key/URL extension (defaults to PNG).
fn guess_image_mime(name: &str) -> String {
    let ext = name
        .split('?')
        .next()
        .unwrap_or(name)
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => "image/png",
    }
    .to_string()
}

fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}
