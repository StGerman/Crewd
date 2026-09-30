# Coding guidelines

For: the coding agent that edits `src/` or `tests/` and needs the PR to pass CI and review on the
first round. No human reads this code; agents write it, agents review it, and the daemon
dispatches more of them.

This file is authoritative for how code in this repository is written. [CLAUDE.md](../CLAUDE.md)
covers what the system is, how to run it, and which invariants it protects. Read that file for
architecture. Read this one before the first edit.

## How to read a rule

Each rule has four parts: the rule, one sentence of why, a place in the tree that shows it done
right, and a **Check** line that names what enforces it. **MUST** is a merge blocker. **SHOULD**
needs a reason in the PR description when it is skipped.

Enforcement is deterministic wherever a tool can do it. A rule with a lint or a CI step behind it
cannot be skipped or drifted from; a rule that only review can check is marked
`Check: review`, and the reviewing agent verifies it against this file. `Check: review
(crew-reviewer)` is that check for the Failure paths section, verified against the diff before
the agent reports done. Repetitive work that a lint demands is acceptable here; a rule that
depends on judgment is the thing to avoid.

Rules describe the target state. A rule that the code does not yet meet carries
`Not yet enforced: #NN`. The linked issue tracks the fix, and new code follows the rule from now.
Remove the tag when the issue closes. Every such tag has an issue; a rule without one is a bug in
this file.

## Error handling and panics

- **MUST** use `anyhow` only at the binary boundary and `thiserror` enums inside library
  modules. Why: a typed error lets the scheduler classify a failure; an `anyhow` error can only
  be logged. Example: `ErrorClass::retryable()` in [src/model.rs](../src/model.rs) decides
  between retry and quarantine, and a new error kind must be classifiable there.
  Check: review.
- **MUST NOT** call `unwrap()` or `expect()` on a production path, with one exception. An
  `expect("...")` whose message names a local invariant that the surrounding code just
  established is allowed. Why: a panic in the scheduler ends every run in flight. Example:
  `.expect("piped at spawn")` in [src/worker/claude.rs](../src/worker/claude.rs).
  Check: `clippy::unwrap_used` and `clippy::expect_used` at `deny` in `[lints.clippy]`, with
  `#[allow]` on the justified `expect` sites and on test modules. `Not yet enforced: #52`.
- **MUST NOT** call `lock().unwrap()`. Why: a panic while the lock is held poisons a
  `std::sync::Mutex`, and every later call on that lock panics for the rest of the process.
  Use `parking_lot`, whose locks do not poison. Check: `clippy::disallowed_types` on
  `std::sync::Mutex`, `std::sync::RwLock` and `std::sync::Condvar` in `clippy.toml`.
  `Not yet enforced: #49`.
- **MUST** handle an error once. Either log it and continue, or propagate it with `?`. Never
  both. Why: a double-handled error appears twice in the log and once in the caller, and the
  reader cannot tell how many failures happened. Example: `log_tracker_failure` in
  [src/sched/mod.rs](../src/sched/mod.rs) logs and skips the tick; nothing above it logs again.
  Check: review.
- **MAY** write `let _ = ...` on a best-effort seam, only with a comment that names why the
  result does not matter. Why: the projector, the transcript and the snapshot channel are
  designed to degrade rather than fail, and the comment is what distinguishes a decision from
  an oversight. Example: the `snap_tx.send` sites in [src/main.rs](../src/main.rs).
  Check: `clippy::let_underscore_must_use` at `deny`, so every site needs an explicit
  `#[allow]` with the reason beside it. `Not yet enforced: #52`.
- **MUST NOT** use `panic!`, `unreachable!` or `todo!` in production code, except an
  `unreachable!` guarded by a validation step in the same function. Why: the guard is what
  makes the branch unreachable, and the message must name that guard. Example:
  `unreachable!("validate rejects unknown tools")` in [src/broker/mod.rs](../src/broker/mod.rs).
  Check: `clippy::panic`, `clippy::todo` and `clippy::unimplemented` at `deny`;
  `clippy::unreachable` at `warn` with `#[allow]` on the guarded site. `Not yet enforced: #52`.

## Testing

- **MUST** name a test as a sentence that asserts the invariant, not `test_foo`. Why: if the
  name cannot say what the test defends, the test probably defends nothing. Example:
  `a_claim_stranded_by_a_hard_kill_is_recovered_at_the_next_startup` in
  [tests/scheduler.rs](../tests/scheduler.rs). Check: review. A CI grep for `fn test_` is
  possible and is part of #53.
- **MUST** add a trait and a fake for every new external effect, in the same commit as the
  effect. Why: the scheduler tests drive real `Scheduler::tick()` calls with no network, no
  disk and no clock, and a seam without a fake is a path those tests cannot reach. Example:
  `Clock` and `FakeClock` in [src/clock.rs](../src/clock.rs). Check: review.
- **MUST** fix a divergence between `FakeForge` and the real forge in the fake, and add the
  scenario that shows it as a case in [src/forge/contract.rs](../src/forge/contract.rs), which
  runs every case against both. Why: every delivery test in `tests/scheduler.rs` runs against
  the fake, so a fake that answers differently passes code that fails against GitHub, as the
  per-branch head on #174 did (#190). Check: review; a case is never skipped for one backend.
- **MUST** move time through `FakeClock` in scheduler and API tests. `std::thread::sleep` and
  `Instant::now` are allowed only in the tests that drive a real child process or a real
  socket, in [src/worker/claude.rs](../src/worker/claude.rs) and
  [src/broker/server.rs](../src/broker/server.rs). Why: a test that waits on the wall clock is
  a test that flakes on a slow runner. Check: `clippy::disallowed_methods` on
  `std::thread::sleep`, `std::time::Instant::now` and `std::time::SystemTime::now` in
  `clippy.toml`, with `#[allow]` in the two named files and in `src/clock.rs`.
  `Not yet enforced: #52`.
- **MUST** pair every row in the invariant table (`docs/invariants.md`) with a named guard test. A change
  that weakens a mechanism must first make its guard test fail. Why: several guard tests fail
  only in the exact scenario they were written for, so a green run is not proof that the test
  still means anything. Check: a CI step that extracts the test names from the table and runs
  `cargo test <name>` for each; a missing test fails the step. `Not yet enforced: #53`.
- **MUST** assert rendered text with an `insta` snapshot, not with `contains()`. Why: a
  substring assertion couples the test to wording, and a snapshot makes a wording change a
  reviewed diff instead of a broken build. Check: review, and `cargo insta test --check` in CI
  once snapshots exist. `Not yet enforced: #56`.
- **MUST** build test fixtures through one shared builder per type. Why: the same `Issue`,
  `Config` and `Harness` literals repeated across files drift apart the first time a field is
  added. Check: review. `Not yet enforced: #54`.
- **MUST NOT** run an example from any test or CI step. Why: `examples/broker_live.rs` spawns
  a real `claude` process and spends tokens. Cargo's `test = false` default for examples is the
  only thing that keeps `cargo test` from calling it. Check: a CI grep that fails on
  `cargo run --example` anywhere under `.github/`. `Not yet enforced: #53`.

## Comments and docs

- **MUST NOT** write a comment that says what the code already says. Before you write one,
  delete the code in your head and read the comment alone: if a reader could regenerate the
  comment from the code, the comment is noise and does not go in. A comment exists only for
  what the code cannot carry: the failure mode it avoids, the alternative that was rejected, the
  external fact it relies on, the invariant that another file depends on. Why: a comment that
  mirrors the code is wrong the moment the code changes and teaches an agent to keep writing
  more of them. Example: `// increment the counter` above `n += 1` is banned; the comment on
  `EXP_CAP` in [src/sched/retry.rs](../src/sched/retry.rs), which names the overflow it
  prevents, is the model. Check: review. The reviewing agent deletes any comment that fails the
  test and does not ask first.
- **MUST** re-read every comment attached to code you change, and delete any that fails the
  rule above. The check is not optional and not deferred: a comment above, beside or inside the
  function you edit is part of the edit. If the comment still holds, leave it; if the code
  moved and the comment now describes the old code, rewrite it or delete it; if it only
  restates the code, delete it. Do not ask, do not leave a `TODO`, do not keep it "for
  context". Why: a comment is never checked by the compiler, so an edit is the only moment
  anyone looks at it, and a stale comment left behind is the next agent's wrong assumption.
  Check: review. The reviewing agent treats a stale or restating comment inside the diff's
  context lines as a defect of the PR, not of the original author.
- **MUST** state the failure mode a comment guards against, in the first sentence. Why: the
  next agent to touch the line needs to know what breaks if it is removed, not how it works.
  Example: the module doc of [src/transcript.rs](../src/transcript.rs) opens each load-bearing
  choice with the defect it prevents. Check: review.
- **MUST** open every module with a `//!` doc that states what the module is for and which
  decision it records. Why: the module doc is the one place a reader looks before the code.
  Example: [src/broker/server.rs](../src/broker/server.rs), which records why the transport is
  hand-rolled. Check: `missing_docs = "deny"` in `[lints.rust]` covers public items; a CI grep
  for a first line that is not `//!` covers the module head. `Not yet enforced: #52`.
- **MUST** link incident history rather than retell it. A comment cites the issue number or
  the ADR, in one line. Why: the same story told in a comment, in a module doc and in CLAUDE.md
  goes out of sync in three places. Check: review. `Not yet enforced: #63`.
- **MUST** match the comment density of the surrounding code. This codebase comments decisions,
  not lines. Why: a block of narrative in a file of terse code is a sign the narrative belongs
  in a doc. Check: review.
- **MUST NOT** state how many members a growing set has (routes, tools, tests, packages,
  commands, invariant rows) in a doc or a comment. Name the members, or say "every" or "the".
  Why: every addition rewrites the sentence, so two pull requests in flight conflict on it
  (CLAUDE.md's test count did so on #102, #103 and #104), and one addition that misses it
  leaves a number that is silently wrong. A fixed fact (the MCP handshake's four methods, a
  historical "the six rows from #47") is not a growing set. Check: review; the diff of a PR
  that adds a route, tool or test should not touch prose elsewhere to renumber it.
- **MUST** run `cargo fmt` rather than hand-wrapping. The settings are `max_width = 100` and
  `use_small_heuristics = "Max"` in [rustfmt.toml](../rustfmt.toml). Why: `rustfmt` makes
  different choices than you will. Check: `cargo fmt --check` in CI. Enforced.

## Dependencies

- **MUST** prefer a maintained crate over hand-rolled infrastructure once the hand-rolled
  version passes about fifty lines, unless a module doc records why not. Why: a parser, a
  migration runner or a signal wrapper that the crate ecosystem already provides is code this
  repository then has to test and maintain alone. Check: review.
- **MUST** add a dependency only from the approved list below, or add it to the list in the
  same PR with one paragraph: what it replaces, why the alternatives do not fit, and what it
  adds to the tree. Why: the tree is small on purpose, and every addition is a permanent
  maintenance cost. Check: the approved list is the `[bans].allow` list in `deny.toml`, so a
  crate that is not in it fails `cargo deny check`. `Not yet enforced: #53`.
- **MUST** prefer a crate for platform-specific plumbing over a direct syscall or a device
  file. Why: a `/dev/urandom` read or a raw `libc::kill` works on the developer machine and
  fails silently elsewhere. Check: `unsafe_code = "deny"` covers the syscall half;
  `clippy::disallowed_methods` on `std::fs::File::open` with a `/dev/` literal is not
  expressible, so the device-file half is review (#51 removed the last `/dev/urandom` read).
- **MUST** keep `Cargo.lock` committed and build with `--locked` in CI. Why: a drifted lock
  file must fail the build rather than be rewritten quietly. Check: `--locked` on every cargo
  step in [.github/workflows/ci.yml](../.github/workflows/ci.yml). Enforced.

### Current dependencies

| Crate | Role in this repository | Note |
| :---- | :---- | :---- |
| `anyhow` | Error context at the binary boundary: `main.rs`, `tui`, `api`, `sched`, `project` | Never inside a domain trait |
| `base64` | URL-safe encoding of the GitHub App JWT, standard encoding of Jira's Basic auth header (#99) | Already in the tree under `ureq` |
| `blake3` | Collision-proof suffix for `worktree_key`, derivation of `session_id` | Deterministic on purpose, see `src/model.rs` |
| `clap` | Command line, derive style | |
| `crossterm` | Terminal backend for the TUI | |
| `insta` (dev) | Snapshot tests for rendered text, first used for the worker's prompts | Rolls out to the rest with #56 |
| `nix` (`signal` only) | Signals to a process group in `src/worker/claude.rs` and `src/gate/git.rs`, without `unsafe` | Replaced `libc` (#50) |
| `parking_lot` | The GitHub App's token cache in `src/credentials.rs` | Rolls out to the rest with #49 |
| `ratatui` | The dashboard | |
| `ring` | RS256 signature on the GitHub App JWT (#64), the `crewd init` state nonce (#65), and the broker's per-run bearer token (#51) | Already in the tree under `rustls`; `jsonwebtoken` would add a second RSA stack |
| `rusqlite` (bundled) | The store | Bundled so no system SQLite is needed |
| `rustls` (dev) | The TLS listener the CA-bundle test (#150) serves a real handshake from | Already in the tree under `ureq`; `default-features = false` with only the `ring` and `std` features, never `aws-lc-rs`, which needs a C toolchain |
| `rustls-pki-types` | PEM parsing of the GitHub App private key | Already in the tree under `rustls` |
| `serde`, `serde_json` | Config, `stream-json`, MCP framing, GitHub payloads | |
| `thiserror` | Typed errors in library modules | |
| `time` | RFC 3339 parsing of GitHub timestamps, and Jira's colonless-offset timestamps (e.g. `+0300`, #99) | The only date parsing in the crate |
| `tokio` | Ops API listener, `watch`/`mpsc`/`oneshot` channels, the main loop, signals | Not used by the domain traits |
| `toml` | Config file | |
| `tracing`, `tracing-subscriber` | Logs, always to stderr | stdout belongs to the TUI |
| `ureq` | Blocking HTTP client over `rustls` | Chosen for the blocking API and for needing no C toolchain |

`ring`, `rustls-pki-types` and `base64` arrived with `crewd init` (#65), which signs one JWT with
the key GitHub hands back to read the new App as itself. The first two were already compiled into
the tree by `ureq`'s TLS, so they add nothing to the build; `jsonwebtoken` was the alternative, and
its crypto backends would add a second RSA implementation for one signature. #64 adds the same
three for the same reason, and whichever lands second keeps one JWT signer.

### Approved to add

Pre-approved by the quality plan. Adding one needs no justification paragraph, only the
linked issue.

| Crate | Purpose | Issue |
| :---- | :---- | :---- |
| `parking_lot` | Non-poisoning locks, removes every `lock().unwrap()` | #49 |
| `tempfile` | Temp directories in tests with automatic cleanup | #54 |
| `rstest` | Shared fixtures for the test harness | #54 |
| `tiny_http` | One HTTP/1.1 server for both listeners | #57 or its follow-ups |
| `bon` | Builders for signatures over the parameter limit | #60 |
| `serde_rusqlite` | Derived row mapping in the store | #61 |
| `strum` | Derived enum labels | #62 |

### Needs an ADR first

`axum`, `hyper`, `rmcp`, `reqwest`, `octocrab`, `sqlx`. Each one changes the sync-versus-async
model or doubles the dependency tree. The decision is #57, and the module docs in
[src/broker/server.rs](../src/broker/server.rs) and [src/api/mod.rs](../src/api/mod.rs) record
why they were rejected so far.

## Concurrency

- **MUST** keep the domain traits blocking: `Tracker`, `TrackerWrites`, `Store`, `Worker`,
  `Workspace` and `Scheduler::tick`. Why: the scheduler tests drive `tick()` directly, and an
  async scheduler would need a runtime in every test. Check: review, until #57 decides the
  model.
- **MUST** confine `tokio` to `src/main.rs` and `src/api/`. Why: the runtime exists for the ops
  surface and the main loop. A domain module that imports it has crossed the boundary.
  Check: a CI grep for `tokio::` outside those two paths. `Not yet enforced: #53`.
- **MUST NOT** call a blocking operation on a runtime thread. SQLite, `ureq` and `git` go
  through `spawn_blocking` or stay off the runtime. Why: one blocked worker thread delays every
  ops API response that shares the runtime. Check: review. `Not yet enforced: #55`.
- **MUST** confine `std::thread::spawn` to the two places that drive a blocking resource:
  the reader thread in [src/worker/claude.rs](../src/worker/claude.rs) and the listener in
  [src/broker/server.rs](../src/broker/server.rs). Why: a thread that nothing joins is a
  process that does not exit. Check: `clippy::disallowed_methods` on `std::thread::spawn`
  with `#[allow]` in the two named files. `Not yet enforced: #52`.
- **MUST NOT** hold a lock across an `.await`. Why: the runtime cannot preempt the holder, and
  every other task that wants the lock stalls. Check: `clippy::await_holding_lock` at `deny`.
  It is in `clippy::all`, so this is enforced today.
- **MUST** pass state between the scheduler and its observers through channels, never through
  shared mutable state. Why: `watch` for the snapshot and `oneshot` for a reply are what keep
  a slow client off the tick. Example: `Command` handling in [src/api/mod.rs](../src/api/mod.rs).
  Check: review.
- **MUST** read the clock only through the injected `Clock`. Nothing outside
  [src/clock.rs](../src/clock.rs) calls `Instant::now` or `SystemTime::now`. Why: time that
  only moves when a test moves it is what makes the suite deterministic. Check: the same
  `clippy::disallowed_methods` entry as the testing rule above. `Not yet enforced: #52`.
- **Open decision.** The crate runs a blocking domain under a `tokio` ops surface. Whether to
  go all-async or drop `tokio` is #57. Until that ADR lands, the rules above describe the
  model as it is.

## Structure

- **MUST** keep a function to 5 parameters or fewer. `&self` does not count. Why: past five,
  call sites become positional puzzles, and a builder or a parameter struct names each value.
  Check: `too-many-arguments-threshold = 5` in `clippy.toml`; `clippy::too_many_arguments` is
  in `clippy::all`. `Not yet enforced: #60`.
- **MUST** keep a function to 100 lines or fewer. Why: a function that needs a scroll is a
  function whose invariants no reader holds in their head at once. Check:
  `clippy::too_many_lines` at `deny` in `[lints.clippy]` with `too-many-lines-threshold = 100`
  in `clippy.toml`. Test functions may `#[allow]` it. `Not yet enforced: #60`.
- **MUST** give each concern its own file and its own `impl` block. A new `Scheduler` concern
  goes into a submodule under `src/sched/` with its own `impl Scheduler`, not into the existing
  block. Why: multiple `impl` blocks keep `self` in reach without threading the whole
  scheduler through a free function. Check: review.
- **MUST** make observers share a formatter, never each other. `fmt_count`, `fmt_ms` and
  `Phase::label` belong in a shared module, and `api/render.rs` must not import from `tui`.
  Why: the TUI, the ops API and the status client are peers over one `Snapshot`. Check: a CI
  grep for `crate::tui` outside `src/tui/` and `src/main.rs`. `Not yet enforced: #56`.
- **MAY** use let-chains (`if x && let Some(y) = z`). The crate is Rust edition 2024. Why: they
  remove a level of nesting and are already used throughout. Check: the compiler.

## Lints and commit gate

- **MUST** run, before every commit, in this order:
  1. `cargo fmt`
  2. `cargo clippy --fix --all-targets --allow-dirty`
  3. `cargo clippy --all-targets -- -D warnings`
  4. `cargo test`
  Why: CI runs steps 1, 3 and 4 as checks, and a commit that fails any of them fails the PR.
  Step 2 is local only, because CI cannot commit what `--fix` changes. Check: CI. Enforced.
- **MUST** treat every warning as an error. `clippy::all` plus the individual lints named in
  this file, at the thresholds named in this file. No `pedantic` group as a whole. Why: a
  warning that is allowed to stay becomes twenty. Check: `-D warnings` in CI. Enforced for
  `clippy::all`; the named lints arrive with #52.
- **MUST** declare the lint set in the `[lints]` table of `Cargo.toml` and the thresholds in
  `clippy.toml`, so that a local `cargo clippy` and rust-analyzer report the same errors CI
  does. Why: a bar that lives only in a CI command line is invisible in the editor.
  `Not yet enforced: #52`.
- **MUST** keep the crate free of `unsafe`, with `unsafe_code = "deny"` in
  `[workspace.lints.rust]`. Why: `nix` signals a process group safely, so nothing here needs
  `unsafe`. `deny` rather than `forbid` because the one exception, `std::env::set_var` in
  `no_tracker_credential_reaches_the_child_environment`, is `unsafe` in Rust 2024 and carries a
  local `#[allow(unsafe_code)]` with a `// SAFETY:` line; `forbid` cannot be lifted locally.
  Check: the compiler. Enforced.
- **MUST** run `cargo deny check` and `cargo machete` in CI, run the tests through
  `cargo nextest` for per-test timeouts, and run the grep checks this file names. Why: a
  RUSTSEC advisory, an unused dependency or a stray `tokio::` import has no other signal.
  Check: CI. `Not yet enforced: #53`.
- **MUST** keep the toolchain pinned in [rust-toolchain.toml](../rust-toolchain.toml). Why:
  "it compiles" must mean the same thing on CI, on the developer machine and in every
  dispatched worktree. Check: CI resolves the pin on every run. Enforced.

## Visibility

- **MUST** default to `pub(crate)`. An item is `pub` only when `tests/` or `examples/` reaches
  it. Why: the library exists to back one binary and one test suite, and a public item is a
  promise to a consumer that does not exist. Check: `unreachable_pub = "deny"` in
  `[lints.rust]`. `Not yet enforced: #58`.
- **MUST** give an observer a `Snapshot` and never a `Store`. Why: rule 3 in CLAUDE.md is
  enforced by the type when the observer's struct cannot hold a `Store`. Example: `Api` in
  [src/api/mod.rs](../src/api/mod.rs) holds a `watch::Receiver` and a `Command` sender.
  Check: a CI grep for `Store` under `src/api/` and `src/tui/`. `Not yet enforced: #53`.
- **MUST** add a field to `Snapshot` when an observer needs data it does not carry, rather
  than giving the observer a new source. Why: `Row.branch` was added for the status client
  instead of letting the client ask `git`, and the same choice applies next time.
  Check: the same grep as above, plus review.

## Failure paths

The classes review keeps finding after the commit gate has passed. CLAUDE.md says when the
diff is checked against this section and against [invariants.md](invariants.md). Each rule
cites the pull request whose review found the class.

- **MUST** make the writes that establish one fact durable together. Why: a kill between them
  leaves the first applied and the second gone, and the next start treats that as the truth
  ([#168](https://github.com/StGerman/crewd/pull/168)). Example: `apply` in
  [src/store/schema.rs](../src/store/schema.rs), which commits a migration and its
  `user_version` bump in one transaction. Check: review (crew-reviewer).
- **MUST** reap a child before publishing its outcome. Why: a published outcome is the signal
  that the run is over and its workspace is free, and the process can still be running after
  its last line ([#166](https://github.com/StGerman/crewd/pull/166)). Example: the `terminal`
  outcome held until `wait` in `run_reader`, in [src/worker/claude.rs](../src/worker/claude.rs)
  and [src/worker/grok.rs](../src/worker/grok.rs). Check: review (crew-reviewer).
- **MUST** count a truncation marker toward the byte cap it marks. Why: a marker added on top
  of the cap makes the result longer than the bound it claims
  ([#175](https://github.com/StGerman/crewd/pull/175)). Example: `tail` in
  [src/sched/workers.rs](../src/sched/workers.rs). Check: review (crew-reviewer).
- **MUST** return from a wait when the run has been stopped. Why: a wait with no stop check
  keeps working after the scheduler has treated the run as stopped
  ([#161](https://github.com/StGerman/crewd/pull/161),
  [#123](https://github.com/StGerman/crewd/pull/123)). Example: `GitGate::hold_fetch_lock` in
  [src/gate/git.rs](../src/gate/git.rs). Check: review (crew-reviewer).
- **MUST** check for uncommitted tracked changes and for a rebase or merge in progress before
  rebasing, merging, or removing a worktree. Why: an operation that looks only at `HEAD` drops
  that work or misreads the branch ([#123](https://github.com/StGerman/crewd/pull/123)).
  Example: `uncommitted`, `rebase_in_progress` and `merge_in_progress` on `GitGate` in
  [src/gate/git.rs](../src/gate/git.rs). Check: review (crew-reviewer).
- **MUST** fail the whole call when a page errors or the response ends before the protocol
  marks it complete, rather than returning the pages already gathered. Why: that prefix is
  indistinguishable from the whole result ([#185](https://github.com/StGerman/crewd/pull/185)).
  Example: `fetch_open_labelled` in [src/tracker/github.rs](../src/tracker/github.rs) and
  `paginate` in [src/forge/github.rs](../src/forge/github.rs), which propagate a page error
  instead of a short list. Check: review (crew-reviewer).
- **MUST** make a git read that needs the repository's credential use the credential the push
  uses. Why: a host whose only access is that credential cannot read through another remote
  ([#161](https://github.com/StGerman/crewd/pull/161),
  [#174](https://github.com/StGerman/crewd/pull/174)). Example: `GitGate::fetch_with_credential`
  in [src/gate/git.rs](../src/gate/git.rs). Check: review (crew-reviewer).
- **MUST** make a fake answer what its real adapter answers, on every case both are run
  against. Why: a test that only drives the fake passes code that fails against the real
  backend ([#174](https://github.com/StGerman/crewd/pull/174)). Example: a case in
  [src/forge/contract.rs](../src/forge/contract.rs) runs against `FakeForge` and `GithubForge`,
  and a divergence is fixed in the fake. Check: review (crew-reviewer).
- **MUST** change a stated guarantee everywhere it is stated, in the same change. Why: a
  sentence rewritten in one place and left in the others promises two different things
  ([#129](https://github.com/StGerman/crewd/pull/129)). Example: the invariant table lives in
  [invariants.md](invariants.md), and CLAUDE.md points at that file. Check: review
  (crew-reviewer).
