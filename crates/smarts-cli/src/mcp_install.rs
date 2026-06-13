//! `smarts mcp install` — register the smarts MCP server into local MCP clients.
//!
//! Writes the absolute path to this binary (so GUI apps with a minimal PATH can
//! still launch it) into each client's config, merging without clobbering other
//! servers and backing up the file first. JSON-file clients are edited directly;
//! Claude Code and VS Code are configured via their own CLIs. ChatGPT and other
//! hosted clients can't be done locally — they need the hosted MCP URL.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

const SERVER_NAME: &str = "smarts";

/// Clients that take a JSON config file with an `mcpServers` object.
/// (label, returns config path, parent dir that signals the app is installed)
fn json_file_clients() -> Vec<(&'static str, &'static str, Option<PathBuf>, Option<PathBuf>)> {
    let home = dirs_home();
    let config = dirs_config();
    vec![
        (
            "claude-desktop",
            "Claude Desktop",
            config.as_ref().map(|c| c.join("Claude/claude_desktop_config.json")),
            config.as_ref().map(|c| c.join("Claude")),
        ),
        (
            "cursor",
            "Cursor",
            home.as_ref().map(|h| h.join(".cursor/mcp.json")),
            home.as_ref().map(|h| h.join(".cursor")),
        ),
        (
            "windsurf",
            "Windsurf",
            home.as_ref().map(|h| h.join(".codeium/windsurf/mcp_config.json")),
            home.as_ref().map(|h| h.join(".codeium/windsurf")),
        ),
        (
            "gemini-cli",
            "Gemini CLI",
            home.as_ref().map(|h| h.join(".gemini/settings.json")),
            home.as_ref().map(|h| h.join(".gemini")),
        ),
    ]
}

/// All client ids we know about (for `--all` and help).
const ALL_CLIENTS: &[&str] = &[
    "claude-desktop",
    "claude-code",
    "cursor",
    "windsurf",
    "gemini-cli",
    "vscode",
];

pub fn run_install(clients: Vec<String>, all: bool, print: bool) -> Result<()> {
    let exe = current_exe_path()?;
    let base_url = std::env::var("SMARTSBIO_BASE_URL").ok().filter(|s| !s.is_empty());

    if print {
        print_snippet(&exe, base_url.as_deref());
        return Ok(());
    }

    let targets = resolve_targets(&clients, all)?;
    if targets.is_empty() {
        println!(
            "No supported MCP clients detected. Target one explicitly, e.g.:\n  \
             smarts mcp install claude-desktop\n  \
             smarts mcp install --all\n  \
             smarts mcp install --print     # show the config to paste manually"
        );
        return Ok(());
    }

    for id in targets {
        match install_one(&id, &exe, base_url.as_deref()) {
            Ok(msg) => println!("✓ {id}: {msg}"),
            Err(e) => println!("✗ {id}: {e}"),
        }
    }
    println!("\nRestart the client(s) to pick up the smarts MCP server.");
    Ok(())
}

pub fn run_uninstall(clients: Vec<String>, all: bool) -> Result<()> {
    let targets = resolve_targets(&clients, all)?;
    if targets.is_empty() {
        println!("No clients detected. Pass a client name or --all.");
        return Ok(());
    }
    for id in targets {
        match uninstall_one(&id) {
            Ok(msg) => println!("✓ {id}: {msg}"),
            Err(e) => println!("✗ {id}: {e}"),
        }
    }
    Ok(())
}

/// Which clients to act on: explicit names, `--all`, or auto-detected.
fn resolve_targets(clients: &[String], all: bool) -> Result<Vec<String>> {
    if !clients.is_empty() {
        for c in clients {
            if !ALL_CLIENTS.contains(&c.as_str()) && c != "chatgpt" {
                bail!(
                    "unknown client '{c}'. Supported: {}",
                    ALL_CLIENTS.join(", ")
                );
            }
        }
        return Ok(clients.to_vec());
    }
    if all {
        return Ok(ALL_CLIENTS.iter().map(|s| s.to_string()).collect());
    }
    // Auto-detect installed clients.
    let mut found = Vec::new();
    for (id, _, _, marker) in json_file_clients() {
        if marker.as_deref().map(Path::exists).unwrap_or(false) {
            found.push(id.to_string());
        }
    }
    if on_path("claude").is_some() {
        found.push("claude-code".into());
    }
    if on_path("code").is_some() {
        found.push("vscode".into());
    }
    Ok(found)
}

fn install_one(id: &str, exe: &Path, base_url: Option<&str>) -> Result<String> {
    if id == "chatgpt" {
        bail!(
            "ChatGPT only supports REMOTE MCP servers (no local config). Add a connector in \
             ChatGPT → Settings → Apps (Developer mode) pointing at the hosted mcp.smarts.bio URL."
        );
    }
    if id == "claude-code" {
        return install_via_cli("claude", &claude_args(exe, base_url),
            "added via `claude mcp add`",
        );
    }
    if id == "vscode" {
        return install_via_cli("code", &vscode_args(exe, base_url),
            "added via `code --add-mcp`",
        );
    }

    // JSON-file clients.
    let (_, _, path, _) = json_file_clients()
        .into_iter()
        .find(|(cid, ..)| *cid == id)
        .ok_or_else(|| anyhow!("unknown client"))?;
    let path = path.ok_or_else(|| anyhow!("could not determine config path"))?;
    merge_json_file(&path, exe, base_url)?;
    Ok(format!("updated {}", path.display()))
}

fn uninstall_one(id: &str) -> Result<String> {
    if id == "claude-code" {
        return install_via_cli(
            "claude",
            &[
                "mcp".into(),
                "remove".into(),
                SERVER_NAME.into(),
                "--scope".into(),
                "user".into(),
            ],
            "removed via `claude mcp remove`",
        );
    }
    if id == "vscode" || id == "chatgpt" {
        bail!("remove the '{SERVER_NAME}' entry manually for this client");
    }
    let (_, _, path, _) = json_file_clients()
        .into_iter()
        .find(|(cid, ..)| *cid == id)
        .ok_or_else(|| anyhow!("unknown client"))?;
    let path = path.ok_or_else(|| anyhow!("could not determine config path"))?;
    if !path.exists() {
        return Ok("nothing to remove".into());
    }
    let mut root = read_json(&path)?;
    if let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) {
        servers.remove(SERVER_NAME);
    }
    backup_and_write(&path, &root)?;
    Ok(format!("removed from {}", path.display()))
}

/// Merge `mcpServers.smarts = { command, args, env? }` into a JSON config file.
fn merge_json_file(path: &Path, exe: &Path, base_url: Option<&str>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut root = if path.exists() {
        read_json(path)?
    } else {
        json!({})
    };
    if !root.is_object() {
        bail!("{} is not a JSON object — edit it manually", path.display());
    }
    let obj = root.as_object_mut().unwrap();
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        bail!("'mcpServers' in {} is not an object", path.display());
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert(SERVER_NAME.to_string(), server_entry(exe, base_url));
    backup_and_write(path, &root)
}

/// The `{ command, args, env? }` entry for a stdio MCP server.
fn server_entry(exe: &Path, base_url: Option<&str>) -> Value {
    let mut entry = json!({
        "command": exe.to_string_lossy(),
        "args": ["mcp", "serve"],
    });
    if let Some(url) = base_url {
        entry["env"] = json!({ "SMARTSBIO_BASE_URL": url });
    }
    entry
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|e| {
        anyhow!(
            "{} isn't plain JSON ({e}); it may contain comments. Edit it manually or use `smarts mcp install --print`.",
            path.display()
        )
    })
}

fn backup_and_write(path: &Path, root: &Value) -> Result<()> {
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("json.bak"));
    }
    let text = serde_json::to_string_pretty(root)?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

fn claude_args(exe: &Path, base_url: Option<&str>) -> Vec<String> {
    let mut args = vec!["mcp".into(), "add".into(), "--scope".into(), "user".into()];
    if let Some(url) = base_url {
        args.push("--env".into());
        args.push(format!("SMARTSBIO_BASE_URL={url}"));
    }
    args.push(SERVER_NAME.into());
    args.push("--".into());
    args.push(exe.to_string_lossy().into_owned());
    args.push("mcp".into());
    args.push("serve".into());
    args
}

fn vscode_args(exe: &Path, base_url: Option<&str>) -> Vec<String> {
    let mut entry = json!({
        "name": SERVER_NAME,
        "command": exe.to_string_lossy(),
        "args": ["mcp", "serve"],
    });
    if let Some(url) = base_url {
        entry["env"] = json!({ "SMARTSBIO_BASE_URL": url });
    }
    vec!["--add-mcp".into(), entry.to_string()]
}

fn install_via_cli(bin: &str, args: &[String], ok_msg: &str) -> Result<String> {
    if on_path(bin).is_none() {
        bail!("`{bin}` not found on PATH — install it or run the command manually");
    }
    let status = Command::new(bin)
        .args(args)
        .status()
        .with_context(|| format!("running {bin}"))?;
    if status.success() {
        Ok(ok_msg.to_string())
    } else {
        bail!("`{bin}` exited with {status}")
    }
}

fn print_snippet(exe: &Path, base_url: Option<&str>) {
    let entry = server_entry(exe, base_url);
    let snippet = json!({ "mcpServers": { SERVER_NAME: entry } });
    println!(
        "Add this to a client's MCP config (Claude Desktop, Cursor, Windsurf, Gemini CLI):\n\n{}\n",
        serde_json::to_string_pretty(&snippet).unwrap_or_default()
    );
    println!(
        "ChatGPT / Gemini app / Claude web: these are REMOTE-only — add a connector in their\n\
         settings pointing at the hosted mcp.smarts.bio URL (not a local command)."
    );
}

// ---- small platform helpers ----------------------------------------------

fn current_exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("could not resolve the smarts binary path")
}

fn dirs_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

fn dirs_config() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().to_path_buf())
}

/// Find an executable on `$PATH` (cross-platform, minimal).
fn on_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{cmd}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
