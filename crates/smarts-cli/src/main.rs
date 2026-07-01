//! `smarts` — the smarts.bio command-line interface.
//!
//! Phase 1: API-key auth (env `SMARTSBIO_API_KEY` or `smarts auth set-key`),
//! covering workspaces, query, tools, pipelines, runs, files. Browser login,
//! local-file `open`, and the MCP server arrive in later phases and currently
//! print a pointer to that work.

mod chat;
mod localopen;
mod mcp_install;
mod output;

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde_json::{json, Value};
use smarts_client::{resolve_path, Config, DevicePoll, SmartsClient, TokenSource};

use output::{extract_array, first_str, human_size, print_json, table, truncate};

#[derive(Parser)]
#[command(
    name = "smarts",
    version,
    about = "smarts.bio — bioinformatics from your terminal",
    propagate_version = true,
    disable_help_subcommand = true
)]
struct Cli {
    /// Workspace id (overrides the saved default).
    #[arg(long, short = 'w', global = true)]
    workspace: Option<String>,

    /// Emit raw JSON instead of formatted tables.
    #[arg(long, global = true)]
    json: bool,

    /// Subcommand. Omit it to drop into an interactive chat (in a terminal).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start an interactive chat with the agent.
    Chat(ChatArgs),
    /// Log in via the browser (device-code flow; works over SSH/headless).
    Login,
    /// Clear stored credentials.
    Logout,
    /// Manage authentication.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// List and select workspaces.
    #[command(alias = "workspaces")]
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// Ask the agent a question (streams by default).
    Query(QueryArgs),
    /// Bioinformatics tools.
    #[command(alias = "tools")]
    Tool {
        #[command(subcommand)]
        cmd: ToolCmd,
    },
    /// Pipeline definitions you can run.
    #[command(alias = "pipelines")]
    Pipeline {
        #[command(subcommand)]
        cmd: PipelineCmd,
    },
    /// Pipeline runs (executions).
    #[command(alias = "runs")]
    Run {
        #[command(subcommand)]
        cmd: RunCmd,
    },
    /// Workspace files (shell-like, scoped to one workspace).
    #[command(alias = "files")]
    File {
        #[command(subcommand)]
        cmd: FileCmd,
    },
    /// Open a local file in the browser viewer (no upload).
    Open {
        path: String,
        /// Print the URL instead of launching a browser.
        #[arg(long)]
        print_url: bool,
    },
    /// Conversations.
    #[command(alias = "conv")]
    Conversation {
        #[command(subcommand)]
        cmd: ConversationCmd,
    },
    /// Run the MCP server (for Claude Desktop, Cursor, and other MCP clients).
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Show the current authentication state.
    Status,
    /// Store an `sk_live_` API key in the OS keychain.
    SetKey { key: String },
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// List accessible workspaces.
    List,
    /// Set the default workspace.
    Use { id: String },
}

#[derive(Args)]
struct ChatArgs {
    /// Resume an existing conversation by id.
    #[arg(long)]
    conversation: Option<String>,
}

#[derive(Args)]
struct QueryArgs {
    /// The prompt to send to the agent.
    prompt: String,
    /// Wait for the full response instead of streaming.
    #[arg(long)]
    no_stream: bool,
    /// Continue an existing conversation.
    #[arg(long)]
    conversation: Option<String>,
}

#[derive(Subcommand)]
enum ToolCmd {
    /// List available tools.
    List {
        #[arg(long)]
        category: Option<String>,
    },
    /// Show a tool's parameter schema.
    Show { tool_id: String },
    /// Run a tool directly.
    Run(RunInvocation),
}

#[derive(Subcommand)]
enum PipelineCmd {
    /// List available pipeline definitions.
    List,
    /// Show a pipeline definition.
    Show { pipeline_id: String },
    /// Start a pipeline run.
    Run(RunInvocation),
}

/// Shared invocation args for `tool run` and `pipeline run`.
#[derive(Args)]
struct RunInvocation {
    /// Tool or pipeline id.
    id: String,
    /// Repeatable `key=value` input parameters. Values are parsed as JSON when possible.
    /// Values that look like workspace paths (`@`, `/`, `./`, `../`) are resolved against
    /// the current dir (see `file cd`) — e.g. in /exp2, `./reads.fastq` -> `@exp2/reads.fastq`.
    #[arg(long = "param", short = 'p', value_name = "KEY=VALUE")]
    params: Vec<String>,
    /// JSON object of inputs from a file, `@file`, or `-` for stdin.
    #[arg(long)]
    input: Option<String>,
}

#[derive(Subcommand)]
enum RunCmd {
    /// List runs in the workspace.
    List {
        #[arg(long)]
        status: Option<String>,
    },
    /// Show a run's status.
    Status { id: String },
    /// Poll a run until it reaches a terminal state.
    Watch {
        id: String,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 10)]
        interval: u64,
    },
    /// Cancel a run.
    Cancel { id: String },
}

#[derive(Subcommand)]
enum FileCmd {
    /// List the current directory (or PATH).
    Ls { path: Option<String> },
    /// Change the current directory.
    Cd { path: String },
    /// Print the current directory.
    Pwd,
    /// Upload a local file (defaults to the current directory).
    Upload {
        local: PathBuf,
        #[arg(long)]
        to: Option<String>,
    },
    /// Download a file by key or name.
    Download {
        target: String,
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
    /// Open a workspace file in the browser viewer.
    Open {
        target: String,
        /// Print the viewer URL instead of opening a browser.
        #[arg(long)]
        print_url: bool,
    },
    /// Render a workspace file to a static image (PNG/SVG) and save/open it.
    Render {
        target: String,
        /// Image format: png (default) or svg.
        #[arg(long, default_value = "png")]
        format: String,
        /// Optional region — sequence "start-end" or genomic "chrom:start-end".
        #[arg(long)]
        region: Option<String>,
        /// CSV charts only: chart type (bar-v, line, scatter, pie, heatmap-2d, boxplot, violin, …).
        #[arg(long)]
        chart_type: Option<String>,
        /// Write the image to this path (defaults to the file's name).
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
        /// Print the full-resolution image URL instead of saving/opening.
        #[arg(long)]
        print_url: bool,
    },
    /// Create a folder in the current directory.
    Mkdir { name: String },
    /// Move a file to another folder.
    Mv { target: String, dest: String },
    /// Delete a file by key or name.
    Rm { target: String },
}

#[derive(Subcommand)]
enum ConversationCmd {
    /// List recent conversations.
    List,
    /// Show a conversation's history.
    Show { id: String },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Start the MCP server (stdio).
    Serve,
    /// Register the smarts MCP server into local clients (Claude Desktop, Cursor, …).
    Install(McpInstallArgs),
    /// Remove the smarts MCP server from local clients.
    Uninstall(McpInstallArgs),
}

#[derive(Args)]
struct McpInstallArgs {
    /// Clients to target: claude-desktop, claude-code, cursor, windsurf, gemini-cli, vscode.
    /// Default: every detected client.
    clients: Vec<String>,
    /// Target all supported clients, not just detected ones.
    #[arg(long)]
    all: bool,
    /// Print the config snippet instead of writing it (install only).
    #[arg(long)]
    print: bool,
}

/// Shared handler state.
struct Ctx {
    client: SmartsClient,
    config: Config,
    workspace_flag: Option<String>,
    json: bool,
}

impl Ctx {
    /// Resolve the active workspace: `--workspace` flag, else saved default.
    fn require_workspace(&self) -> Result<String> {
        self.workspace_flag
            .clone()
            .or_else(|| self.config.default_workspace.clone())
            .ok_or_else(|| {
                anyhow!("no workspace selected — pass --workspace <id> or run `smarts workspace use <id>`")
            })
    }

    /// Optional workspace (some tool runs are workspace-agnostic).
    fn optional_workspace(&self) -> Option<String> {
        self.workspace_flag
            .clone()
            .or_else(|| self.config.default_workspace.clone())
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load().context("loading config")?;
    let client = SmartsClient::new(&config).context("initializing client")?;
    let mut ctx = Ctx {
        client,
        config,
        workspace_flag: cli.workspace.clone(),
        json: cli.json,
    };

    let command = match cli.command {
        Some(c) => c,
        None => {
            // Bare `smarts`: chat in a terminal, otherwise print help.
            if std::io::stdin().is_terminal() {
                return chat::run(&ctx, None).await;
            }
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            println!();
            return Ok(());
        }
    };

    match command {
        Command::Chat(args) => chat::run(&ctx, args.conversation).await,
        Command::Login => cmd_login(&mut ctx).await,
        Command::Logout => cmd_logout(),
        Command::Auth { cmd } => cmd_auth(&ctx, cmd).await,
        Command::Workspace { cmd } => cmd_workspace(&mut ctx, cmd).await,
        Command::Query(args) => cmd_query(&ctx, args).await,
        Command::Tool { cmd } => cmd_tool(&ctx, cmd).await,
        Command::Pipeline { cmd } => cmd_pipeline(&ctx, cmd).await,
        Command::Run { cmd } => cmd_run(&ctx, cmd).await,
        Command::File { cmd } => cmd_file(&mut ctx, cmd).await,
        Command::Open { path, print_url } => localopen::open(&path, print_url),
        Command::Conversation { cmd } => cmd_conversation(&ctx, cmd).await,
        Command::Mcp { cmd } => cmd_mcp(&ctx, cmd).await,
    }
}

// ---- Auth -----------------------------------------------------------------

async fn cmd_login(ctx: &mut Ctx) -> Result<()> {
    let info = ctx.client.start_device_login().await?;

    println!("\nTo sign in, open:\n  \x1b[4m{}\x1b[0m", info.verification_uri);
    println!("and enter the code:\n\n      \x1b[1;36m{}\x1b[0m\n", info.user_code);

    let open_url = info
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&info.verification_uri);
    if webbrowser::open(open_url).is_ok() {
        println!("(opening your browser…)");
    }

    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(120));
    spinner.set_message("waiting for you to approve in the browser… (Ctrl-C to cancel)");

    let mut interval = info.interval.max(1);
    let deadline = Instant::now() + Duration::from_secs(info.expires_in);
    loop {
        if Instant::now() >= deadline {
            spinner.finish_and_clear();
            bail!("login timed out — run `smarts login` again");
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        match ctx.client.poll_device_token(&info.device_code).await {
            Ok(DevicePoll::Approved) => {
                spinner.finish_and_clear();
                break;
            }
            Ok(DevicePoll::Pending) => {}
            Ok(DevicePoll::SlowDown) => interval += 5,
            Ok(DevicePoll::Denied) => {
                spinner.finish_and_clear();
                bail!("login was denied in the browser");
            }
            Ok(DevicePoll::Expired) => {
                spinner.finish_and_clear();
                bail!("login request expired — run `smarts login` again");
            }
            // Tolerate transient network errors while polling.
            Err(_) => {}
        }
    }

    // Greet the user and adopt their default workspace if we don't have one.
    match ctx.client.user_profile().await {
        Ok(profile) => {
            let who = first_str(&profile, &["email", "name"])
                .or_else(|| profile.pointer("/data/email").and_then(Value::as_str).map(str::to_string));
            match who {
                Some(w) => println!("✓ Logged in as {w}"),
                None => println!("✓ Logged in"),
            }
            if ctx.config.default_workspace.is_none() {
                if let Some(ws) = first_str(&profile, &["defaultWorkspaceId"])
                    .or_else(|| profile.pointer("/data/defaultWorkspaceId").and_then(Value::as_str).map(str::to_string))
                {
                    ctx.config.default_workspace = Some(ws.clone());
                    let _ = ctx.config.save();
                    println!("  default workspace set to {ws}");
                }
            }
        }
        Err(_) => println!("✓ Logged in"),
    }
    Ok(())
}

fn cmd_logout() -> Result<()> {
    smarts_client::credentials::clear_all()?;
    println!("Logged out — cleared stored credentials.");
    Ok(())
}

async fn cmd_auth(ctx: &Ctx, cmd: AuthCmd) -> Result<()> {
    match cmd {
        AuthCmd::SetKey { key } => {
            if !key.starts_with("sk_live_") {
                bail!("that does not look like an API key (expected an sk_live_ prefix)");
            }
            smarts_client::credentials::set_api_key(&key)?;
            println!("Saved API key to the OS keychain.");
            Ok(())
        }
        AuthCmd::Status => {
            println!("Gateway:  {}", ctx.client.base_url());
            let source = ctx.client.token_source();
            let label = match source {
                TokenSource::None => {
                    println!("Status:   not authenticated");
                    println!(
                        "          run `smarts login`, or set SMARTSBIO_API_KEY / `smarts auth set-key`"
                    );
                    return Ok(());
                }
                TokenSource::Login => "browser login (`smarts login`)",
                TokenSource::EnvApiKey => "API key (SMARTSBIO_API_KEY env)",
                TokenSource::KeychainApiKey => "API key (keychain)",
            };
            println!("Status:   authenticated — {label}");
            if source == TokenSource::Login {
                if let Ok(profile) = ctx.client.user_profile().await {
                    if let Some(email) = first_str(&profile, &["email"])
                        .or_else(|| profile.pointer("/data/email").and_then(Value::as_str).map(str::to_string))
                    {
                        println!("User:     {email}");
                    }
                }
            }
            match ctx.client.list_workspaces().await {
                Ok(ws) => {
                    println!("Access:   {} workspace(s)", ws.len());
                    if let Some(def) = &ctx.config.default_workspace {
                        println!("Default:  {def}");
                    }
                }
                Err(e) => println!("Access:   could not verify ({e})"),
            }
            Ok(())
        }
    }
}

// ---- Workspaces -----------------------------------------------------------

async fn cmd_workspace(ctx: &mut Ctx, cmd: WorkspaceCmd) -> Result<()> {
    match cmd {
        WorkspaceCmd::List => {
            let workspaces = ctx.client.list_workspaces().await?;
            if ctx.json {
                print_json(&serde_json::to_value(&workspaces)?);
                return Ok(());
            }
            let default = ctx.config.default_workspace.clone();
            let mut t = table(&["", "id", "name", "description"]);
            for ws in &workspaces {
                let marker = if Some(&ws.id) == default.as_ref() { "*" } else { "" };
                t.add_row(vec![
                    marker.to_string(),
                    ws.id.clone(),
                    ws.name.clone().unwrap_or_default(),
                    truncate(ws.description.as_deref().unwrap_or(""), 50),
                ]);
            }
            println!("{t}");
            if !workspaces.is_empty() {
                println!("\n* = default workspace");
            }
            Ok(())
        }
        WorkspaceCmd::Use { id } => {
            ctx.config.default_workspace = Some(id.clone());
            ctx.config.save()?;
            println!("Default workspace set to {id}");
            Ok(())
        }
    }
}

// ---- Query ----------------------------------------------------------------

async fn cmd_query(ctx: &Ctx, args: QueryArgs) -> Result<()> {
    let ws = ctx.optional_workspace();
    let ws = ws.as_deref();
    let conv = args.conversation.as_deref();

    if args.no_stream {
        let resp = ctx.client.query(&args.prompt, ws, conv).await?;
        if ctx.json {
            print_json(&resp);
        } else {
            print_query_result(&resp);
        }
        return Ok(());
    }

    let json_mode = ctx.json;
    let mut final_answer: Option<String> = None;
    ctx.client
        .query_stream(&args.prompt, ws, conv, |event| {
            if json_mode {
                println!("{event}");
                return;
            }
            match event.get("status").and_then(Value::as_str) {
                Some("complete") => {
                    if let Some(result) = event.get("result").and_then(Value::as_str) {
                        final_answer = Some(result.to_string());
                    }
                }
                Some("error") => {
                    let msg = event
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    eprintln!("  ⚠ {msg}");
                }
                _ => {
                    // Progress frames go to stderr so stdout stays the answer.
                    if let Some(kind) = event.get("type").and_then(Value::as_str) {
                        let agent = event
                            .get("agent")
                            .and_then(Value::as_str)
                            .unwrap_or(kind);
                        eprintln!("  … {agent}");
                    }
                }
            }
        })
        .await?;

    if let Some(answer) = final_answer {
        println!("{answer}");
    } else if !json_mode {
        eprintln!("(no final answer received)");
    }
    Ok(())
}

fn print_query_result(resp: &Value) {
    let answer = resp
        .get("result")
        .and_then(Value::as_str)
        .or_else(|| resp.pointer("/data/result").and_then(Value::as_str))
        .or_else(|| resp.get("response").and_then(Value::as_str));
    match answer {
        Some(text) => println!("{text}"),
        None => print_json(resp),
    }
}

// ---- Tools ----------------------------------------------------------------

async fn cmd_tool(ctx: &Ctx, cmd: ToolCmd) -> Result<()> {
    match cmd {
        ToolCmd::List { category } => {
            let tools = ctx.client.list_tools(category.as_deref()).await?;
            if ctx.json {
                print_json(&serde_json::to_value(tools_json(&tools))?);
                return Ok(());
            }
            let mut t = table(&["id", "name", "category", "description"]);
            for tool in &tools {
                t.add_row(vec![
                    tool.id.clone().unwrap_or_default(),
                    tool.name.clone().unwrap_or_default(),
                    tool.category.clone().unwrap_or_default(),
                    truncate(tool.description.as_deref().unwrap_or(""), 60),
                ]);
            }
            println!("{t}");
            println!("\n{} tool(s)", tools.len());
            Ok(())
        }
        ToolCmd::Show { tool_id } => {
            let tools = ctx.client.list_tools(None).await?;
            let tool = tools
                .into_iter()
                .find(|t| t.id.as_deref() == Some(tool_id.as_str()))
                .ok_or_else(|| anyhow!("tool '{tool_id}' not found"))?;
            if ctx.json {
                print_json(&tool.parameters.clone().unwrap_or(Value::Null));
                return Ok(());
            }
            println!("{}", tool.name.clone().unwrap_or(tool_id));
            if let Some(desc) = &tool.description {
                println!("{desc}\n");
            }
            match &tool.parameters {
                Some(p) => {
                    println!("Parameters:");
                    print_json(p);
                }
                None => println!("(no parameter schema published)"),
            }
            Ok(())
        }
        ToolCmd::Run(inv) => {
            let ws = ctx.optional_workspace();
            // Resolve path-like params against the workspace cwd (empty when no workspace).
            let cwd = ws
                .as_deref()
                .map(|w| ctx.config.cwd_for(w))
                .unwrap_or_default();
            let input = build_input(&inv, &cwd)?;
            let result = ctx.client.run_tool(&inv.id, ws.as_deref(), input).await?;
            if ctx.json {
                print_json(&result);
                return Ok(());
            }
            // Unwrap the tool-result envelope ({ result: { data, metadata }, status }) down to
            // the payload so output matches `pipeline run`: async tools (BLAST, Boltz, …) that
            // submit a background job carry a `processId` — print the clean "Started run <id>"
            // line. Synchronous tools print just their data payload, not the metadata noise.
            let payload = result
                .pointer("/result/data")
                .or_else(|| result.pointer("/data"))
                .unwrap_or(&result);
            match payload.get("processId").and_then(Value::as_str) {
                Some(id) => println!("Started run {id}"),
                None => print_json(payload),
            }
            Ok(())
        }
    }
}

fn tools_json(tools: &[smarts_client::models::ToolInfo]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "name": t.name,
                "category": t.category,
                "description": t.description,
            })
        })
        .collect()
}

// ---- Pipelines & Runs -----------------------------------------------------

/// Human-friendly render of a pipeline definition: name/description, the input schema
/// (param name, required, default, description) and the step list, plus a run hint.
fn render_pipeline_def(def: &Value) {
    let id = first_str(def, &["id", "pipelineId"]).unwrap_or_default();
    let name = first_str(def, &["name"]).unwrap_or_else(|| id.clone());
    println!("{name}  ({id})");
    if let Some(desc) = first_str(def, &["description"]) {
        println!("{desc}");
    }

    if let Some(inputs) = def.get("inputs").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        println!("\nInputs:");
        let mut t = table(&["param", "required", "default", "description"]);
        for inp in inputs {
            let pname = first_str(inp, &["name"]).unwrap_or_default();
            let default_val = inp.get("default").filter(|v| !v.is_null());
            let required = if default_val.is_some() { "no" } else { "yes" };
            let default_str = default_val
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let pdesc = first_str(inp, &["description"]).unwrap_or_default();
            t.add_row(vec![pname, required.to_string(), default_str, truncate(&pdesc, 70)]);
        }
        println!("{t}");
    }

    if let Some(steps) = def.get("steps").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        println!("\nSteps:");
        for (i, s) in steps.iter().enumerate() {
            let tool = first_str(s, &["toolName"]).unwrap_or_default();
            let sdesc = first_str(s, &["description"]).unwrap_or_default();
            if sdesc.is_empty() {
                println!("  {}. {tool}", i + 1);
            } else {
                println!("  {}. {tool}  —  {}", i + 1, truncate(&sdesc, 70));
            }
        }
    }

    if !id.is_empty() {
        println!("\nRun:  smarts pipeline run {id} --param <name>=<value> ...");
    }
}

async fn cmd_pipeline(ctx: &Ctx, cmd: PipelineCmd) -> Result<()> {
    match cmd {
        PipelineCmd::List => {
            let defs = ctx.client.list_pipeline_defs().await?;
            if ctx.json {
                print_json(&defs);
                return Ok(());
            }
            let items = extract_array(&defs);
            let mut t = table(&["id", "name", "category", "description"]);
            for item in &items {
                t.add_row(vec![
                    first_str(item, &["id", "pipelineId", "_id"]).unwrap_or_default(),
                    first_str(item, &["name"]).unwrap_or_default(),
                    first_str(item, &["category"]).unwrap_or_default(),
                    truncate(&first_str(item, &["description"]).unwrap_or_default(), 60),
                ]);
            }
            println!("{t}");
            println!("\n{} pipeline(s)", items.len());
            Ok(())
        }
        PipelineCmd::Show { pipeline_id } => {
            // Prefer the agent's real definition (the actual input schema + steps) over the
            // catalog metadata, so users see exactly which parameters to pass.
            match ctx.client.get_pipeline_def(&pipeline_id).await {
                Ok(def) if def.get("inputs").is_some() || def.get("steps").is_some() => {
                    if ctx.json {
                        print_json(&def);
                    } else {
                        render_pipeline_def(&def);
                    }
                    Ok(())
                }
                // Fallback: catalog-metadata lookup (older gateway, or a non-agent pipeline).
                _ => {
                    let defs = ctx.client.list_pipeline_defs().await?;
                    let item = extract_array(&defs).into_iter().find(|it| {
                        first_str(it, &["id", "pipelineId", "_id"]).as_deref()
                            == Some(pipeline_id.as_str())
                    });
                    match item {
                        Some(it) => print_json(&it),
                        None => bail!("pipeline '{pipeline_id}' not found"),
                    }
                    Ok(())
                }
            }
        }
        PipelineCmd::Run(inv) => {
            let ws = ctx.require_workspace()?;
            let cwd = ctx.config.cwd_for(&ws);
            let input = build_input(&inv, &cwd)?;
            let result = ctx.client.run_pipeline(&inv.id, &ws, input).await?;
            if ctx.json {
                print_json(&result);
                return Ok(());
            }
            match run_id_of(&result) {
                Some(id) => println!("Started run {id}"),
                None => print_json(&result),
            }
            Ok(())
        }
    }
}

async fn cmd_run(ctx: &Ctx, cmd: RunCmd) -> Result<()> {
    let ws = ctx.require_workspace()?;
    match cmd {
        RunCmd::List { status } => {
            let runs = ctx.client.list_runs(&ws, status.as_deref()).await?;
            if ctx.json {
                print_json(&runs);
                return Ok(());
            }
            let items = extract_array(&runs);
            let mut t = table(&["id", "status", "tool", "created"]);
            for item in &items {
                t.add_row(vec![
                    first_str(item, &["id", "processId", "_id"]).unwrap_or_default(),
                    first_str(item, &["status", "state"]).unwrap_or_default(),
                    first_str(item, &["toolName", "tool_id", "tool"]).unwrap_or_default(),
                    first_str(item, &["createdAt", "created_at", "queuedAt"]).unwrap_or_default(),
                ]);
            }
            println!("{t}");
            println!("\n{} run(s)", items.len());
            Ok(())
        }
        RunCmd::Status { id } => {
            let status = ctx.client.run_status(&id, &ws).await?;
            print_json(&status);
            Ok(())
        }
        RunCmd::Watch { id, interval } => watch_run(ctx, &ws, &id, interval).await,
        RunCmd::Cancel { id } => {
            ctx.client.cancel_run(&id, &ws).await?;
            println!("Cancelled run {id}");
            Ok(())
        }
    }
}

const TERMINAL_STATES: [&str; 8] = [
    "completed",
    "complete",
    "succeeded",
    "success",
    "failed",
    "error",
    "cancelled",
    "canceled",
];

/// A pipeline step as surfaced by `GET /v1/pipelines/:id`.
struct StepView {
    step_id: String,
    label: String,
    description: String,
    status: String,
    error: Option<String>,
}

/// Pull the pipeline step list out of a run-status payload (top-level or under `data`).
/// Returns empty for non-pipeline runs (e.g. a single tool), which drives the fallback.
fn extract_steps(resp: &Value) -> Vec<StepView> {
    let arr = resp
        .get("steps")
        .and_then(Value::as_array)
        .or_else(|| resp.pointer("/data/steps").and_then(Value::as_array));
    let Some(arr) = arr else {
        return Vec::new();
    };
    arr.iter()
        .map(|s| {
            let step_id = first_str(s, &["stepId", "id"]).unwrap_or_default();
            let label = first_str(s, &["name", "toolName"]).unwrap_or_else(|| step_id.clone());
            // Build a concise "CODE — message" error string when the step carries one.
            let error = s.get("error").filter(|e| !e.is_null()).and_then(|e| {
                match (first_str(e, &["code"]), first_str(e, &["message"])) {
                    (Some(c), Some(m)) => Some(format!("{c} — {m}")),
                    (None, Some(m)) => Some(m),
                    (Some(c), None) => Some(c),
                    _ => None,
                }
            });
            StepView {
                step_id,
                label,
                description: first_str(s, &["description"]).unwrap_or_default(),
                status: first_str(s, &["status"]).unwrap_or_else(|| "pending".into()),
                error,
            }
        })
        .collect()
}

/// Print a compact end-of-run summary for a pipeline: outcome + duration, any failed
/// step(s) with their error, and where the outputs landed. No-op for non-pipeline runs.
fn print_run_summary(state: &str, resp: &Value, steps: &[StepView]) {
    let lower = state.to_lowercase();

    // Single (non-pipeline) runs have no steps — still surface the outcome and the failure
    // reason (e.g. a quota block) so the user isn't left guessing why a run ended.
    if steps.is_empty() {
        match lower.as_str() {
            "failed" | "error" => match run_error_of(resp) {
                Some(e) => println!("\n✗ Run failed: {e}"),
                None => println!("\n✗ Run failed"),
            },
            "cancelled" | "canceled" => println!("\n⊘ Run cancelled"),
            _ => {}
        }
        return;
    }
    let succeeded = matches!(lower.as_str(), "completed" | "complete" | "success" | "succeeded");
    let dur = resp
        .get("durationMs")
        .and_then(Value::as_u64)
        .map(|ms| format!(" in {}", fmt_duration(Duration::from_millis(ms))))
        .unwrap_or_default();

    println!();
    let outcome = match lower.as_str() {
        _ if succeeded => format!("✓ Pipeline completed{dur}"),
        "failed" | "error" => format!("✗ Pipeline failed{dur}"),
        "cancelled" | "canceled" => format!("⊘ Pipeline cancelled{dur}"),
        other => format!("Pipeline {other}{dur}"),
    };
    println!("{outcome}");

    // On a non-success outcome, surface the failing step(s) and their error.
    if !succeeded {
        for s in steps
            .iter()
            .filter(|s| matches!(s.status.to_lowercase().as_str(), "failed" | "error"))
        {
            match &s.error {
                Some(e) => println!("  ✗ {}: {e}", s.label),
                None => println!("  ✗ {} failed", s.label),
            }
        }
    }

    // Where the outputs landed, plus a ready-to-run browse command.
    if let Some(folder) = resp
        .get("outputFolder")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty())
    {
        println!("  Outputs: /pipelines/{folder}/");
        println!("  Browse:  smarts file ls /pipelines/{folder}");
    }
}

fn step_is_done(status: &str) -> bool {
    matches!(
        status.to_lowercase().as_str(),
        "completed" | "complete" | "success" | "succeeded"
    )
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn spinner_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_spinner())
}

/// Update a step's bar: running steps keep an animated spinner; terminal/pending
/// steps show a static status glyph. `elapsed` is a live local stopwatch (running only).
fn apply_step_bar(bar: &ProgressBar, s: &StepView, elapsed: Option<&str>) {
    let lower = s.status.to_lowercase();
    let desc = if s.description.is_empty() {
        String::new()
    } else {
        format!("  —  {}", truncate(&s.description, 48))
    };

    if lower == "running" {
        bar.set_style(spinner_style("  {spinner:.cyan} {msg}"));
        let elapsed = elapsed.map(|e| format!("  ({e})")).unwrap_or_default();
        bar.set_message(format!("{}{desc}{elapsed}", s.label));
    } else {
        let glyph = match lower.as_str() {
            "completed" | "complete" | "success" | "succeeded" => "✓",
            "failed" | "error" => "✗",
            "cancelled" | "canceled" => "⊘",
            "skipped" => "–",
            _ => "○", // pending / queued / waiting
        };
        let suffix = match lower.as_str() {
            "pending" | "queued" | "waiting" => "  (pending)",
            _ => "",
        };
        bar.set_style(spinner_style("  {msg}"));
        bar.set_message(format!("{glyph} {}{desc}{suffix}", s.label));
    }
}

async fn watch_run(ctx: &Ctx, ws: &str, id: &str, interval: u64) -> Result<()> {
    // JSON mode: poll quietly until terminal, then emit the raw payload (no live UI).
    if ctx.json {
        loop {
            let resp = ctx.client.run_status(id, ws).await?;
            let state = run_state_of(&resp).unwrap_or_else(|| "unknown".into());
            if TERMINAL_STATES.contains(&state.to_lowercase().as_str()) {
                print_json(&resp);
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(interval.max(1))).await;
        }
    }

    let mp = MultiProgress::new();
    let header = mp.add(ProgressBar::new_spinner());
    header.set_style(spinner_style("{spinner:.green} {msg}"));
    header.enable_steady_tick(Duration::from_millis(120));

    // One bar per pipeline step, created lazily once the run first reports steps.
    let mut step_bars: Vec<(String, ProgressBar)> = Vec::new();
    // Per-step local stopwatch (started when we first see it running) so we can show a
    // live elapsed counter without parsing server timestamps / pulling in a date lib.
    let mut step_clock: HashMap<String, Instant> = HashMap::new();

    loop {
        let resp = ctx.client.run_status(id, ws).await?;
        let state = run_state_of(&resp).unwrap_or_else(|| "unknown".into());
        let steps = extract_steps(&resp);

        if step_bars.is_empty() && !steps.is_empty() {
            for s in &steps {
                let bar = mp.add(ProgressBar::new_spinner());
                bar.enable_steady_tick(Duration::from_millis(120));
                step_bars.push((s.step_id.clone(), bar));
            }
        }

        let total = steps.len();
        let done = steps.iter().filter(|s| step_is_done(&s.status)).count();
        let counts = if total > 0 {
            format!("  [{done}/{total}]")
        } else {
            String::new()
        };
        header.set_message(format!("run {id}  ·  {state}{counts}"));

        for s in &steps {
            if let Some((_, bar)) = step_bars.iter().find(|(sid, _)| sid == &s.step_id) {
                if s.status.eq_ignore_ascii_case("running") {
                    step_clock.entry(s.step_id.clone()).or_insert_with(Instant::now);
                }
                let elapsed = step_clock.get(&s.step_id).map(|t| fmt_duration(t.elapsed()));
                apply_step_bar(bar, s, elapsed.as_deref());
            }
        }

        if TERMINAL_STATES.contains(&state.to_lowercase().as_str()) {
            header.set_style(spinner_style("{msg}"));
            header.finish_with_message(format!("run {id}  ·  {state}{counts}"));
            for (_, bar) in &step_bars {
                bar.finish();
            }
            print_run_summary(&state, &resp, &steps);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(interval.max(1))).await;
    }
}

fn run_id_of(value: &Value) -> Option<String> {
    for ptr in ["/data/processId", "/data/id", "/data/_id"] {
        if let Some(s) = value.pointer(ptr).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    first_str(value, &["processId", "id", "_id"])
}

/// Pull a human-readable failure reason out of a run-status payload. The error may be a
/// plain string or an object `{ code, message, details }`, and may sit at the top level,
/// under `/data`, or under `/execution` (depending on whether the gateway normalized it).
/// This is what surfaces "Compute quota exceeded" etc. instead of a bare "failed".
fn run_error_of(value: &Value) -> Option<String> {
    for base in ["/error", "/data/error", "/execution/error"] {
        match value.pointer(base) {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
            Some(obj @ Value::Object(_)) => {
                let msg = first_str(obj, &["message"]);
                let code = first_str(obj, &["code"]);
                match (code, msg) {
                    (Some(c), Some(m)) => return Some(format!("{m} ({c})")),
                    (_, Some(m)) => return Some(m),
                    (Some(c), _) => return Some(c),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    None
}

fn run_state_of(value: &Value) -> Option<String> {
    for ptr in ["/data/status", "/data/state"] {
        if let Some(s) = value.pointer(ptr).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    first_str(value, &["status", "state"])
}

// ---- Files ----------------------------------------------------------------

async fn cmd_file(ctx: &mut Ctx, cmd: FileCmd) -> Result<()> {
    let ws = ctx.require_workspace()?;
    let cwd = ctx.config.cwd_for(&ws);

    match cmd {
        FileCmd::Ls { path } => {
            let target = match &path {
                Some(p) => resolve_path(&cwd, p),
                None => cwd.clone(),
            };
            let items = ctx.client.list_files(&ws, &target).await?;
            if ctx.json {
                print_json(&serde_json::to_value(file_items_json(&items))?);
                return Ok(());
            }
            println!("📁 /{target}");
            let mut t = table(&["name", "type", "size", "modified"]);
            for item in &items {
                let size = if item.is_folder() {
                    "-".to_string()
                } else {
                    item.size.map(human_size).unwrap_or_default()
                };
                t.add_row(vec![
                    item.name.clone().unwrap_or_default(),
                    item.kind.clone().unwrap_or_default(),
                    size,
                    item.last_modified.clone().unwrap_or_default(),
                ]);
            }
            println!("{t}");
            Ok(())
        }
        FileCmd::Cd { path } => {
            let target = resolve_path(&cwd, &path);
            ctx.config.set_cwd(&ws, &target);
            ctx.config.save()?;
            println!("/{target}");
            Ok(())
        }
        FileCmd::Pwd => {
            println!("/{cwd}");
            Ok(())
        }
        FileCmd::Upload { local, to } => {
            let dest = match &to {
                Some(p) => resolve_path(&cwd, p),
                None => cwd.clone(),
            };
            if !local.exists() {
                bail!("local file not found: {}", local.display());
            }
            let spinner = ProgressBar::new_spinner();
            spinner.enable_steady_tick(Duration::from_millis(120));
            spinner.set_message(format!("uploading {}", local.display()));
            let result = ctx.client.upload_file(&ws, &local, &dest).await;
            spinner.finish_and_clear();
            let result = result?;
            if ctx.json {
                print_json(&result);
            } else {
                let key = first_str(&result, &["fileKey", "key"])
                    .or_else(|| result.pointer("/data/fileKey").and_then(Value::as_str).map(str::to_string))
                    .or_else(|| result.pointer("/data/key").and_then(Value::as_str).map(str::to_string));
                match key {
                    Some(k) => println!("Uploaded → {k}"),
                    None => println!("Uploaded."),
                }
            }
            Ok(())
        }
        FileCmd::Download { target, output } => {
            let key = resolve_file_key(ctx, &ws, &cwd, &target).await?;
            let out = output.unwrap_or_else(|| {
                PathBuf::from(key.rsplit('/').next().unwrap_or("download"))
            });
            let bytes = ctx.client.download_bytes(&ws, &key).await?;
            let mut f = std::fs::File::create(&out)
                .with_context(|| format!("creating {}", out.display()))?;
            f.write_all(&bytes)?;
            println!("Downloaded {} ({}) → {}", key, human_size(bytes.len() as u64), out.display());
            Ok(())
        }
        FileCmd::Open { target, print_url } => {
            // `file open` is for workspace files; nudge toward `smarts open` for local paths.
            if std::path::Path::new(&target).is_file() {
                bail!(
                    "'{target}' is a local file — use `smarts open {target}` to view it \
                     (no upload). `file open` is for files already in the workspace."
                );
            }
            let key = resolve_file_key(ctx, &ws, &cwd, &target).await?;
            let url = ctx.client.viewer_url(&ws, &key).await?;
            if print_url || ctx.json {
                if ctx.json {
                    print_json(&json!({ "viewer_url": url }));
                } else {
                    println!("{url}");
                }
                return Ok(());
            }
            println!("Opening {url}");
            if let Err(e) = webbrowser::open(&url) {
                eprintln!("could not open a browser ({e}); URL: {url}");
            }
            Ok(())
        }
        FileCmd::Render { target, format, region, chart_type, output, print_url } => {
            let key = resolve_file_key(ctx, &ws, &cwd, &target).await?;
            let resp = ctx
                .client
                .render_view(&ws, &key, Some(&format), region.as_deref(), chart_type.as_deref())
                .await?;
            if ctx.json {
                print_json(&resp);
                return Ok(());
            }
            let image_url = resp
                .get("image_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if print_url {
                println!("{image_url}");
                return Ok(());
            }
            if image_url.is_empty() {
                bail!("render did not return an image URL");
            }
            let fmt = resp.get("format").and_then(Value::as_str).unwrap_or(&format);
            let out = output.unwrap_or_else(|| {
                let name = key.rsplit('/').next().unwrap_or("render");
                let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
                PathBuf::from(format!("{stem}.{fmt}"))
            });
            let bytes = ctx.client.fetch_url_bytes(&image_url).await?;
            std::fs::File::create(&out)
                .with_context(|| format!("creating {}", out.display()))?
                .write_all(&bytes)?;
            println!("Rendered {key} ({}) → {}", human_size(bytes.len() as u64), out.display());
            if let Err(e) = webbrowser::open(&out.to_string_lossy()) {
                eprintln!("saved but could not open it ({e})");
            }
            Ok(())
        }
        FileCmd::Mkdir { name } => {
            let result = ctx.client.create_folder(&ws, &name, &cwd).await?;
            if ctx.json {
                print_json(&result);
            } else {
                println!("Created folder /{}", resolve_path(&cwd, &name));
            }
            Ok(())
        }
        FileCmd::Mv { target, dest } => {
            let key = resolve_file_key(ctx, &ws, &cwd, &target).await?;
            let dest_path = resolve_path(&cwd, &dest);
            let result = ctx.client.move_file(&ws, &key, &dest_path).await?;
            if ctx.json {
                print_json(&result);
            } else {
                println!("Moved → /{dest_path}");
            }
            Ok(())
        }
        FileCmd::Rm { target } => {
            let key = resolve_file_key(ctx, &ws, &cwd, &target).await?;
            ctx.client.delete_file(&ws, &key).await?;
            println!("Deleted {key}");
            Ok(())
        }
    }
}

/// Resolve a user-supplied file argument to a storage key: pass through full
/// keys (anything containing `/`), otherwise look up a file by name in the cwd.
async fn resolve_file_key(ctx: &Ctx, ws: &str, cwd: &str, target: &str) -> Result<String> {
    if target.contains('/') {
        return Ok(target.to_string());
    }
    let items = ctx.client.list_files(ws, cwd).await?;
    items
        .into_iter()
        .find(|it| it.name.as_deref() == Some(target) && !it.is_folder())
        .and_then(|it| it.key)
        .ok_or_else(|| anyhow!("no file named '{target}' in /{cwd} (pass a full key to be explicit)"))
}

fn file_items_json(items: &[smarts_client::models::FileItem]) -> Vec<Value> {
    items
        .iter()
        .map(|it| {
            json!({
                "key": it.key,
                "name": it.name,
                "type": it.kind,
                "size": it.size,
                "lastModified": it.last_modified,
                "format": it.format,
            })
        })
        .collect()
}

// ---- Conversations --------------------------------------------------------

async fn cmd_conversation(ctx: &Ctx, cmd: ConversationCmd) -> Result<()> {
    match cmd {
        ConversationCmd::List => {
            let convs = ctx.client.list_conversations().await?;
            if ctx.json {
                print_json(&convs);
                return Ok(());
            }
            let items = extract_array(&convs);
            let mut t = table(&["id", "title", "updated"]);
            for item in &items {
                t.add_row(vec![
                    first_str(item, &["id", "_id", "conversationId", "sessionId"]).unwrap_or_default(),
                    truncate(&first_str(item, &["title", "name", "summary"]).unwrap_or_default(), 50),
                    first_str(item, &["updatedAt", "updated_at", "lastMessageAt"]).unwrap_or_default(),
                ]);
            }
            println!("{t}");
            Ok(())
        }
        ConversationCmd::Show { id } => {
            let conv = ctx.client.get_conversation(&id).await?;
            print_json(&conv);
            Ok(())
        }
    }
}

// ---- Stubs for later phases ----------------------------------------------

async fn cmd_mcp(ctx: &Ctx, cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::Install(args) => mcp_install::run_install(args.clients, args.all, args.print),
        McpCmd::Uninstall(args) => mcp_install::run_uninstall(args.clients, args.all),
        McpCmd::Serve => {
            // stdout is the MCP protocol channel — must not print anything to it.
            if !ctx.client.has_credentials() {
                eprintln!(
                    "warning: not authenticated — run `smarts login` (or set SMARTSBIO_API_KEY) \
                     so the MCP tools can reach your account."
                );
            }
            eprintln!("smarts MCP server ready on stdio.");
            smarts_mcp::serve_stdio(ctx.client.clone(), ctx.config.default_workspace.clone())
                .await
                .map_err(|e| anyhow!(e))
        }
    }
}

// ---- Input parsing --------------------------------------------------------

/// Build the `input` object for a tool/pipeline run from `--input` plus
/// repeated `--param key=value` overrides.
///
/// `cwd` is the current workspace directory (from `file cd`); path-like `--param`
/// values are resolved against it. Pass `""` when there is no workspace context.
fn build_input(inv: &RunInvocation, cwd: &str) -> Result<Value> {
    let mut value = match &inv.input {
        Some(src) => {
            let text = read_input_source(src)?;
            serde_json::from_str::<Value>(&text)
                .with_context(|| "parsing --input as JSON".to_string())?
        }
        None => json!({}),
    };
    if !value.is_object() {
        bail!("--input must be a JSON object");
    }
    let map = value.as_object_mut().expect("checked is_object");
    for pair in &inv.params {
        let (key, raw) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --param '{pair}', expected key=value"))?;
        map.insert(key.to_string(), coerce_param_value(raw, cwd));
    }
    Ok(value)
}

/// Read an `--input` source: `-` (stdin), `@file`, or a plain path.
fn read_input_source(src: &str) -> Result<String> {
    if src == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        return Ok(buf);
    }
    let path = src.strip_prefix('@').unwrap_or(src);
    std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
}

/// Parse a `--param` value as JSON when it is valid (numbers, bools, arrays,
/// objects, quoted strings), otherwise keep it as a bare string.
fn coerce_value(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// True when a `--param` value carries an explicit workspace-path sigil (`@`, `/`,
/// `./`, `../`). Only such values are resolved against the cwd, so plain string
/// params (e.g. `sampleName=samp1`) are never rewritten. A bare filename is left
/// untouched because it is ambiguous with an ordinary string value.
fn is_workspace_path(raw: &str) -> bool {
    raw.starts_with('@')
        || raw.starts_with('/')
        || raw.starts_with("./")
        || raw.starts_with("../")
}

/// Coerce a `--param` value, resolving workspace file paths against `cwd`.
///
/// A value with an explicit path sigil is normalized against the current directory and
/// emitted in the canonical `@`-anchored form the API understands (e.g. in `/exp2`,
/// `./test_reads.fastq` -> `@exp2/test_reads.fastq`). Everything else falls back to
/// JSON/string coercion.
fn coerce_param_value(raw: &str, cwd: &str) -> Value {
    if is_workspace_path(raw) {
        Value::String(format!("@{}", resolve_path(cwd, raw)))
    } else {
        coerce_value(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_detects_json_and_falls_back_to_string() {
        assert_eq!(coerce_value("42"), json!(42));
        assert_eq!(coerce_value("true"), json!(true));
        assert_eq!(coerce_value("[1,2]"), json!([1, 2]));
        assert_eq!(coerce_value("BLASTN"), json!("BLASTN"));
    }

    #[test]
    fn build_input_merges_params_over_input() {
        let inv = RunInvocation {
            id: "x".into(),
            params: vec!["evalue=0.01".into(), "program=blastn".into()],
            input: None,
        };
        let v = build_input(&inv, "").unwrap();
        assert_eq!(v["evalue"], json!(0.01));
        assert_eq!(v["program"], json!("blastn"));
    }

    #[test]
    fn build_input_rejects_bad_param() {
        let inv = RunInvocation {
            id: "x".into(),
            params: vec!["noequals".into()],
            input: None,
        };
        assert!(build_input(&inv, "").is_err());
    }

    #[test]
    fn is_workspace_path_requires_explicit_sigil() {
        assert!(is_workspace_path("@exp2/x.fastq"));
        assert!(is_workspace_path("/exp2/x.fastq"));
        assert!(is_workspace_path("./x.fastq"));
        assert!(is_workspace_path("../x.fastq"));
        // bare strings/filenames are NOT treated as paths (ambiguous with values)
        assert!(!is_workspace_path("x.fastq"));
        assert!(!is_workspace_path("samp1"));
        assert!(!is_workspace_path("blastn"));
    }

    #[test]
    fn coerce_param_resolves_relative_path_against_cwd() {
        // In /exp2, "./test_reads.fastq" -> "@exp2/test_reads.fastq"
        assert_eq!(
            coerce_param_value("./test_reads.fastq", "exp2"),
            json!("@exp2/test_reads.fastq")
        );
        // bare relative (no ./) is unchanged unless an explicit absolute/parent form
        assert_eq!(
            coerce_param_value("../shared/ref.fa", "exp2/sub"),
            json!("@exp2/shared/ref.fa")
        );
    }

    #[test]
    fn coerce_param_absolute_and_at_paths_are_idempotent() {
        // cwd is irrelevant for absolute forms
        assert_eq!(
            coerce_param_value("@exp2/test_reads.fastq", "other"),
            json!("@exp2/test_reads.fastq")
        );
        assert_eq!(
            coerce_param_value("/exp2/test_reads.fastq", "other"),
            json!("@exp2/test_reads.fastq")
        );
    }

    #[test]
    fn coerce_param_leaves_non_paths_alone() {
        assert_eq!(coerce_param_value("samp1", "exp2"), json!("samp1"));
        assert_eq!(coerce_param_value("42", "exp2"), json!(42));
        assert_eq!(coerce_param_value("true", "exp2"), json!(true));
    }

    #[test]
    fn build_input_resolves_path_params_with_cwd() {
        let inv = RunInvocation {
            id: "quality-control".into(),
            params: vec![
                "inputFastqR1=./test_reads.fastq".into(),
                "sampleName=samp1".into(),
            ],
            input: None,
        };
        let v = build_input(&inv, "exp2").unwrap();
        assert_eq!(v["inputFastqR1"], json!("@exp2/test_reads.fastq"));
        assert_eq!(v["sampleName"], json!("samp1"));
    }

    #[test]
    fn extract_steps_parses_top_level_and_nested() {
        let resp = json!({
            "status": "running",
            "steps": [
                { "stepId": "step1_fastqc", "name": "fastqc", "description": "QC", "status": "completed" },
                { "stepId": "step2_trim", "toolName": "trimmomatic", "status": "running" }
            ]
        });
        let steps = extract_steps(&resp);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].label, "fastqc");
        assert_eq!(steps[0].status, "completed");
        // falls back to toolName when name is absent
        assert_eq!(steps[1].label, "trimmomatic");

        // also reads from a `data`-wrapped payload
        let wrapped = json!({ "data": { "steps": [ { "stepId": "s", "status": "pending" } ] } });
        assert_eq!(extract_steps(&wrapped).len(), 1);
    }

    #[test]
    fn extract_steps_empty_for_non_pipeline() {
        let resp = json!({ "status": "running", "toolName": "blastn" });
        assert!(extract_steps(&resp).is_empty());
    }

    #[test]
    fn extract_steps_captures_step_error() {
        let resp = json!({
            "status": "failed",
            "steps": [
                { "stepId": "s1", "name": "fastqc", "status": "completed", "error": null },
                { "stepId": "s2", "name": "trimmomatic", "status": "failed",
                  "error": { "code": "QUOTA_EXCEEDED", "message": "Compute quota exceeded" } }
            ]
        });
        let steps = extract_steps(&resp);
        assert_eq!(steps[0].error, None);
        assert_eq!(steps[1].error.as_deref(), Some("QUOTA_EXCEEDED — Compute quota exceeded"));
    }

    #[test]
    fn step_is_done_and_duration_format() {
        assert!(step_is_done("completed"));
        assert!(step_is_done("SUCCESS"));
        assert!(!step_is_done("running"));
        assert!(!step_is_done("failed"));
        assert_eq!(fmt_duration(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_duration(Duration::from_secs(220)), "3m40s");
    }
}
