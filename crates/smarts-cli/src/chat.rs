//! Interactive chat REPL — a conversational front-end over the agent.
//!
//! Reuses the streaming client: each turn streams `POST /v1/query/stream`,
//! captures the agent's `sessionId` so follow-up turns continue the same
//! conversation, and renders progress frames as a spinner before printing the
//! final answer. Ctrl-C interrupts an in-flight response (`/v1/query/stop`);
//! Ctrl-D (or `/exit`) leaves the chat.

use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use serde_json::Value;

use crate::Ctx;

const PROMPT: &str = "\x1b[1;36msmarts ›\x1b[0m ";

/// Run the chat loop, optionally resuming an existing conversation.
pub async fn run(ctx: &Ctx, mut conversation_id: Option<String>) -> Result<()> {
    let workspace = ctx.optional_workspace();
    print_banner(ctx, workspace.as_deref(), conversation_id.as_deref());

    let mut editor = DefaultEditor::new()?;

    loop {
        match editor.readline(PROMPT) {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(input);

                if let Some(rest) = input.strip_prefix('/') {
                    if handle_slash(ctx, rest, &workspace, &mut conversation_id).await? {
                        break; // slash command asked to exit
                    }
                    continue;
                }

                // A failed turn (network blip, auth, upstream error) prints and
                // keeps the session alive rather than dropping out of chat.
                if let Err(err) = turn(ctx, input, workspace.as_deref(), &mut conversation_id).await
                {
                    println!("\x1b[31merror:\x1b[0m {err}\n");
                }
            }
            // Ctrl-C at the prompt: cancel the current line, stay in chat.
            Err(ReadlineError::Interrupted) => {
                println!("(Ctrl-C — press Ctrl-D or type /exit to quit)");
            }
            // Ctrl-D: leave.
            Err(ReadlineError::Eof) => {
                println!("bye 👋");
                break;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// Stream a single conversational turn.
async fn turn(
    ctx: &Ctx,
    prompt: &str,
    workspace: Option<&str>,
    conversation_id: &mut Option<String>,
) -> Result<()> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spinner.enable_steady_tick(Duration::from_millis(120));
    spinner.set_message("thinking…");

    let mut final_answer: Option<String> = None;
    let mut error_message: Option<String> = None;
    let mut session: Option<String> = None;

    let stream = ctx.client.query_stream(
        prompt,
        workspace,
        conversation_id.as_deref(),
        |event: Value| {
            if let Some(s) = event.get("sessionId").and_then(Value::as_str) {
                session = Some(s.to_string());
            }
            match event.get("status").and_then(Value::as_str) {
                Some("complete") => {
                    final_answer = event
                        .get("result")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                Some("error") => {
                    error_message = Some(
                        event
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string(),
                    );
                }
                _ => {
                    if let Some(kind) = event.get("type").and_then(Value::as_str) {
                        let label = event.get("agent").and_then(Value::as_str).unwrap_or(kind);
                        spinner.set_message(format!("{label}…"));
                    }
                }
            }
        },
    );

    // Race the response against Ctrl-C so a long turn can be interrupted.
    tokio::select! {
        result = stream => {
            spinner.finish_and_clear();
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            spinner.finish_and_clear();
            println!("⏹  interrupted");
            let stop_id = conversation_id.as_deref().or(session.as_deref());
            if let Some(id) = stop_id {
                let _ = ctx.client.stop_query(id).await;
            }
            // Adopt the session so the next turn still continues this conversation.
            if conversation_id.is_none() {
                *conversation_id = session;
            }
            return Ok(());
        }
    }

    // Persist the conversation id captured from the stream.
    if conversation_id.is_none() {
        *conversation_id = session;
    }

    if let Some(msg) = error_message {
        println!("\x1b[31m⚠ {msg}\x1b[0m\n");
    } else if let Some(answer) = final_answer {
        // The agent embeds rendered visualizations as markdown images pointing at
        // an `s3://…` key (or a signed URL). Printing that raw is useless in a
        // terminal, so download each referenced image, open it locally, and
        // replace the markdown with a friendly line — mirroring `file render`.
        let rendered = render_answer(ctx, workspace, &answer).await;
        println!("{rendered}\n");
    } else {
        println!("(no answer received)\n");
    }
    Ok(())
}

/// Post-process an agent answer for the terminal:
///   * markdown images `![alt](target)` → download the file and open it locally;
///   * markdown links  `[text](s3://…)`  → resolve the `s3://` key to a working
///     signed URL and show it inline (a raw `s3://` is useless in a terminal).
/// All other text — including ordinary `[text](https://…)` links — is untouched.
async fn render_answer(ctx: &Ctx, workspace: Option<&str>, answer: &str) -> String {
    let mut out = String::new();
    let mut rest = answer;
    while let Some(open) = rest.find('[') {
        // A markdown image is a link preceded by '!'.
        let is_image = open > 0 && rest.as_bytes()[open - 1] == b'!';
        let after = &rest[open..]; // starts at '['

        // Parse `[text](target)`.
        let parsed = after.find("](").and_then(|text_end| {
            let url_start = text_end + 2;
            after[url_start..]
                .find(')')
                .map(|rel| (&after[1..text_end], &after[url_start..url_start + rel], url_start + rel + 1))
        });

        match parsed {
            Some((text, target, consumed)) => {
                // For images, also drop the leading '!' from the emitted prefix.
                let prefix_end = if is_image { open - 1 } else { open };
                out.push_str(&rest[..prefix_end]);

                let replacement = if is_image {
                    handle_image(ctx, workspace, text, target)
                        .await
                        .unwrap_or_else(|| format!("[image: {}]", if text.is_empty() { target } else { text }))
                } else {
                    // Only rewrite s3:// links; leave normal links exactly as-is.
                    handle_link(ctx, workspace, text, target)
                        .await
                        .unwrap_or_else(|| format!("[{text}]({target})"))
                };
                out.push_str(&replacement);
                rest = &after[consumed..];
            }
            None => {
                // Not a complete token — emit up to and including '[' and continue
                // so the scan always advances (no infinite loop).
                out.push_str(&rest[..open + 1]);
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resolve an `s3://<key>` markdown link to a working signed URL and format it
/// as `text: <url>`. Returns `None` for non-`s3://` targets (kept verbatim) or
/// when the key can't be resolved.
async fn handle_link(ctx: &Ctx, workspace: Option<&str>, text: &str, target: &str) -> Option<String> {
    let key = target.strip_prefix("s3://")?;
    let ws = workspace
        .map(str::to_string)
        .or_else(|| parse_workspace_from_key(key))?;
    let url = ctx.client.download_url(&ws, key).await.ok()?;
    let label = if text.is_empty() { basename(key) } else { text.to_string() };
    Some(format!("{label}: {url}"))
}

/// Download the image a markdown reference points at, save it to a temp file,
/// open it in the default viewer, and return a one-line summary. Returns `None`
/// if the target isn't fetchable (the caller then falls back to alt text).
async fn handle_image(ctx: &Ctx, workspace: Option<&str>, alt: &str, target: &str) -> Option<String> {
    let (bytes, name) = if let Some(key) = target.strip_prefix("s3://") {
        let ws = workspace
            .map(str::to_string)
            .or_else(|| parse_workspace_from_key(key))?;
        let bytes = ctx.client.download_bytes(&ws, key).await.ok()?;
        (bytes, basename(key))
    } else if target.starts_with("http://") || target.starts_with("https://") {
        let bytes = ctx.client.fetch_url_bytes(target).await.ok()?;
        (bytes, basename(target.split('?').next().unwrap_or(target)))
    } else {
        return None;
    };

    // Save into the current working directory (matching `file render`), using a
    // clean `<stem>.<ext>` name derived from the source file rather than the
    // timestamped render key (e.g. `info_test.csv` → `info_test.png`).
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("png");
    // `alt` may be a nested workspace path (e.g. "exp2/test_gatk.bam") — take only
    // its file name, otherwise the derived local path lands in a cwd subdirectory
    // that doesn't exist and the write silently fails.
    let stem_src = basename(if alt.is_empty() { name.as_str() } else { alt });
    let stem = stem_src.rsplit_once('.').map(|(s, _)| s).unwrap_or(stem_src.as_str());
    let filename = format!("{stem}.{ext}");
    let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let path = dir.join(&filename);
    std::fs::write(&path, &bytes).ok()?;
    // Open with the OS default app for the file type (Preview for images, etc.),
    // not the web browser.
    let _ = crate::output::open_with_default_app(&path);

    let label = if alt.is_empty() { filename.as_str() } else { alt };
    Some(format!(
        "🖼  {label} — {} ({}, opened)",
        path.display(),
        crate::output::human_size(bytes.len() as u64),
    ))
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

/// Final path segment (the file name) of a `/`-separated key or URL path.
fn basename(path: &str) -> String {
    path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("image").to_string()
}

/// Handle a `/slash` command. Returns `true` when the chat should exit.
async fn handle_slash(
    ctx: &Ctx,
    rest: &str,
    workspace: &Option<String>,
    conversation_id: &mut Option<String>,
) -> Result<bool> {
    let mut parts = rest.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        "exit" | "quit" | "q" => {
            println!("bye 👋");
            return Ok(true);
        }
        "help" | "?" => print_help(),
        "clear" | "new" => {
            *conversation_id = None;
            println!("Started a new conversation.");
        }
        "workspace" | "ws" => match workspace {
            Some(w) => println!("workspace: {w}"),
            None => println!("no workspace selected (start chat with -w <id>)"),
        },
        "ls" | "files" => {
            let Some(ws) = workspace.as_deref() else {
                println!("no workspace selected (start chat with -w <id>)");
                return Ok(false);
            };
            let cwd = ctx.config.cwd_for(ws);
            match ctx.client.list_files(ws, &cwd).await {
                Ok(items) => {
                    println!("📁 /{cwd}");
                    for it in &items {
                        let name = it.name.clone().unwrap_or_default();
                        if it.is_folder() {
                            println!("  {name}/");
                        } else {
                            println!("  {name}");
                        }
                    }
                }
                Err(e) => println!("could not list files: {e}"),
            }
        }
        other => {
            println!("unknown command /{other} — type /help for the list");
        }
    }
    Ok(false)
}

fn print_banner(ctx: &Ctx, workspace: Option<&str>, conversation_id: Option<&str>) {
    println!("\x1b[1msmarts.bio chat\x1b[0m — {}", ctx.client.base_url());
    match workspace {
        Some(w) => println!("workspace: {w}"),
        None => println!("workspace: (none — start with -w <id> for file-aware answers)"),
    }
    if let Some(id) = conversation_id {
        println!("resuming conversation: {id}");
    }
    println!("Type your question, /help for commands, Ctrl-D to exit.\n");
}

fn print_help() {
    println!(
        "Commands:\n  \
         /new, /clear   start a fresh conversation\n  \
         /workspace     show the active workspace\n  \
         /ls, /files    list files in the current directory\n  \
         /help, /?      show this help\n  \
         /exit, /quit   leave chat (or press Ctrl-D)\n\n\
         While the agent is answering, press Ctrl-C to interrupt.\n"
    );
}
