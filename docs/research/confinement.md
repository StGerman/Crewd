# Confining the worker: research behind ADR 3

What a `[sandbox] mode` for crewd's worker can be built on, what comparable products use and
expose, and what two spikes on the dogfood machine measured. [ADR 3](../adr/0003-sandbox-modes.md)
records the decision; this file is its evidence. Facts are as of 2026-10-03 unless dated
otherwise, and the space moves monthly, so re-check a version or a star count before reusing it.

## The question

A dispatched agent runs on the host as the operator. On macOS `gh` and `claude` authenticate from
the login keychain, so the agent can comment, push and close as the operator whatever the
environment allowlist removes (#135; [architecture](../architecture.md), "Why the broker is not
an isolation boundary"). Every mode is judged against #135's criteria:

| | Criterion |
|---|---|
| C1 | Headless stream-json over stdio still works |
| C2 | The worktree, its git directory (the commondir in the main repo) and the cargo caches are writable, and `cargo` builds |
| C3 | Egress reaches the model API and the crate registry only; `github.com` is refused |
| C4 | Reads of the daemon config, hook sources and the GitHub App key are denied |
| C5 | A sandbox that cannot start fails closed: crewd exits at startup or fails the run, never runs unconfined |
| C6 | No new product dependency for a stranger beyond what the agent CLI needs (binding on `off` and `wrapper`) |
| C7 | The whole worker process is wrapped, claude and grok alike, not only the agent's shell tool |

## Method

- **Desk research:** workflow run `wf_c85d49d2-ea6`, 2026-10-03.
  - Two Haiku scouts swept for candidates beyond a seed list.
  - Sonnet deep-read nine clusters, one agent each: macOS wrapper, Linux wrapper, egress,
    microVM, container, remote, Kubernetes, agent CLIs, runners. Every candidate was scored
    against C1–C7 from primary sources.
  - A Sonnet verifier per cluster tried to refute the load-bearing claims. A Sonnet critic
    looked for gaps across clusters.
- **Follow-up:** Sonnet evaluated every platform in a [microVM platform
  survey](https://www.pistack.xyz/posts/2026-05-09-microvm-platforms-firecracker-cloud-hypervisor-crosvm-guide/)
  against its projects' own sources.
- **Spikes:**
  - Seatbelt on macOS 26.6.2 (M4, 10 cores), claude 2.1.288, cargo 1.97.1.
  - bubblewrap 0.9.0 in a Lima 2.2.1 Ubuntu 24.04.5 VM (kernel 6.8, 6 vCPUs).
  - Both ran against a clone of this repository with a worktree laid out as crewd lays it out,
    and an allowlisting HTTP CONNECT proxy in front.
- **Earlier evidence still used:** the smolvm 1.22.2 spike of 2026-10-03 (the discarded PR
  #283).

The critic's open questions that are decisions, not facts (gate confinement, ops API exposure,
microVM timing, Linux timing) went to the operator and are answered in the ADR.

## Findings by mode

### `wrapper` on macOS: Seatbelt

- **Mechanism:** crewd generates an SBPL profile and execs `/usr/bin/sandbox-exec -p <profile>
  -D k=v -- <worker argv>`. This is the shape Codex, Gemini CLI and Anthropic's `srt` use.
- **Deprecation:** `sandbox-exec` has been marked deprecated since its 2017 man page. It is
  still shipped and dated 2026-08-13 on macOS 26.6.2, and all three of those products depend on
  it.
- **Measured: it fits crewd's spawn and kill model.**
  - It execs in place: the same pid, stdio passed through.
  - `killpg(SIGTERM)` on crewd's process group took down the sandboxed shell and its background
    children, leaving no strays.
  - `claude -p --output-format stream-json` ran with keychain login, and a `--resume` turn
    worked.
  - `cargo test` of this repository took 38.9 s sandboxed against 43.7 s natively (warm, so the
    difference is noise).
- **Egress needs a host proxy.** The profile compiler accepts only `*` or `localhost` as a
  remote host (verified). The profile therefore allows loopback, and a host-side CONNECT
  allowlist proxy does the domain filtering.
  - Direct DNS fails inside the profile.
  - A direct connection to a GitHub IP was refused.
  - `github.com` through the proxy got 403, `git ls-remote` against GitHub failed, and the
    crates index went through.
- **Loopback is all or nothing (measured).**
  - Allowing only the proxy and broker ports breaks tests that bind loopback servers: two
    `crewctl` tests here fail with "Operation not permitted".
  - Allowing `localhost:*` fixes them, but SBPL then ignores a narrower deny of the ops API port
    (8787). A request from inside the sandbox reached the live ops API whichever order the rules
    were in.
  - A deny of `localhost:8787` on its own does work.
  - The ops API therefore needs its own protection under `wrapper`.
- **Profiles cannot nest (measured).** Claude Code's own Bash sandbox, enabled inside an outer
  profile, fails with `sandbox-exec: sandbox_apply: Operation not permitted` (exit 71).
  - With `failIfUnavailable: true` set, the model then retried the command unsandboxed. The
    outer profile still applied.
  - Codex issue #45657 reports the same exit 71.
- **The keychain stays reachable**, and that is accepted.
  - `gh auth token` with an empty `GH_CONFIG_DIR` printed the operator's token inside the
    profile, and `security find-generic-password -s gh:github.com` found the item.
  - Neither token is usable once `github.com` egress is refused. That is the egress-is-the-lever
    decision of #135, now measured.
  - Denying securityd would also break claude's own keychain login.
  - Among `srt`, Codex and nono, only nono denies the keychain Mach services.
- **The agent's own state can be narrowed (measured).**
  - `claude -p` writes `~/.claude/projects/<encoded cwd>/<session>.jsonl` and, on some runs,
    `~/.claude.json`.
  - A profile that leaves only `~/.claude/projects/<encoded worktree>/` writable, with
    `~/.claude.json` read-only, still ran a session and resumed it.
  - The Bash tool needs `$CLAUDE_CODE_TMPDIR/claude-<uid>/` writable. It ignores `TMPDIR`, and
    defaults to `/tmp`.
- **Startup cost.**
  - The first run in a new directory took 24.6 s: a one-time initialisation of `~/.claude`,
    using 60 s of CPU.
  - Later runs took 5.8 s against 3.3 s natively. The proxy was refusing telemetry and
    `mcp-proxy.anthropic.com` calls, which Claude retried.
  - With `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` it took 2.1 s, the same as native.
- **Deny by path, not by name.** A read deny on the pattern `*/crewd-push-*` failed one of this
  repository's own tests (`the_gate_fetch_resolves_the_push_token_ahead_of_an_ambient_helper`),
  because the test creates files with that name in its temp directory. Deny the daemon's exact
  directories, and give each run a private `TMPDIR`.
- **TLS:** `cargo fetch` with a fresh `CARGO_HOME` through the proxy took 8.6 s, and TLS was
  fine. Go binaries (`gh`, `gcloud`) need `com.apple.trustd.agent` for TLS: this is documented
  by `srt` and Claude Code, and a research agent observed it but nobody re-measured it.
- **Rejected as the mechanism:**
  - `srt`: it needs Node and is a research preview (v0.0.78).
  - nono as a binary: pre-1.0 (v0.79.0, repository created 2026-01-31). It stays a candidate
    source for the proxy.
  - Apple's App Sandbox and Endpoint Security: both fail C6 and C7.

Verdict: **ship in M8.**

### `wrapper` on Linux: bubblewrap

- **Mechanism:** bubblewrap with crewd-generated argv. Codex and `srt` both settled on the same
  design: `--unshare-net`, a bind-mounted Unix socket bridged to a host-side allowlist proxy,
  and a seccomp filter that then blocks new `AF_UNIX` sockets.
- **Measured: Ubuntu 24.04.5 blocks it until root acts.**
  - `kernel.apparmor_restrict_unprivileged_userns=1` makes `bwrap --unshare-user --unshare-net`
    fail with `loopback: Failed RTM_NEWADDR: Operation not permitted`.
  - Without `--unshare-net` it fails with `setting up uid map: Permission denied`.
  - A root-installed `/etc/apparmor.d/bwrap` (`profile bwrap /usr/bin/bwrap
    flags=(unconfined) { userns, }`) fixes it.
  - That is the stranger's cost on Ubuntu: `apt install bubblewrap` plus one root step.
- **Measured: the confinement holds.**
  - The deployment-dir sentinel was masked by a tmpfs.
  - Direct DNS failed, `github.com` was refused by the proxy, and the crates index went
    through.
  - Writes to `$HOME` and to the commondir's `hooks/` hit a read-only file system.
  - A commit in the worktree succeeded, and a `fake_claude` fixture's stream-json passed
    through stdio.
  - `cargo test` passed 783 of 783 sandboxed, and the warm run took 7.56 s against 7.50 s
    natively.
  - Loopback servers work inside the sandbox's own network namespace, and the host's loopback
    (ops API, broker) is unreachable by construction.
- **Measured: kill.**
  - With `--die-with-parent`, a `killpg(SIGTERM)` ends the whole tree, but the inner process
    never runs its TERM handler: the grace period collapses into SIGKILL.
  - Without `--die-with-parent` and without `--new-session`, the inner process ran its TERM
    handler.
  - Without `--die-with-parent` but **with** `--new-session`, the inner tree **survived** the
    kill.
  - `--new-session` without `--die-with-parent` must never be used.
- **Rejected:**
  - Landlock: its network rules are TCP and UDP ports only, so it cannot tell
    `api.anthropic.com:443` from `github.com:443`, which fails C3.
  - systemd `--user`: namespacing in user services needs `PrivateUsers=`, and IP filtering is
    system-only.
  - firejail, nsjail, minijail and AppArmor: each fails C6 or C3.
  - seccomp-pledge: defunct.

Verdict: **ship in M8, after macOS.** It reuses the macOS proxy.

### Egress, shared by every mode

- **The proxy design.** Neither Seatbelt nor bubblewrap can name a host. Every serious design
  (srt, Codex, Docker Sandboxes, smolvm, Anthropic's secure-deployment guide) leaves the worker
  one route, to a host-side proxy, and sets `HTTPS_PROXY`.
- **Client support.** Claude Code honours `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` and does not
  support SOCKS; cargo, git, rustup and grok honour the HTTP variables.
- **Cargo's proxy order** (corrected by the verifier): `http.proxy`/`CARGO_HTTP_PROXY`, then
  git's `http.proxy`, then `HTTPS_PROXY`/`https_proxy` for HTTPS.
- **No TLS termination.** A CONNECT allowlist that never terminates TLS is enough for C3 and
  needs no CA.
- **Hosts:**

  | Worker or tool | Hosts it needs | Note |
  |---|---|---|
  | claude | `api.anthropic.com`; `platform.claude.com` and `claude.ai` for the OAuth exchange and refresh | Telemetry and `mcp-proxy.anthropic.com` are optional and are turned off |
  | grok | `cli-chat-proxy.grok.com` and `auth.x.ai` | Its docs ask for idle timeouts of at least 10 minutes |
  | cargo | `index.crates.io` and `static.crates.io` | `crates.io` itself is the publish and owner API, so it is an exfiltration path and is left out |

- **Credential substitution is separate and harder.**
  - Without TLS termination it works only through a loopback reverse proxy reached via
    `ANTHROPIC_BASE_URL`, which is documented for sampling requests.
  - smolvm terminates TLS only for the hosts a credential binding names, using a
    name-constrained CA.
  - It is deferred for `wrapper`.
- **Rejected:**
  - pf and nftables: both need root.
  - DNS-only filtering: hard-coded IPs bypass it.
  - tinyproxy, squid and Envoy: each is a new product.
  - Seekrit (zero stars) and Prax (Python, zero stars).
  - Iron-proxy (Go) and Pipelock (Go) are designs to learn from, not dependencies.

### `microvm`

- **smolvm is the one backend that meets C3, C4 and C7 through a plain argv call on both macOS
  arm64 and Linux.**
  - virtiofs directory mounts.
  - A network that is off by default, with `--allow-host`, `--allow-host-pattern` and
    `--allow-cidr`.
  - `--credential NAME=ENV@host`, so the real token never enters the guest.
  - `machine run` ties the VM's life to the CLI process; the source comment says SIGKILL of the
    CLI is how orchestrators enforce timeouts.
- **Open: output streaming.** Without `--stream` the output is buffered until exit, and stdin
  forwarding under `--stream` was not found.
- **Open: the broker.** A guest cannot reach host loopback (the spike). `--mount-socket` exists
  in the CLI source, but whether `machine run` accepts it is unverified.
- **Open: hooks and config.** smolvm mounts directories only, so it cannot make `.git/config`
  read-only inside a writable `.git`.
- **Maturity:** 6,560 stars, Apache-2.0, v1.22.2 released 2026-10-02, one named maintainer,
  unsigned releases. A re-signed build loses the hypervisor entitlement and fails with
  `EINVAL`.
- **Second choices and rejections:**
  - **microsandbox** (8,544 stars, v0.7.6): the closest second, with richer egress rules. But it
    is beta, its egress is open by default, and its stdio and kill behaviour is unverified.
  - **QEMU** (from the pistack follow-up): a second-tier candidate. It runs on HVF and KVM, and
    `-netdev user,restrict=on,guestfwd=…` gives an egress path and a broker path without root.
    But only 9p sharing is available on macOS, so `cargo` over the share is the weak spot, and
    crewd would own the kernel, rootfs and init.
  - **Apple `container`** (1.5.0, macOS 26 only): no egress allowlist, and a report that
    `--internal` still allows outbound TCP is open and disputed.
  - **Firecracker, cloud-hypervisor, crosvm, Dragonball and Kata:** Linux/KVM only.
    - Firecracker does no traffic filtering.
    - crosvm has no releases and stubs for macOS.
    - Kata belongs under Kubernetes.
  - **Lima:** good mounts and no egress control.
  - **Unikraft:** no `fork`. **Ignite:** archived.
  - **Hosted microVMs** (AWS Lambda MicroVMs, Fly Sprites, Blaxel): these belong to `remote`.

Verdict: **M8, experimental, behind gates** (the ADR names the gates).

### `container`

- **Shape:** a generic OCI wrapper (`docker` or rootless `podman` `run -i --rm`) is the only
  container shape that fits the spawn model, and it passes C1, C4 and C7 by construction.
- **It fails C6.** On macOS it needs Docker Desktop (paid above 250 employees or $10M), OrbStack,
  Colima or a podman machine.
- **C3 is not turnkey.**
  - No engine ships a domain allowlist.
  - Anthropic's reference `init-firewall.sh` allows GitHub's ranges, outbound SSH, DNS and the
    host's /24, and needs `NET_ADMIN`.
- **Kill breaks:** `docker run` and `podman run` do not forward SIGKILL, so killing crewd's
  process group would orphan a running container.
- **Auth:** a container has no keychain, so claude needs `CLAUDE_CODE_OAUTH_TOKEN` inside the
  boundary.
- **Docker Sandboxes:**
  - `docker sandbox` has been removed; the product is now the closed-source `sbx`.
  - It is really a microVM product, with Balanced and Locked Down network policies that deny by
    default.
  - It needs a Docker sign-in.
  - It mounts only the worktree, so the `.git` pointer is unresolvable.
- **Dropped:** container-use and Dagger. The agent stays on the host and only its tools go into
  the container.

Verdict: **later.** It earns its place on Linux servers and CI where an engine is already
installed.

### `remote`

- **What every hosted service gives:** C4 by construction. E2B, Cloudflare and Vercel document
  `claude --output-format stream-json` running in the guest.
- **What conflicts with crewd's design:**
  - No host path exists in the guest, and `github.com` is refused, so remote needs a new
    workspace model: a self-contained clone in, a git bundle out, and commits imported
    host-side.
  - No provider documents a path from the guest to host loopback for the broker.
  - No provider has a Rust SDK.
  - Each adds a SaaS account and billing, an API key instead of keychain login, and source code
    leaving the operator's network.
- **Providers:**
  - E2B is the only one with a public, language-neutral in-guest protocol: `envd`
    `process.proto` with streaming `Start`, `SendInput`, `CloseStdin` and `SendSignal`. Its
    runtime is Apache-2.0, and its allow lists work by SNI.
  - Vercel has the best credential brokering, but no documented stdin.
  - Daytona's open repository is archived; development went private in June 2026.
  - Fly Machines' exec does not stream.
- **Dropped:** Cased, Rivet and Google Managed Agents.

Verdict: **later**, behind the sandbox seam, preferably out of tree (ADR 1).

### `kubernetes` (self-hosted k3s or k8s)

- **Shape:** one plain Pod per run (`restartPolicy: Never`, `stdin` plus `stdinOnce`) can meet
  C1, C3, C4 and C7.
- **But only with a stack no stranger has:**
  - a Linux cluster (k3s is Linux only);
  - Cilium, because stock NetworkPolicy has no FQDN allowlist;
  - a gVisor or Kata RuntimeClass (Kata needs nested virtualisation or bare metal);
  - an agent image;
  - kube-rs with `ws`, measured at about 76 extra crates.
- **kubernetes-sigs/agent-sandbox:**
  - v1.0.5, 4,134 stars, API still `v1beta1`.
  - Its managed default egress allows the public internet, and FQDN rules are on its roadmap.
  - Per-claim env forces a cold start, so warm pools do not help crewd.
- **Prior art:** OpenHands' Kubernetes backend maps a sandbox to an agent-sandbox claim and talks
  HTTP, not stdio.

Verdict: **later**, for an operator who already runs crewd in a cluster.

## The criteria against each mode's recommended implementation

| Mode, implementation | C1 | C2 | C3 | C4 | C5 | C6 | C7 | Verdict |
|---|---|---|---|---|---|---|---|---|
| `off` (today) | pass | pass | fail | fail | n/a | pass | fail | default |
| `wrapper`, macOS: generated SBPL + crewd proxy | pass (spike) | pass (spike) | pass (spike, via proxy) | pass (spike) | canary | pass | pass | M8 |
| `wrapper`, Linux: generated bwrap argv + bridge + crewd proxy | pass (spike, fixture) | pass (spike) | pass (spike, via proxy) | pass (spike) | probe | partial: bwrap package, root step on Ubuntu 24.04 | pass | M8, after macOS |
| `microvm`: smolvm | partial: `--stream` unverified | pass | pass (spike) | pass | partial | fail (allowed outside `wrapper`) | pass | M8, experimental, gated |
| `container`: docker or podman | pass | partial | needs crewd proxy | pass | pass | fail | partial: kill orphans | later |
| `remote`: E2B first | pass | partial: new workspace model | pass | pass | pass | fail | pass | later |
| `kubernetes`: Pod + gVisor + Cilium | partial | partial | pass with Cilium | pass | partial | fail | pass | later |

## What competitors use and expose

### Agent CLIs

| Product | Default | Mechanism | Wraps the whole process | Option surface |
|---|---|---|---|---|
| Codex CLI | Sandboxed (`exec`: read-only) | Seatbelt; bwrap + seccomp, managed proxy over a UDS bridge | No, spawned commands only | `--sandbox read-only\|workspace-write\|danger-full-access`; `sandbox_workspace_write.network_access` (false); `writable_roots`; domain allow/deny, deny wins; `.git`, `.codex` read-only under writable roots |
| Claude Code | Off | srt: Seatbelt; bwrap + socat | No: Bash, PowerShell, Monitor only | `sandbox.enabled`; `failIfUnavailable` (false, fails open); `allowUnsandboxedCommands`; `filesystem.allowWrite/denyRead`; `network.allowedDomains/strictAllowlist`; `credentials` with mask |
| Gemini CLI | Off | `sandbox-exec` relaunch of the whole CLI; docker, podman, runsc, lxc | Yes (the legacy path) | `GEMINI_SANDBOX=true\|docker\|podman\|sandbox-exec\|runsc\|lxc`; `SEATBELT_PROFILE=permissive\|restrictive\|strict-open\|proxied`; fails closed when a requested sandbox is missing |
| Cursor CLI | Not documented for headless | Seatbelt; Landlock + seccomp | No, terminal only | `--sandbox enabled\|disabled`; `sandbox.json` `type=workspace_readwrite\|workspace_readonly\|insecure_none`; `networkPolicy{default,allow,deny}`; falls back to prompts on old kernels |
| Copilot CLI | Off (experimental) | Microsoft MXC: Seatbelt, bwrap | No | `/sandbox enable`; network and keychain toggles (keychain off) |
| goose | None | Seatbelt sandbox shipped in v1.25.0 and deleted two weeks later | n/a | `GOOSE_SANDBOX` (removed; users still believed it worked months later) |
| opencode, Amp | None locally | Permissions only (Amp: remote "Orbs") | n/a | Permission rules |

### Runners and hosted agents

| Product | Where the agent runs | Option surface | Network |
|---|---|---|---|
| openai/symphony SPEC | Workspace dir; sandboxing is a non-goal; each implementation must document its posture | Codex sandbox values passed through | Left to the agent |
| Bernstein | Worktree by default | `--sandbox worktree\|docker\|podman\|e2b\|modal\|daytona\|blaxel\|runloop\|vercel\|microvm`; an explicit request never silently swaps runtime; `host_isolation_tier` declares an outer boundary | Per backend |
| OpenHands | Docker by default | `RUNTIME=docker\|process\|remote`; `process` means no isolation | Not documented |
| Cyrus | Worktree, unconfined; full `process.env` with `HOME` | `sandbox.enabled` (false); `networkPolicy` with a ~200-domain trusted preset including GitHub | Proxy covers Bash children only |
| Agent Orchestrator | Desktop: worktree. Cloud: container per session | Cloud provider `docker\|createos\|coder` | Not documented |
| claude-squad, Conductor, Vibe Kanban, Sculptor | Worktree on host | Agent flags passed through | None |
| Devin, Cursor cloud, Codex cloud, Copilot coding agent, Jules | A VM or container per session, vendor-hosted | Network allowlist presets, secrets policy, git access level | Allowlists; Copilot's firewall covers Bash children only |

### Lessons for crewd's option surface

1. **One key that selects a backend, named for the boundary.** Precedent: Bernstein's
   `--sandbox`, OpenHands' `RUNTIME`, Gemini's `GEMINI_SANDBOX`. Do not add an `enabled` bool
   beside it; Claude Code's three flags exist to patch fail-open. `off` as the default is
   mainstream; only Codex sandboxes by default.
2. **The network is an allowlist, not a boolean.**
   - A boolean works only where the model call is made outside the sandbox: Codex, Cursor.
   - A whole-process wrap needs the model API reachable, so every whole-process precedent uses
     a domain list with deny-wins.
   - Do not copy presets that allow GitHub: Cyrus's, or Codex cloud's "Common dependencies".
3. **Fail closed with no knob.**
   - Claude defaults to fail-open and Cursor falls back to prompts.
   - Bernstein documents a nested Codex bwrap that refuses every command while the run exits 0.
   - Only Gemini and Bernstein refuse a requested sandbox that cannot start.
4. **Wrap from outside, and turn the agent's own sandbox off.** Anthropic says to run
   `--dangerously-skip-permissions` sessions in a container, VM or `srt`. Codex's bypass flag is
   "intended solely for running in environments that are externally sandboxed".
5. **Reserved names fail loudly.** goose's deleted `GOOSE_SANDBOX` left users believing they
   were sandboxed.
6. **Git metadata is the opposite of Codex's default.** crewd needs the commondir writable for
   commits, while `hooks/` and `config` stay read-only, as `srt` and Codex protect them.

## Spike artefacts

The spike scripts and profiles were throwaway and are not committed. What a slice needs from
them is in its ADR consequence. These were the shapes:

- **Seatbelt profile:** `(allow default)`, then:
  - `(deny file-write*)`, followed by allows for the worktree, the commondir, the run's
    `TMPDIR` and `CARGO_HOME`, and `~/.claude/projects/<encoded worktree>`;
  - a later deny for `<commondir>/hooks` and `<commondir>/config`;
  - `(deny file-read*)` for the deployment dir, `~/.ssh` and `~/.config/gh`;
  - `(deny network-outbound)`, then `(allow network-outbound (remote ip "localhost:*"))`.
- **bwrap argv:**
  - `--unshare-user --unshare-ipc --unshare-pid --unshare-net --unshare-uts --die-with-parent`;
  - `--ro-bind / / --dev /dev --proc /proc --tmpfs /tmp`, plus a tmpfs over each denied
    directory;
  - `--bind` for the worktree, the commondir and the run dir, and `--ro-bind` for
    `<commondir>/hooks` and `<commondir>/config`;
  - inside, `socat TCP-LISTEN:127.0.0.1:<port>` to the bind-mounted proxy socket. crewd would
    do this bridge in-binary.

## Claims that stay unverified

- `sandbox-exec` spawn overhead (3.7–10 ms) and +9–10% on a read-heavy workload: measured once
  by a research agent and not repeated. The cargo and claude timings above are measured.
- `claude` reads the keychain by exec'ing `security`: observed locally, no primary source.
- `--settings '{"sandbox":{"enabled":false}}'` overrides a user-scope or managed
  `sandbox.enabled: true`: not tested.
- With `CLAUDE_CONFIG_DIR` set, the keychain item is keyed to a hash of the path, so a per-run
  config dir would break keychain login: third-party sources only.
- grok under any profile: grok was not installed on the spike machine. Its credential is a file,
  `~/.grok/auth.json`, so it needs no keychain.
- smolvm `--stream` with stdin, `--mount-socket` on `machine run`, and whether a SIGKILL of the
  CLI leaves no VM.
- Cursor's headless default and failure mode, and Copilot's failure mode.
- gVisor and Kata `cargo build` overhead.

## Sources

Primary sources the verdicts rest on:

- **Seatbelt:**
  - [Codex Seatbelt policy](https://github.com/openai/codex/blob/main/codex-rs/sandboxing/src/seatbelt_base_policy.sbpl)
  - [Codex network policy](https://github.com/openai/codex/blob/main/codex-rs/sandboxing/src/seatbelt_network_policy.sbpl)
  - [srt macOS utils](https://github.com/anthropic-experimental/sandbox-runtime/blob/main/src/sandbox/macos-sandbox-utils.ts)
  - [Gemini Seatbelt args](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/sandbox/macos/seatbeltArgsBuilder.ts)
  - [nono macOS](https://github.com/always-further/nono/blob/main/crates/nono/src/sandbox/macos.rs)
- **Linux:**
  - [bubblewrap](https://github.com/containers/bubblewrap)
  - [Codex linux-sandbox](https://github.com/openai/codex/blob/main/codex-rs/linux-sandbox/README.md)
  - [srt Linux utils](https://github.com/anthropic-experimental/sandbox-runtime/blob/main/src/sandbox/linux-sandbox-utils.ts)
  - [Landlock](https://docs.kernel.org/userspace-api/landlock.html)
  - [Ubuntu userns restriction](https://discourse.ubuntu.com/t/spec-unprivileged-user-namespace-restrictions-via-apparmor-in-ubuntu-23-10/37626)
- **Claude Code:**
  - [sandboxing](https://code.claude.com/docs/en/sandboxing)
  - [sandbox environments](https://code.claude.com/docs/en/sandbox-environments)
  - [network config](https://code.claude.com/docs/en/network-config)
  - [secure deployment](https://code.claude.com/docs/en/agent-sdk/secure-deployment)
  - [authentication](https://code.claude.com/docs/en/authentication)
- **Grok:** [enterprise network requirements](https://docs.x.ai/build/enterprise)
- **microVM:**
  - [smolvm](https://github.com/smol-machines/smolvm)
  - [smolvm credential substitution](https://github.com/smol-machines/smolvm/blob/main/docs/credential-substitution.md)
  - [microsandbox](https://github.com/superradcompany/microsandbox)
  - [apple/container](https://github.com/apple/container)
  - [Firecracker design](https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md)
  - [libkrun](https://github.com/libkrun/libkrun)
- **Container:**
  - [Docker Sandboxes policy](https://docs.docker.com/ai/sandboxes/security/policy/)
  - [Claude Code devcontainer firewall](https://github.com/anthropics/claude-code/blob/main/.devcontainer/init-firewall.sh)
  - [podman run](https://docs.podman.io/en/latest/markdown/podman-run.1.html)
- **Remote:**
  - [E2B](https://github.com/e2b-dev/E2B)
  - [E2B runtime](https://github.com/e2b-dev/runtime)
  - [Vercel Sandbox firewall](https://vercel.com/docs/sandbox/concepts/firewall.md)
  - [Daytona (archived)](https://github.com/daytonaio/daytona)
- **Kubernetes:**
  - [agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox)
  - [agent-sandbox roadmap](https://github.com/kubernetes-sigs/agent-sandbox/blob/main/roadmap.md)
  - [Cilium DNS policy](https://docs.cilium.io/en/stable/security/dns/)
  - [kube-rs](https://github.com/kube-rs/kube)
- **Competitors:**
  - [openai/symphony SPEC](https://github.com/openai/symphony/blob/main/SPEC.md)
  - [Bernstein sandbox](https://github.com/sipyourdrink-ltd/bernstein/blob/main/docs/architecture/sandbox.md)
  - [OpenHands runtimes](https://docs.openhands.dev/openhands/usage/runtimes/overview)
  - [Cyrus config](https://github.com/cyrusagents/cyrus/blob/main/docs/CONFIG_FILE.md)
  - [Cursor sandbox](https://cursor.com/docs/reference/sandbox)
  - [Copilot local sandbox](https://docs.github.com/copilot/concepts/about-cloud-and-local-sandboxes)
  - [goose #10900](https://github.com/aaif-goose/goose/pull/10900)
  - [Devin security profiles](https://docs.devin.ai/product-guides/security-profiles)
