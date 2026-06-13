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
        println!("{answer}\n");
    } else {
        println!("(no answer received)\n");
    }
    Ok(())
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
