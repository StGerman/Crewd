# 3. A worker runs in the sandbox its config names

- **Status:** Proposed
- **Date:** 2026-10-04
- **Issues:** #135 and M8 (*A dispatched agent cannot act as the operator*), #138, #259. An
  earlier draft (PR #283, smolvm only) was discarded and binds nothing.
- **Evidence:** [docs/research/confinement.md](../research/confinement.md): desk research,
  competitor survey and two spikes.

## Context

A dispatched agent runs on the host as the operator, under `--permission-mode
bypassPermissions`. On macOS, `gh` and `claude` authenticate from the login keychain, so no
environment allowlist can take their credentials away. The agent can comment, push and close as
the operator ([architecture](../architecture.md), "Why the broker is not an isolation
boundary"). #135 fixed the bar on 2026-10-01 (criteria C1–C7 in the research file) and set the
lever: delivery pushes and the broker writes host-side, so the agent needs only the model API and
the crate registry. With `github.com` unreachable, a token read from the keychain cannot be used.

The research found:

- **No agent's built-in sandbox covers the whole process.** Claude Code's confines Bash,
  PowerShell and Monitor; Codex's, Cursor's and Copilot's confine spawned commands. Anthropic
  and OpenAI both point unattended runs at an external boundary.
- **Seatbelt on macOS and bubblewrap on Linux are the consensus mechanism** (Codex, Gemini CLI,
  `srt`, Copilot). Neither can name a host, so every whole-process design pairs it with a
  host-side allowlist proxy.
- **Both wrappers hold in spikes on this repository.** Under them, `claude -p` stream-json ran
  (Seatbelt), a commit landed, `cargo test` passed with no measurable overhead, and `github.com`
  was refused. Reads of the deployment directory and writes to the repository's hooks were
  denied.
- **Fail-open is the field's common failure.** Claude Code's `failIfUnavailable` defaults to
  false. goose shipped a sandbox, deleted it, and left users believing they were confined.

## Decision

### The setting

`[sandbox] mode` is one closed enum. It selects a boundary, never a posture:

| Mode | What it is | Lands |
|---|---|---|
| `off` | Today: the worker runs on the host as the operator. **The default** | — |
| `wrapper` | OS-native: Seatbelt on macOS, bubblewrap on Linux | M8 (macOS first, then Linux) |
| `microvm` | One smolvm microVM per run | M8, experimental, behind the gates below |
| `container`, `remote`, `kubernetes` | Reserved. crewd stops at startup with "not implemented" | Later; see Options considered |

- **No other switch.** There is no `enabled` flag and no fail-open knob. A config that names a
  mode crewd cannot start exits non-zero at startup and names the cause (rule 5). The only way
  to run unconfined is `mode = "off"`.
- **No silent fallback.** A run whose sandbox fails to start is a worker-level failure that
  pauses the worker (the #216 pattern). It is never charged to the issue, and never rerun
  unconfined.
- **Telling the sandbox's failure from the run's.** The exit status cannot do it: `sandbox-exec`
  execs in place, so once the agent runs, a status of 65 or 71 is the agent's. Under `wrapper`,
  the wrapper therefore starts crewd's own helper (crewd re-exec'd), not the agent. Before it
  execs the agent, the helper reports over an inherited pipe that the sandbox is up, or that the
  agent binary is missing.
  - **No report:** the sandbox failed to start. The worker pauses, and the failure names the
    wrapper (`sandbox-exec`, `bwrap`). The spawn-failure path today would report a missing
    wrapper as `AgentNotFound` and name the agent's binary.
  - **The binary is missing:** this keeps the `agent_not_found` clause of the rate-limit row in
    the invariant table. The worker pauses, naming the agent's path, and no issue is
    quarantined.
  - **A report, then any exit:** the run's own, handled as today.
  - **Under `microvm`:** the guest-side forwarder plays the helper's part (gate 4).
- **Visible state.** The effective mode is logged at startup and published on `Snapshot`, so
  `crewctl status` shows it.
- **No silent ignoring.** Config tables crewd does not know are refused at load, because an
  ignored `[sandbox]` is exactly the silent partial state rule 5 forbids.
- **Extending the defaults.** The operator extends them with `writable`, `deny_read` and
  `network.allow` lists. A list can add to the defaults and never shrink them:
  - the deny set over the deployment directory and the App key cannot be removed;
  - `github.com` and `api.github.com` in `network.allow` are a config error, because they reopen
    the hole this ADR closes.

### What every mode wraps

The whole worker process, for claude and grok alike, through **one spawn helper** shared by
both workers. Today each builds its own `Command`.

- **The gate's commands** (`cargo test`, `build.rs`, everything the operator listed) run under
  the same mode with their own allowlist: the crate hosts, no model API. This reverses #135's
  line that the gate stays outside: the code it runs is the agent's.
- **What stays on the host:** the gate's own git (fetch, rebase, merge), delivery's push and the
  reconcilers.
- **Host-side git trusts no file the agent could have written.** That is the worktree's `.git`
  file, `worktrees/<name>/gitdir`, `commondir` and `config.worktree`, and `<commondir>/config`.
  - These files are write-denied wherever the mode can deny a single file (`wrapper`).
  - In every mode, crewd checks them against what it wrote before each host-side git call. On a
    mismatch it parks the issue `Blocked`, naming the file.
  - Hooks and fsmonitor are also switched off on the command line
    (`-c core.hooksPath=/dev/null -c core.fsmonitor=false`).
  - The reason: a redirected `commondir` or `gitdir` would hand host-side git a config the agent
    wrote, and with it an exec-capable key such as `gpg.program`. microvm's directory mounts
    cannot deny a single file, so there the check is the only guard.
- **The agent's own sandbox is turned off** under any mode other than `off`
  (`--settings '{"sandbox":{"enabled":false}}'` for claude). A Seatbelt profile cannot apply
  another one (measured: exit 71), and the outer boundary is the one crewd can verify.

### Egress

**The proxy.** Under `wrapper`, crewd runs one loopback HTTP CONNECT allowlist proxy. Under
`microvm`, smolvm's own allowlist does this job (below), because a guest cannot reach host
loopback.

- It never terminates TLS.
- It refuses IP-literal targets and resolved loopback, link-local, private and metadata
  addresses.
- It keeps established tunnels open for at least 10 minutes.
- It logs each refusal into the run's transcript.
- A proxy that cannot bind stops startup.
- **The confined agent is its client by design, so it is bounded like crewd's other
  listeners.** That means a cap on connections in flight and a deadline on a request that has
  not finished its `CONNECT`: the property of the invariant rows for the MCP transport and the
  init listener, each with its guard test.
- **What it is built on:**
  - a well-maintained crate if the slice finds one that fits (nono's proxy is the first to
    evaluate);
  - otherwise it is served by the broker's listener in `src/broker/server.rs`, reusing its
    `Limits`. That adds no `std::thread::spawn` site beyond the two the coding guidelines allow,
    and keeps `tokio` where they confine it.

**The env.** Each `wrapper` run gets:

- `HTTPS_PROXY`/`HTTP_PROXY` (and the lowercase twins), carrying run-scoped credentials for
  attribution;
- `NO_PROXY=127.0.0.1,localhost`;
- `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`.

**Default hosts per worker:**

| Worker | Hosts |
|---|---|
| claude | `api.anthropic.com`, `platform.claude.com`, `claude.ai` |
| grok | `cli-chat-proxy.grok.com`, `auth.x.ai` |
| every worker and the gate | `index.crates.io`, `static.crates.io` |

Never `crates.io` (the publish API), and never GitHub.

**The OS rule is the boundary, not the env.** A client that ignores the proxy variables has no
route out.

### `wrapper` on macOS

crewd generates the SBPL profile and execs `/usr/bin/sandbox-exec -p <profile> -D … -- <argv>`,
which runs in place, so crewd's process-group kill holds (measured).

**Writes** are denied, then allowed only for:

- the run's worktree;
- the commondir, except `hooks/`, `config` and `worktrees/<name>/{gitdir,commondir,config.worktree}`,
  and except the worktree's own `.git` file;
- a private per-run `TMPDIR`, which also holds the run's MCP config and grok's prompt file, with
  `CLAUDE_CODE_TMPDIR` pointed into it;
- a crewd-owned `CARGO_HOME` per deployment, never `~/.cargo`, whose `bin/` is on the operator's
  PATH;
- `~/.claude/projects/<encoded worktree>/`, which is enough for `--resume` (measured).

`~/.claude.json`, settings, commands and hooks stay read-only, so an agent cannot plant code
into the operator's interactive sessions.

**Reads** are broad, minus a deny set:

- the deployment directory (config, hook sources, the App key);
- the daemon's `TMPDIR` entries, by exact path;
- `~/.ssh`, `~/.config/gh`, `~/.aws`, `~/.netrc`, `~/.cargo/credentials.toml`;
- other worktrees and `.transcripts/`.

A secret that remains readable can reach only the allowed hosts.

**Network:** outbound is denied except loopback, with direct DNS refused (measured).

- Loopback is allowed whole, because an agent's tests bind loopback servers, and SBPL cannot
  carve one port out of `localhost:*` (measured).
- **The ops API therefore requires a bearer token** on its mutating routes and on the ops MCP
  endpoint whenever the mode is not `off`. The token is kept in the deployment directory, which
  the profile denies; `crewctl` and the supervising session read it.

**Mach services stay allowed.** The keychain is reachable, which claude's own login needs, and
the token it yields is useless without `github.com` (measured). Denying securityd in favour of
token auth is a follow-up, not decided here.

**Startup canary.** It never depends on an outside host answering, so a model-API or DNS outage
at boot stays a per-run failure:

- the profile compiles and the helper reports ready;
- a read of a sentinel in the deployment directory fails;
- a direct connection to a non-loopback address is refused;
- the proxy's policy refuses a non-allowlisted host and admits an allowlisted one. This is
  checked against the policy, not by connecting upstream.

### `wrapper` on Linux

crewd generates bubblewrap argv against the system `bwrap`, and never falls back to a bundled
one.

- **Namespaces:** every one is named (`--unshare-user --unshare-ipc --unshare-pid --unshare-net
  --unshare-uts`), never `--unshare-all`, whose `-try` variants skip silently.
- **Mounts:** writes are the same allowlist as macOS (binds over a read-only root). The deny set
  is masked with tmpfs. The git files host-side git trusts are bound read-only, the same set as
  macOS: `hooks/`, `config`, the worktree's `.git` file, and `worktrees/<name>/gitdir`,
  `commondir` and `config.worktree`.
- **Network:** `--unshare-net`. The same crewd helper that reports readiness also bridges
  loopback ports to bind-mounted Unix sockets for the proxy and the broker. It then applies a
  seccomp filter against new `AF_UNIX` sockets and execs the agent. That is the Codex and `srt` design,
  without `socat` or Node. The host's loopback, ops API included, is unreachable by
  construction.
- **Kill:** `--die-with-parent` and no `--new-session`, so a crashed daemon leaves no agent
  behind.
  - The cost is measured: SIGTERM's grace collapses into SIGKILL under bwrap.
  - `--new-session` without `--die-with-parent` orphaned the whole tree in the spike, and is
    never used.
  - If losing the grace costs claude's session file on resume, the slice moves the first signal
    to the in-sandbox helper.
- **Startup probe:** `bwrap --unshare-user --unshare-net --ro-bind / / true`. On Ubuntu 24.04+
  it fails with `Failed RTM_NEWADDR` until root installs an AppArmor profile for `bwrap`. crewd
  exits naming that fix.

This mode breaches C6 on Linux: it needs the `bubblewrap` package, and one root step on Ubuntu.
The ADR accepts that cost. `off` stays free.

### `microvm`

**smolvm**, driven by argv in front of the worker, as an opt-in mode labelled experimental. Per
run:

- `machine run --stream --net`, with the worktree and the main `.git` mounted at identical
  absolute paths, plus the crewd-owned cargo home;
- `--allow-host-pattern` for the default hosts;
- `--credential` substitution of `CLAUDE_CODE_OAUTH_TOKEN`, so the guest holds a placeholder.
  This mode therefore requires token auth;
- a non-root user in an image that has git.

Startup resolves smolvm and boots a probe VM. A binary without the hypervisor entitlement (smolvm
releases are unsigned, and a re-signed build loses it) or a host without a hypervisor stops
startup.

**It ships only after these gates pass on the dogfood machine**, and moves to *later* if any
fails:

1. `--stream` yields stream-json line by line with the prompt delivered.
2. SIGKILL of crewd's process group leaves no VM in `smolvm machine ls` (a guard test).
3. `machine run` accepts `--mount-socket`.
4. The broker works through that socket and a guest-side forwarder. The forwarder also reports
   readiness, so a VM that fails to boot is told apart from the run's own exit.

The argv builder sits behind a small backend seam. microsandbox (once out of beta) or QEMU
(user-mode networking with `restrict=on`) can be added there without changing the mode.

## Options considered

| Option | Verdict | Why |
|---|---|---|
| Claude Code's own sandbox, `srt` | Rejected as the mechanism | Built-in: only Bash, PowerShell and Monitor, fails open by default, and cannot nest. `srt`: needs Node, a research preview (v0.0.78), wraps via `shell -c` |
| Landlock in-process (Linux) | Rejected | Network rules are ports only, so it cannot tell `api.anthropic.com:443` from `github.com:443` |
| nono | Not the mechanism | Pre-1.0, eight months old. Its proxy is the first dependency to evaluate |
| A pf/nftables firewall, DNS filtering | Rejected | Root, or bypassed by hard-coded IPs |
| `container` (docker, podman) | Later | A new product on macOS (C6); no domain allowlist; `run` does not forward SIGKILL, so a kill orphans the container; claude's token sits inside the boundary. Fits Linux servers and CI later |
| Docker Sandboxes (`sbx`) | Not adopted | Closed source, needs a Docker sign-in, mounts only the worktree (the `.git` pointer is unresolvable) |
| Apple `container` | Later | macOS 26 only, no egress policy; an open report says `--internal` still routes out |
| Firecracker, cloud-hypervisor, crosvm, Kata | Rejected for `microvm` | Linux/KVM only, so a macOS operator needs a VM first. Firecracker does no traffic filtering; crosvm has no releases; Kata belongs under `kubernetes` |
| `remote` (E2B, Vercel, Modal, Daytona, …) | Later, out of tree (ADR 1) | Needs a new workspace model (a bundle in, a bundle out), a public path to the broker, a SaaS account, and an API key instead of keychain login. E2B first if built: its `envd` protocol streams stdio and signals |
| `kubernetes` (k3s/k8s) | Later | A cluster, Cilium for FQDN egress, a gVisor or Kata RuntimeClass, and about 76 more crates (kube-rs). agent-sandbox is `v1beta1` and allows the public internet by default |

## Consequences

- **The slices.** #135 is split into M8 issues in value order. Each adds its invariant row with
  a guard test that fails without its mechanism:
  1. **The seam.**
     - the closed enum with reserved names, refusing unknown tables, and the effective mode on
       `Snapshot`;
     - the shared spawn helper for both workers and the gate;
     - host-side git checking the files it trusts, which applies in every mode, `off` included.

     No confining mode lands yet.
  2. **The egress proxy.** OS-neutral, with the per-worker allowlist, the env wiring and the
     listener bounds.
  3. **`wrapper` on macOS.**
     - the profile and the canary;
     - the readiness helper and the failure classification it enables;
     - the inner sandbox off;
     - a private `TMPDIR` and `CLAUDE_CODE_TMPDIR`, the crewd-owned `CARGO_HOME`, and the narrow
       `~/.claude`;
     - the ops API token.

     The crewd-written `--settings` is a settings source that the invariant row "A dispatched
     agent's Claude Code configuration comes from the repository and the broker" rules out
     today. This slice amends that row and its argv guard
     (`the_worker_argv_loads_only_project_settings_and_the_broker`).
  4. **The gate under the worker's mode.**
  5. **`wrapper` on Linux.** The argv builder, the in-binary bridge helper, the seccomp filter
     and the probe. CI needs the AppArmor step on `ubuntu-24.04` runners.
  6. **grok under `wrapper`.** It was not available to the spike. Its credential is a file,
     `~/.grok/auth.json`, so its profile differs from claude's.
  7. **`microvm`** behind its gates.
  8. **Profiles that `extend`**, with `preflight` rejecting an unknown profile (#135's original
     list).
- **What the sandbox still allows.**
  - Under `wrapper` on macOS the keychain stays reachable: a token can be read, though not used
    against GitHub.
  - Whatever the agent can read can reach the model API and the crate registry's download
    hosts.
  - `off` remains exactly today's exposure.
  - The vision's Sandboxing row turns from "Not yet" only when slice 3 lands, and it says this.
- **Docs and settings.** Each slice updates the docs its row touches: `docs/vision.md`'s
  Sandboxing row; architecture's isolation section; CLAUDE.md's worker and broker constraints;
  `docs/invariants.md`; the shipped configs; and the `crewd init` template.
- **#259** is unchanged. With `github.com` refused, an agent cannot push itself; the
  `workflows: write` question concerns delivery's own push and stays there.
- **#138** (redaction) applies under every mode and is not replaced by this one.
- **Revisit** if Apple removes `sandbox-exec`: the fallback is a crewd helper that calls
  `sandbox_init` and execs.
