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

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
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
    /// Repeatable `key=value` input parameters (values are parsed as JSON when possible).
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
        #[arg(long, default_value_t = 5)]
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
            let input = build_input(&inv)?;
            let ws = ctx.optional_workspace();
            let result = ctx.client.run_tool(&inv.id, ws.as_deref(), input).await?;
            print_json(&result);
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
            let defs = ctx.client.list_pipeline_defs().await?;
            let item = extract_array(&defs).into_iter().find(|it| {
                first_str(it, &["id", "pipelineId", "_id"]).as_deref() == Some(pipeline_id.as_str())
            });
            match item {
                Some(it) => print_json(&it),
                None => bail!("pipeline '{pipeline_id}' not found"),
            }
            Ok(())
        }
        PipelineCmd::Run(inv) => {
            let ws = ctx.require_workspace()?;
            let input = build_input(&inv)?;
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

async fn watch_run(ctx: &Ctx, ws: &str, id: &str, interval: u64) -> Result<()> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner} {msg}").unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spinner.enable_steady_tick(Duration::from_millis(120));

    loop {
        let status_resp = ctx.client.run_status(id, ws).await?;
        let state = run_state_of(&status_resp).unwrap_or_else(|| "unknown".into());
        spinner.set_message(format!("run {id}: {state}"));

        if TERMINAL_STATES.contains(&state.to_lowercase().as_str()) {
            spinner.finish_with_message(format!("run {id}: {state}"));
            if ctx.json {
                print_json(&status_resp);
            }
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
fn build_input(inv: &RunInvocation) -> Result<Value> {
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
        map.insert(key.to_string(), coerce_value(raw));
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
        let v = build_input(&inv).unwrap();
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
        assert!(build_input(&inv).is_err());
    }
}
