# 3. Confining a dispatched agent: a sandbox around the worker and the gate

- **Status:** Proposed. The operator took decisions 2 to 9 in a supervised session on
  2026-10-03. Decision 1 is still open: it names the two candidate backends, and a spike
  chooses one. The ADR is Accepted when the spike's result is written into decision 1.
- **Date:** 2026-10-03
- **Issues:** #135 (the umbrella and its criteria), #138 (redaction), #259 (`workflows: write`),
  milestone M8

## Context

A dispatched agent runs `claude -p --permission-mode bypassPermissions` or
`grok --always-approve` as the operator. The broker is not an isolation boundary
([architecture.md](../architecture.md), "Why the broker is not an isolation boundary"). On
macOS, `gh` and `claude` read the login keychain, so an agent can comment, push and close as the
operator. M8's outcome is that it cannot.

Agent-authored code and data reach the operator's authority by four paths:

1. **The worker's process tree.** This covers shell commands, the file tools, hooks, LSP servers
   and MCP children.
2. **The gate.** `gate.commands` are the operator's, but `cargo test` and `cargo clippy` compile
   and run the agent's `build.rs`, proc-macros and tests with the operator's environment and
   network. A failing command's output goes back to the agent.
3. **Host-side git in the worktree.** The gate's rebase, delivery's push and workspace
   preparation all honour repository config and hooks, which the agent can write
   (`core.hooksPath`, `.git/hooks`). Today nothing turns them off.
4. **The branch.** Delivery pushes whatever the agent commits.

Two constraints bind. The operator's bound in #135 allows no new product dependency for a
stranger beyond what the agent CLI already needs. CLAUDE.md rule 5 says that anything the config
turns on starts, or crewd exits naming it.

The options were checked on 2026-10-03:

- **Claude Code's built-in sandbox** (`--settings`) confines Bash, PowerShell and Monitor
  commands only. The file tools, hooks, MCP servers and LSP servers run outside it, and its
  documentation says that a single boundary around them means running the whole process in a
  container, a VM or the sandbox runtime. It also does nothing for Grok.
- **srt** (Anthropic's sandbox runtime) wraps any command. It uses `sandbox-exec` on macOS and
  `bubblewrap` on Linux, with a filtering proxy. It is installed from npm, needs `ripgrep`, and
  is a beta research preview whose config format may change. It would be a new dependency for a
  stranger.
- **A separate uid** needs administrator setup on every host.
- **A container, or a VM run by a separate manager** (Docker, Lima), is a new dependency.
- **A Firecracker microVM** needs Linux with KVM, and shares host files only as block devices.
  It does not run on macOS.
- **A libkrun microVM through smolvm** (Apache-2.0, embeddable as the `smolmachines` crate)
  runs on Hypervisor.framework on macOS and on KVM on Linux. It shares directories with
  virtio-fs and has egress off by default, with a host allowlist. The guest has its own kernel
  and no access to the host keychain. The costs:
  - a guest image carrying the agent CLIs and the build toolchain;
  - the agent's login has to reach the guest (`CLAUDE_CODE_OAUTH_TOKEN`, or smolvm's
    credential substitution);
  - builds are Linux builds, even on macOS;
  - Linux needs KVM;
  - on macOS, a hypervisor entitlement on the binary that calls Hypervisor.framework.

  How smolvm meets these for an embedding app is not yet verified.
- **A crewd-written profile** uses the same primitives srt uses, with nothing installed on
  macOS. On Linux it needs `bubblewrap`, which Claude Code's own sandbox also requires. A spike
  on macOS 26.6.2 ran `/usr/bin/sandbox-exec` with a profile that denies outbound traffic except
  to loopback, and denies reading and executing the `gh` binary:
  - `curl https://github.com` failed; it returns 200 unconfined;
  - `claude --version` ran;
  - reading `~/.crewd` was refused;
  - running `gh` was refused, and so was copying it.

## Decision

crewd confines every process that runs agent-authored code inside an OS sandbox that crewd
writes itself.

1. **Mechanism: a `Sandbox` trait, with the backend open.** Confinement is a trait
   (ADR 1, boundary 1), so decisions 2 to 9 do not depend on which backend implements it. If
   confinement is on and the backend cannot be used, crewd fails at startup. If the sandbox
   cannot start for one run, that run fails (rule 5); it never runs unconfined. The two
   candidates:
   - **Native.** On macOS, crewd generates a Seatbelt profile and runs the child under
     `/usr/bin/sandbox-exec`. On Linux, it runs the child under `bubblewrap` with user and
     network namespaces, and serves egress through its own loopback proxy. Nothing to install on
     macOS; `bubblewrap` on Linux.
   - **microVM.** Each run gets a smolvm (libkrun) guest. The worktree and the git state its
     commits need are shared into the guest, and egress uses smolvm's host allowlist. Decision 5
     becomes unnecessary, because the guest has no keychain.

   **The spike decides.** It runs on the dogfood Mac, against a throwaway clone. It checks:
   - that a guest boots with the worktree mounted read-write and the repository's `.git`
     config and hooks mounted read-only;
   - that `claude -p` with stream-json runs inside the guest and reaches the host broker;
   - that the egress allowlist admits the model API and the crate registry, and refuses
     `github.com`;
   - how long this repository's cold and warm `cargo test` take, compared with native;
   - how the login reaches the guest;
   - how an installed crewd binary gets the hypervisor entitlement;
   - whether there is a Linux grok.

   The microVM is the sole backend if every check holds at an acceptable cost. Otherwise the
   native backend is the default, and the microVM is an opt-in second implementation.
2. **What is wrapped.** The whole worker process, for both claude and grok, and so everything it
   starts. The gate's commands run under their own profile (decision 6).
3. **Egress.** Direct outbound traffic is denied, and only the profile's allowlisted hosts
   are reachable. The native backend does this with a loopback HTTP CONNECT proxy and
   `HTTPS_PROXY`/`HTTP_PROXY`, so a program that ignores those variables fails closed. The
   microVM backend uses its host allowlist.
   - The worker's default allowlist is its model API (Anthropic for claude, xAI for grok) and the
     crate registry.
   - The broker's loopback port stays reachable.
   - No default list contains GitHub. Delivery pushes and the broker writes host-side, so a
     confined agent never needs it, and a token it reads has nowhere to go.
4. **Filesystem.**
   - Writes are allowed to the worktree and the git state its commits need, the cargo caches,
     the per-user temp directory, and the worker's own state (`~/.claude` and `~/.claude.json`
     for claude).
   - Reads are allowed everywhere except the deny set: the deployment directory (config,
     `crew.db`, log, transcripts), the GitHub App key and settings under `~/.crewd/`, hook
     sources, and the Jira credentials file.
   - A profile can widen what is allowed. It can never shrink the deny set.
5. **Keychain (macOS, native backend).** The keychain stays reachable, because claude
   authenticates from it. The
   profile denies reading and executing the known credential helpers: `gh`,
   `git-credential-osxkeychain` and `security`. Any other binary that reads a keychain item meets
   that item's access prompt, which a headless run cannot answer.
6. **The gate** runs under a `gate` profile.
   - It has the same deny set.
   - It can write to the worktree and the cargo caches.
   - Its only egress is the crate registry.
   - The operator extends it per deployment with the hosts and paths their suite needs.

   This replaces #135's line that the gate stays outside confinement. The commands are the
   operator's, but the code they run is the agent's.
7. **Host-side git.** Every git command crewd runs in a worktree runs with hooks off, and
   ignores repository config that the agent could have written. The worker's writable git state
   excludes the shared repository's `.git/config` and `.git/hooks`.
8. **Profiles.** crewd ships built-in `worker` and `gate` profiles. A TOML profile can `extend`
   one of them, within decision 4's rule. `preflight` rejects an unknown profile by name.
9. **`workflows: write` (#259).** `crewd init`'s manifest does not request it. Once egress is
   denied, delivery is the only path from an agent to `.github/workflows/`, and a workflow on a
   pushed branch can read the repository's secrets. An operator may grant it to their own
   installation. This deployment keeps its 2026-10-03 grant, and the exposure stays recorded in
   M8's description.

## Consequences

- **Slices.** #135 is split into M8 issues, each dispatchable once decision 1 is settled:
  - the `Sandbox` trait, the deny set, and wrapping for the worker and the gate profile,
    failing fast;
  - egress and its allowlists;
  - Grok under the same profile;
  - TOML profiles with `extend`, and `preflight` rejecting an unknown name;
  - host-side git without hooks or repository config.

  #135's acceptance criteria are spread across them. Each slice adds its row to
  [invariants.md](../invariants.md), with a guard test that fails when confinement is off.
- **What becomes harder.**
  - A test suite that needs the network or extra paths must have the operator extend the `gate`
    profile.
  - A tool that ignores the proxy variables fails inside the sandbox.
  - Linux operators install `bubblewrap`. On Ubuntu 24.04 and later they also allow
    unprivileged user namespaces in AppArmor, as Claude Code's own sandbox requires.
- **Still open, recorded rather than solved.**
  - The branch carries whatever an agent commits. Decisions 4 and 5 keep secrets out of its
    reach, and GitHub's push protection blocks known token formats, but nothing scans the diff
    before a push.
  - DNS lookups through the system resolver can carry a few bytes out, as they can under srt.
  - Apple marks `sandbox-exec` deprecated. Chrome, Claude Code and srt still depend on it; if it
    is removed, the macOS slice is revisited.
- **To verify before the native backend's egress slice is built on:**
  - that claude and grok honour `HTTPS_PROXY`;
  - that cargo fetches through the proxy;
  - whether Rust CLIs (grok, cargo) need `com.apple.trustd.agent` for TLS under a stricter base.
    The spike used an allow-default base with targeted denies, and that is the starting base.
- **Docs.** When the first slice lands:
  - architecture.md's "not an isolation boundary" section changes;
  - CLAUDE.md's rule against "the worker cannot reach a credential" stays true, because the
    keychain is still reachable;
  - docs/vision.md's "Sandboxing" row moves from Not yet to built.
