#!/usr/bin/env bash
# SessionStart probe for the rust-analyzer-lsp plugin's language server.
#
# .claude/settings.json enables the plugin, which runs `rust-analyzer` from PATH. Under rustup
# that is a proxy for the pinned toolchain's component (rust-toolchain.toml), and when it cannot
# resolve one the server exits 1 and the plugin reports only "crashed", with no hint of which
# piece is missing. This reports that up front. It never installs and never fails the session —
# a missing server degrades navigation to grep and diagnostics to `cargo check`, it does not
# stop work.
set -uo pipefail

missing=()

rust-analyzer --version >/dev/null 2>&1 ||
  missing+=("rust-analyzer — the language server the plugin runs")

if command -v rustc >/dev/null 2>&1; then
  sysroot=$(rustc --print sysroot 2>/dev/null || true)
  if [ -z "$sysroot" ] || [ ! -d "$sysroot/lib/rustlib/src/rust" ]; then
    missing+=("rust-src — std sources; without them std navigation answers blank")
  fi
else
  missing+=("a Rust toolchain — rustc is not on PATH")
fi

[ ${#missing[@]} -eq 0 ] && exit 0

list=""
for item in "${missing[@]}"; do
  list="$list\\n  - $item"
done

printf '{"systemMessage":"rust-analyzer is not set up on this machine:%s\\n\\nRun /setup-rust-analyzer to install it, then start a new session. Until then, navigation in this repo is grep rather than LSP, and an edit reports no diagnostics."}\n' "$list"
