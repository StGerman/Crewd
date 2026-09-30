#!/usr/bin/env bash
# SessionStart probe for the rust-analyzer-lsp plugin's language server.
#
# `command -v rust-analyzer` succeeds on the rustup proxy even when the pinned toolchain has no
# rust-analyzer component, and the plugin then starts and reports only "crashed" (#192). Invoking
# the proxy (`rust-analyzer --version`) is the check that sees that, and under rustup that
# invocation installs the toolchain and the components rust-toolchain.toml lists when they are
# absent. A server that is still missing does not fail the session: navigation stays grep, and
# an edit's check stays `cargo check`.
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
