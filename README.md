# Zed (macOS Local Edition)

A streamlined, macOS-only personal build of the Zed code editor focused on speed, clean editing, and multi-workspace organization:

- **macOS-native only** (`gpui` + Metal on Apple Silicon / macOS)
- **No AI / collab / telemetry bloat** — stripped AI/LLM providers, collab/voice/audio, auto-updater, crash reporting, onboarding, Vim mode, debugger UI, REPL, dev containers, and non-macOS platform crates
- **Multi-Workspace Sidebar** (`crates/sidebar`) — manage multiple projects, Git worktrees, and persistent per-project terminal threads in one window

## Prerequisites (macOS)

- Rust toolchain (managed via [`rust-toolchain.toml`](./rust-toolchain.toml))
- Xcode + Metal Toolchain (`xcodebuild -downloadComponent MetalToolchain`)
- `cmake` (`brew install cmake`)

## Common Commands

```sh
make help           # List all targets
make run            # Build & run (debug)
make run-fast       # Build & run (release-fast)
make open           # Launch already-built debug binary
make check          # Fast cargo check for zed
make check-sidebar  # Fast cargo check for sidebar + workspace
make build-release  # Build release zed + cli binaries
make bundle         # Package standalone target/bundle/Zed Local.app
make install        # Build & install to /Applications/Zed Local.app
make size           # Show repo, crates, target, and free disk space
```

## License

Licensed under [GPL-3.0-or-later](./LICENSE-GPL) and [Apache-2.0](./LICENSE-APACHE) where marked.
