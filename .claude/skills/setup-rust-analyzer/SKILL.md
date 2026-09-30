---
name: setup-rust-analyzer
description: Install and verify the language server behind this repo's rust-analyzer-lsp plugin — rust-analyzer and rust-src. Use when the SessionStart hook reports a missing piece, when the LSP tool errors with "rust-analyzer crashed with exit code 1", when edits bring back no diagnostics long after the first index, or when setting the repo up on a new machine.
---

# Setting up rust-analyzer

[.claude/settings.json](../../settings.json) enables Claude Code's `rust-analyzer-lsp` plugin
from the official marketplace (and declares that marketplace, so it loads under
`--setting-sources project`). The plugin runs `rust-analyzer` from `PATH` and passes it no
options; [rust-analyzer.toml](../../../rust-analyzer.toml) is the only configuration it gets.
Two pieces have to be present; the SessionStart hook
([.claude/hooks/rust-analyzer-check.sh](../../hooks/rust-analyzer-check.sh)) names whichever
are missing.

## 1. Identify the toolchain flavor first

```bash
command -v rustup cargo rustc
```

- **rustup present** — [rust-toolchain.toml](../../../rust-toolchain.toml) lists
  `rust-analyzer` and `rust-src`, so the first `cargo` or `rust-analyzer` call in the repo
  installs them with the pinned toolchain. If it did not (an older rustup, an offline
  machine), `rustup component add rust-analyzer rust-src` inside the repo.
- **cargo/rustc present, no rustup** (e.g. `brew install rust`) — the pin is ignored.
  Homebrew's `rust` formula ships `rust-src` but **not** `rust-analyzer`:
  `brew install rust-analyzer`.
- **neither** — install a toolchain first (`brew install rustup-init && rustup-init`, or
  rustup.rs).

## 2. Verify before handing back

Re-run the hook script; silence and exit 0 means both are present:

```bash
.claude/hooks/rust-analyzer-check.sh; echo "exit=$?"
```

`rust-analyzer --version` must print a version, not `Unknown binary 'rust-analyzer'`: that is
the rustup proxy for a component the pinned toolchain lacks, and it is what the plugin sees
as a crash.

Then check that the plugin loads the way a dispatched agent runs:

```bash
echo ok | claude -p --setting-sources project --strict-mcp-config \
  --output-format stream-json --verbose --max-turns 1 |
  jq -c 'select(.subtype=="init") | {plugins: [.plugins[].name], lsp: (.tools | index("LSP") != null)}'
```

`rust-analyzer-lsp` among the plugins and `"lsp": true` is the pass condition.

## 3. Tell the user to start a new session

Plugins load at session start; the running session keeps whatever it started with.

## Known traps

- **First-load emptiness.** `references`/`definition`/`hover` answer from whatever is indexed
  so far and return empty during the initial load, and an edit made then brings back no
  diagnostic. Empty is indistinguishable from "no callers" — ask again until an answer is
  non-empty before concluding anything.
- **Xcode license.** On macOS, an install that dies at the linker is usually
  `xcode-select -p` pointing at Xcode.app rather than CommandLineTools. See Troubleshooting
  in [CLAUDE.md](../../../CLAUDE.md).
