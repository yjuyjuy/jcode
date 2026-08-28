# Repository Guidelines

## Development Workflow

- **Stay on your own branch** - Do not take, cherry-pick, merge, or copy code from other
  people's or other agents' branches unless the source branch belongs to a repository
  maintainer and the user explicitly asks you to integrate it. Only work from your branch
  and its base (e.g. `main`) otherwise. Never integrate branches owned by non-maintainers
  or other agents yourself; tell the user and let them decide how to proceed.

## Install Notes
- `~/.local/bin/jcode` is the launcher symlink used from `PATH`.
- `~/.jcode/builds/current/jcode` is the active local/source-build channel; self-dev builds and `scripts/install_release.sh` point the launcher here.
- `~/.jcode/builds/stable/jcode` is the stable release channel; `scripts/install.sh` installs this and points the launcher here.
- `~/.jcode/builds/versions/<version>/jcode` stores immutable binaries.
- `~/.jcode/builds/canary/jcode` still exists for canary/testing flows, but it is not the primary self-dev install path.
- On Windows, the equivalents are `%LOCALAPPDATA%\\jcode\\bin\\jcode.exe` for the launcher, `%LOCALAPPDATA%\\jcode\\builds\\stable\\jcode.exe` for stable, and `%LOCALAPPDATA%\\jcode\\builds\\versions\\<version>\\jcode.exe` for immutable installs; `scripts/install.ps1` currently installs the stable channel.
- Ensure `~/.local/bin` is **before** `~/.cargo/bin` in `PATH`.

## Verifying a change at runtime

`cargo build` alone proves nothing about behavior. `jcode run` and interactive
sessions are served by the long-lived daemon at
`~/.jcode/builds/shared-server/jcode`, which is a symlink into
`~/.jcode/builds/versions/<version>/`. Until that symlink is repointed and the
daemon restarted (`jcode self-dev --build`), a freshly built binary is inert and
every runtime check silently measures the old code.

To test a change without disturbing the shared daemon or the caller's session,
run your build against its own socket:

```bash
cargo build --profile selfdev
./target/selfdev/jcode run --no-update --socket /run/user/1000/jcode-mytest.sock '<prompt>'
```

Two things that waste time otherwise:

- `crate::logging::info` writes to a log file, not stderr, so instrumenting a
  code path with it produces no visible output under `--trace`. Use `eprintln!`
  for throwaway diagnostics and delete it before committing.
- Confirm which binary you are actually inspecting. `strings` on
  `builds/shared-server/jcode` reads a 70-byte symlink, not a program; resolve it
  with `readlink -f` first.

## Subscription usage cache

Usage fetching lives in `crates/jcode-base/src/usage/`.
Two layers back the Anthropic and OpenAI usage fetchers: an in-memory per-session cache (L1, `usage/cache.rs`) and a host-wide on-disk cache shared with the `quota-axi` tool (L2, `usage/shared_cache.rs`).
L2 is only read/written for the host-active account and only for successful fetches, so all error and 429 backoff stays owned by L1.
The shared file (`${XDG_CACHE_HOME:-~/.cache}/quota-axi/quotas.json`, `schemaVersion` 1) must byte-match `quota-axi`; its window-identity contract is authoritative, so verify against that tool before changing the on-disk shape.

## Session account-switch control surface

The daemon exposes a headless, subscription-free control surface for switching a
live session's account/model without terminal injection (ADR 0031). Requests are
`ListSessions`, `SwitchSessionAccount`, and `SwitchSessionAccountModel` in
`crates/jcode-protocol/src/wire.rs`, dispatched as lightweight control requests
(see `is_lightweight_control_request` in `crates/jcode-protocol/src/lib.rs`) and
handled in `crates/jcode-app-core/src/server/session_control.rs`. The CLI entry
points are `jcode session list` and `jcode session switch-account`
(`src/cli/commands.rs`); the socket client methods live in
`crates/jcode-app-core/src/server/client_api.rs`.

Key constraint: account selection is otherwise process-global (a static runtime
override plus the active label in `auth.json`), so it only affects new sessions.
Per-session live switching works via a per-instance account pin on the provider
runtime (`account_label`/`set_account_label` on the `Provider` trait, honored in
the Anthropic and OpenAI runtimes and forwarded by `MultiProvider`). A switch is
adopted on the session's next turn and never interrupts a running turn; when a
turn holds the agent lock, the switch is deferred and applied on drain. The
end-to-end smoke test is `scripts/asw_session_control_e2e.sh`.

After an account switch the TUI client must refresh the footer's context limit
through `App::refresh_context_limit_for_current_model`
(`crates/jcode-tui/src/tui/app/model_context.rs`), which re-resolves the live
session model. Never write `provider.context_window()` there: remote clients run
an inert provider whose window is the 200K default, and the follow-up catalog
event carries the same model, so the latch would stick (footers misreporting
1M-window models as 200K until restart). Same rule for the model-switch path:
`update_context_limit_for_model` is the only correct writer.

## Non-interactive model + reasoning-effort set

To change a running session's model (and optionally its reasoning effort) from
automation, use `jcode session set-model <model> [--effort <e>] [--session <sid>]`
(`run_session_set_model_command` in `src/cli/commands/session_control.rs`), not by
typing `/model` into the TUI composer. Typing a slash command races the
autocomplete popup and can silently no-op, which is what drifted fleet workers
onto the wrong model. The verb applies model and effort together and then
verifies by reading the session state back, exiting non-zero on any mismatch or
bad input.

It rides the debug socket, which is opt-in via `[display] debug_socket = true` in
`~/.jcode/config.toml`. The transport is the shared `send_debug_command` helper in
`src/cli/debug.rs`. Server side, the effort-aware `set_model:` verb accepts a JSON
payload `{"model":..,"effort":..}` (a JSON payload because model specs contain `:`
and `@`, so no delimiter is safe) and calls `Agent::set_model_and_effort`
(`crates/jcode-app-core/src/agent/provider.rs`), which sets model first then effort
(Anthropic re-clamps stored effort for the new model) and rolls the model back if
the effort is rejected. The debug `state` verb reports `effort` alongside `model`
for readback (`crates/jcode-app-core/src/server/debug_command_exec.rs`).

## Pre-compact flow

`[compaction] pre_compact_action` and `blocking_compact` (both opt-in) live in
`crates/jcode-app-core/src/agent/compaction.rs`
(`run_pre_compact_flow_if_due`, called from both turn loops); the knobs are
snapshotted from the global config when the `CompactionManager` is constructed
in `crates/jcode-base/src/compaction.rs`. The emergency hard-compact path
(critical threshold, context-limit recovery) deliberately ignores both knobs.
Forms and behavior are documented in `docs/COMPACTION.md` and the config
template in `crates/jcode-base/src/config/default_file.rs`.
## Provider-unavailability mark store scoping

Transient provider-unavailability marks live in `ACCOUNT_RUNTIME_UNAVAILABLE_PROVIDERS`
(`crates/jcode-base/src/provider/models.rs`), keyed `{provider}::{account_label}`
(for Claude/OpenAI) or `{provider}::global`. Readers resolve the key against the
CURRENT active account, so marks are per-account by construction. Invariant: the
reactive 429 switch (`reactive_switch_on_rate_limit` in `provider/account_failover.rs`)
must record its mark for the exhausted account label via
`record_provider_unavailable_for_account_label`, never the implicit current-scope
variant - the record predates the override swap, so a reordering would poison the
healthy sibling and force a cross-provider prompt. The failover pass
(`complete_with_failover`, `provider/mod.rs`) tries sibling accounts before any
cross-provider proposal; regression tests live in
`provider/tests/reactive_switch_stays_on_claude.rs`.
## Session persistence intent

Upstream skips persisting sessions whose only content is the hidden session-context
message ("untouched sessions"). Daemon-prepared sessions that must be durable before
any user message exists opt out with `Session::mark_persist_intent()` (set in
`Agent::build_base`, visible-spawn preparation, and the selfdev launch path); raw
panel scaffolding never marks it. Test fixtures that assert on-disk state must mark
the intent too, or `Session::load` fails with ENOENT.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
