# publish.md — smarts CLI build / run / release cheatsheet

All commands run from the repo root: `smarts-bio-cli/`.
If `cargo` isn't found: `source "$HOME/.cargo/env"` (add it to `~/.zshrc` to make it permanent).

## Build

```bash
cargo build                 # debug  → target/debug/smarts
cargo build --release       # optimized → target/release/smarts
cargo test                  # unit tests
cargo clippy                # lints
```

## Run locally (dev)

```bash
# point at the local gateway + your API key
export SMARTSBIO_BASE_URL=http://localhost:3022
export SMARTSBIO_API_KEY=sk_live_...        # or: smarts auth set-key sk_live_...

# run without installing
cargo run -- auth status
cargo run -- workspace list
cargo run -- chat

# or use the built binary directly
./target/debug/smarts workspace list
```

## Install on your machine

```bash
# copy an optimized build into ~/.cargo/bin (re-run after code changes)
cargo install --path crates/smarts-cli --force

# OR symlink the debug build so every `cargo build` is picked up automatically (best while developing)
ln -sf "$PWD/target/debug/smarts" ~/.cargo/bin/smarts
```

## Cross-compile a single target locally (optional)

```bash
brew install zig && cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-gnu
cargo zigbuild --release --target x86_64-unknown-linux-gnu
# binary → target/x86_64-unknown-linux-gnu/release/smarts
```

## Release (production) — via cargo-dist

Config: `dist-workspace.toml` · Workflow: `.github/workflows/release.yml`
Builds macOS (arm64/x86_64), Linux (glibc x86_64/aarch64, musl x86_64), Windows (x86_64),
and publishes the shell + PowerShell installers and the Homebrew formula.

```bash
dist plan                   # dry-run: show everything a release would build/publish
dist generate               # regenerate the workflow after editing dist-workspace.toml
```

One-time GitHub setup:
1. Create + push repo `smartsbio/smarts-bio-cli`.
2. Create empty tap repo `smartsbio/homebrew-tap`.
3. Add repo secret `HOMEBREW_TAP_TOKEN` (PAT with `contents:write` on the tap repo).

Cut a release:
```bash
# bump version in Cargo.toml [workspace.package] version, commit, then:
git tag v0.1.0 && git push --tags     # CI builds all platforms + publishes the release
```

## How end users install (after a release)

```bash
# macOS / Linux
brew install smartsbio/tap/smarts
curl -LsSf https://smarts.bio/install.sh | sh

# Windows (PowerShell)
irm https://smarts.bio/install.ps1 | iex
```

Vanity URLs `https://smarts.bio/install.sh` / `install.ps1` are 302 redirects
(configured in `smarts-bio-website-2/next.config.ts`) to the latest GitHub
release installers. They 404 until the first release is published.
