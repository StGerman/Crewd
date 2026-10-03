# 3. Confining a dispatched agent: an opt-in smolvm microVM around the worker and the gate

- **Status:** Proposed. The operator took these decisions in a supervised session on
  2026-10-03, after a spike. The ADR is Accepted when it merges.
- **Date:** 2026-10-03
- **Issues:** #135 (the umbrella and its criteria), #138 (redaction), #259 (`workflows: write`),
  milestone M8

## Context

A dispatched agent runs `claude -p --permission-mode bypassPermissions` or
`grok --always-approve` as the operator. The broker is not an isolation boundary
([architecture.md](../architecture.md), "Why the broker is not an isolation boundary"). On
macOS, `gh` and `claude` read the login keychain, so an agent can comment, push and close as the
operator.

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
stranger beyond what the agent CLI needs. CLAUDE.md rule 5 says that anything the config turns on
starts, or crewd exits naming it.

The options considered:

- **Claude Code's built-in sandbox** confines Bash, PowerShell and Monitor commands only. The
  file tools, hooks, MCP servers and LSP servers run outside it, and it does nothing for Grok.
- **srt** wraps any command, but it is installed from npm, needs `ripgrep`, and is a beta
  research preview.
- **A native profile written by crewd.** This means Seatbelt via `/usr/bin/sandbox-exec` on
  macOS, and `bubblewrap` with crewd's own loopback proxy on Linux. A spike showed it works:
  - `github.com` was unreachable;
  - `claude` ran;
  - reads of `~/.crewd` were refused;
  - running and copying `gh` were refused.

  But it shares the host kernel and the keychain, so it needs a blocklist of credential
  helpers. It also rests on a tool Apple marks deprecated, and crewd would own a proxy and a
  profile language.
- **Separate uid; Docker or Lima.** These need administrator setup or a new manager.
- **Firecracker.** It needs Linux with KVM, has no virtio-fs, and does not run on macOS.
- **smolvm** (Apache-2.0, built on libkrun, version 1.22.2 at the time). It runs on
  Hypervisor.framework on macOS and on KVM on Linux. The guest has its own kernel, and nothing
  from the host that is not mounted. A spike on the dogfood Mac, against a throwaway clone,
  found:
  - **Boot:** about 1 s warm.
  - **Egress:** `--allow-host` filters by DNS and by IP. crates.io and the Anthropic API were
    reachable; `github.com` got NXDOMAIN, and a direct connection to its IP was refused.
  - **Mount:** the worktree mounted read-write over virtio-fs. A cold `cargo test` took 248 s on
    4 vCPUs, against 218 s natively on 10 cores.
  - **Guest image:** the image needs `git` and a non-root user. As root, a test that expects an
    unexecutable file failed.
  - **Claude:** `claude -p --output-format stream-json` installed and ran in the guest. With
    `--credential`, the guest saw only a placeholder, and the host's value reached the API.
  - **Host services:** a guest cannot reach the host's loopback, only a service bound to the
    host's LAN address.
  - **Signing:** `smolvm-bin` is ad-hoc signed with `com.apple.security.hypervisor`, and ships
    libkrun and libkrunfw as bundled libraries.
  - **Grok:** it ships Linux arm64 builds.

## Decision

1. **Confinement is opt-in, and smolvm is its only backend.** With `[sandbox]` unset, a worker
   runs as it does today: on the host, as the operator, unconfined. With
   `[sandbox] backend = "smolvm"`, every run is confined as below. There is no native backend.
   The default keeps a stranger's setup at zero. The operator who wants the boundary pays for
   the strongest one available: an image, a token and, on Linux, KVM.
2. **Mechanism.** A `Sandbox` trait (ADR 1, boundary 1), with one implementation that drives the
   operator-installed `smolvm` binary through `Command::new` and argv, never a shell. Because it
   is opt-in, the binary is the operator's dependency, and crewd neither embeds libkrun nor signs
   itself. With the backend on, `preflight` resolves `smolvm`, checks its version against the
   one this ADR was verified on, and checks that it can start a guest; otherwise crewd exits at
   startup (rule 5). A run whose guest cannot start fails. It never falls back to running
   unconfined.
3. **What runs in a guest.** The whole worker process, claude or grok, and so everything it
   starts. The gate's commands also run in a guest, under the gate profile (decision 8).
4. **Mounts replace a deny set.** A guest sees:
   - the worktree, read-write;
   - the git state its commits need;
   - the cargo registry cache.

   The shared repository's `.git/config` and `.git/hooks` are never writable. Nothing else of
   the host is visible: no keychain, no `~/.crewd`, no deployment directory.
5. **Egress.** The worker profile allows the worker's model API (`api.anthropic.com` for claude,
   xAI's API for grok) and the crate registry, as exact host patterns. No default list contains
   GitHub. Delivery pushes and the broker writes host-side, so a confined agent never needs it.
6. **Credentials.** The agent's login reaches the guest only as a smolvm `--credential`: the
   guest holds a placeholder, and the host substitutes the real value only on HTTPS requests to
   the model API. Claude's comes from `claude setup-token` (`CLAUDE_CODE_OAUTH_TOKEN`), and
   grok's from its API key.
7. **The broker.** The guest reaches the broker without the broker listening on the LAN. A
   LAN-bound broker is rejected. The first slice finds the path, for example a vsock or socket
   bridge, and the backend is unusable until it exists.
8. **The gate** runs in its own guest with the gate profile:
   - the same mounts;
   - egress to the crate registry only;
   - hosts and resources the operator adds per deployment.

   The commands are the operator's, but the code they run is the agent's. This replaces #135's
   line that the gate stays outside confinement.
9. **Host-side git.** Every git command crewd runs in a worktree runs with hooks off, and ignores
   repository config that the agent could have written. This applies with or without the
   sandbox.
10. **The image.** The operator names an OCI image in the config. It carries the agent CLIs, the
    repository's toolchain, `git`, and a non-root user that the guest runs as. crewd documents a
    reference image.
11. **Profiles.** crewd ships built-in `worker` and `gate` profiles. A TOML profile can `extend`
    one of them, and only adds hosts, mounts or resources. `preflight` rejects an unknown
    profile by name.
12. **`workflows: write` (#259).** `crewd init`'s manifest does not request it. An operator may
    grant it to their own installation. This deployment keeps its 2026-10-03 grant, and the
    exposure stays recorded in M8's description.

## Consequences

- **The default is unchanged.** A deployment that does not opt in keeps today's exposure: a
  dispatched agent can act as the operator. The README's "What it will not do" and
  docs/vision.md's "Sandboxing" row say that confinement is opt-in. M8's outcome holds for an
  operator who opts in.
- **Slices.** #135 is split into M8 issues, each dispatchable once this merges:
  - the trait, the smolvm backend with `preflight`, and a broker path that does not listen on
    the LAN. This one gates the rest;
  - mounts and egress for the worker, plus credential substitution for claude and grok;
  - the gate in its own guest;
  - the reference image;
  - host-side git without hooks or repository config;
  - TOML profiles with `extend`.

  #135's acceptance criteria are spread across them. Each slice adds its row to
  [invariants.md](../invariants.md), with a guard test. #138 stays independent.
- **What opting in costs.**
  - Installing smolvm. It is not in Homebrew; the installer puts it under `~/.smolvm`.
  - An image pull of a gigabyte or more.
  - `claude setup-token`.
  - KVM on Linux.
  - On macOS, the gate's results are Linux results, the same as CI and not the same as the
    host.
  - Builds are bounded by the guest's vCPUs, which the profile sets.
- **Still open, recorded rather than solved.**
  - The branch carries whatever an agent commits. GitHub's push protection blocks known token
    formats, but nothing scans the diff before a push.
  - smolvm resolves `--allow-host` names to addresses when the guest starts, so another host
    behind the same CDN address is reachable too.
  - smolvm is young, and its flags may change. `preflight` pins the version this ADR was
    verified on, and a newer one is verified before the pin moves.
- **Docs.** When the backend lands:
  - architecture.md's "not an isolation boundary" section names the opt-in;
  - CLAUDE.md's rule against "the worker cannot reach a credential" stays true for the default.
