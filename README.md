# smarts-bio-cli

`smarts` — the smarts.bio command-line interface. A single cross-platform Rust
binary that brings the smarts.bio platform (AI agent, bioinformatics tools,
pipelines, workspace files) into terminals, scripts, and AI coding agents.

It talks to the public API gateway (`bioinformatics-api`, `/v1`) — the same
surface the SDKs use — so it stays in sync with the platform regardless of
language.

> **Status:** Phase 1 (this build) — full command surface over **API-key auth**.
> Browser login (phase 2), local-file `open` (phase 2), and the MCP server
> (phase 3, local stdio + hosted) are scaffolded and print a pointer to that
> work. See [the plan](../../.claude/plans) for the roadmap.

## Install

Once a release is published (see **Releasing** below):

**macOS / Linux**
```bash
# Homebrew (macOS + Linuxbrew)
brew install smartsbio/tap/smarts

# or the shell installer
curl -LsSf https://smarts.bio/install.sh | sh
```

**Windows** (PowerShell)
```powershell
irm https://smarts.bio/install.ps1 | iex
```

**From source** (any OS, with Rust)
```bash
cargo install --path crates/smarts-cli
```

The script installers (`curl | sh`, `irm | iex`) and Homebrew don't trip macOS
Gatekeeper or Windows SmartScreen — those only flag *browser-downloaded*
`.pkg`/`.msi`/`.exe`. Prebuilt targets: macOS (Intel + Apple Silicon), Linux
(glibc x86_64/aarch64, static musl x86_64), Windows (x86_64).

## Workspace layout

```
crates/
  smarts-client/   # REST + SSE client over the /v1 gateway (auth, config, resources)
  smarts-cli/      # the `smarts` binary (clap command tree) → produces `smarts`
```

## Build & test

```bash
cargo build              # debug binary at target/debug/smarts
cargo test               # unit tests (path resolution, param parsing, …)
cargo build --release    # optimized binary at target/release/smarts
```

## Authenticate

```bash
# CI / headless:
export SMARTSBIO_API_KEY=sk_live_...
# or store it in the OS keychain:
smarts auth set-key sk_live_...

# point at a local stack instead of api.smarts.bio:
export SMARTSBIO_BASE_URL=http://localhost:3022

smarts auth status
```

## Usage

Consistent `smarts <noun> <verb>` grammar. `--json` on any command for machine
output; `-w/--workspace` overrides the saved default.

```bash
smarts                                        # bare command → interactive chat (in a terminal)
smarts chat                                   # same, explicit
smarts chat --conversation <id>              # resume a conversation
smarts -w <workspace-id> chat                 # file-aware chat

smarts workspace list
smarts workspace use <workspace-id>          # set the default

smarts query "analyze the HBB gene"          # one-shot; streams progress + answer
smarts query "..." --no-stream               # wait for the full response

smarts tool list [--category database]
smarts tool show ncbi-blast
smarts tool run ncbi-blast -p program=blastn -p query=@seq.json

smarts pipeline list                         # runnable definitions
smarts pipeline show quality-control
smarts pipeline run quality-control -p inputFastqR1=... # → prints a run id

smarts run list                              # executions
smarts run status <id>
smarts run watch <id>                        # poll until terminal
smarts run cancel <id>

# Files behave like a shell scoped to one workspace (cwd persisted per workspace):
smarts file ls
smarts file cd results
smarts file pwd
smarts file upload report.vcf                # ≤10MB direct, >10MB presigned S3
smarts file download report.vcf -o out.vcf
smarts file open report.vcf                  # workspace file → opens view.smarts.bio
smarts file open report.vcf --print-url      # just print the link (agents/headless)
smarts open ./petri.jpeg                     # LOCAL file → served from a loopback server, no upload
smarts open ./reads.fasta --print-url        # local file, print the viewer URL instead
smarts file mkdir results
smarts file mv report.vcf archive
smarts file rm old.bam

smarts conversation list
smarts conversation show <id>
```

`run` inputs: `-p key=value` (repeatable; values parse as JSON when valid,
otherwise treated as a string) and/or `--input <file|@file|->` for a JSON object;
`-p` overrides `--input`.

### Interactive chat

`smarts chat` (or just `smarts` in a terminal) opens a conversational REPL over
the agent: each turn streams live progress, the conversation continues
automatically (the agent `sessionId` is captured and reused), and **Ctrl-C
interrupts** an in-flight answer (Ctrl-D or `/exit` leaves). In-chat commands:
`/new` (fresh conversation), `/workspace`, `/ls`, `/help`, `/exit`.

## Configuration

- Secrets (the `sk_live_` key) live in the OS keychain via the `keyring` crate.
- Non-secret prefs live at the platform config dir (`config.toml`): gateway URL,
  default workspace, and per-workspace current directory.
- Env overrides: `SMARTSBIO_API_KEY`, `SMARTSBIO_BASE_URL`, and `SMARTSBIO_VIEWER_URL`
  (viewer host for `smarts open`; defaults to `https://view.smarts.bio`, set to
  `http://localhost:3012` to use a local bio-viewers instance).

## Roadmap

- **Phase 2** — `smarts login` (browser OAuth2 PKCE via loopback). *(Local-file
  `smarts open` is already implemented — loopback serve, no upload.)*
- **Phase 3** — `smarts mcp serve` (local stdio) and a hosted Streamable-HTTP
  deployment at `mcp.smarts.bio` for ChatGPT / Gemini / Claude-web.
- Skill generation (`smarts skill install`).

## Releasing

Releases are produced by [`dist`](https://opensource.axo.dev/cargo-dist/) (cargo-dist),
configured in `dist-workspace.toml`. The GitHub Actions workflow at
`.github/workflows/release.yml` builds the macOS binaries and publishes the
shell installer + Homebrew formula on every pushed version tag.

**One-time setup on GitHub:**
1. Create the repo `smartsbio/smarts-bio-cli` and push this project.
2. Create an (empty) tap repo `smartsbio/homebrew-tap`.
3. Add a repo secret `HOMEBREW_TAP_TOKEN` — a GitHub PAT with `contents:write`
   on the tap repo (so the release can push the formula there).

**Cut a release:**
```bash
dist plan                     # dry-run: see exactly what will be built/published
# bump the version in Cargo.toml ([workspace.package] version), commit, then:
git tag v0.1.0 && git push --tags
```
The workflow builds `aarch64`/`x86_64` macOS tarballs, generates
`smarts-installer.sh` + `smarts.rb`, creates the GitHub Release, and updates the
Homebrew tap.

**Add more platforms:** add the triples to `targets` in `dist-workspace.toml`
(`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`)
and the `powershell`/`msi` installers if wanted, then run `dist generate` and
commit the refreshed workflow.
