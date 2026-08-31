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

## Auto-updater is frozen on this fork

The GitHub auto-updater is deliberately disabled. See `UPDATER_FROZEN` in
`crates/jcode-app-core/src/update.rs`: all update entry points error cleanly
instead of reaching GitHub, and updates happen only via the manual
build-review-swap runbook (`jcode self-dev --build`). The `JCODE_NO_AUTO_UPDATE`
kill switch is preserved but independent.

## Rate-limit / usage-cap error vocabulary (keep in sync)

The Anthropic 5-hour OAuth cap is an HTTP 429 `rate_limit_error` whose body text
varies ("usage limit reached", "quota exceeded", ...). Several matchers must stay
in sync on that vocabulary: `is_rate_limit_error` (the reactive account-switch
gate) and `is_fable_scoped_limit_error` in
`crates/jcode-provider-anthropic-runtime/src/lib.rs`, the TUI auto-poke matcher
in `crates/jcode-tui/src/tui/app/commands_auto_poke_errors.rs`, and
`error_looks_like_usage_limit` in
`crates/jcode-base/src/provider/account_failover.rs`. Reuse that shared
vocabulary; do not invent a new spelling list.

The 5h cap surfaces MID-STREAM (anthropic `complete()` has already returned
`Ok(stream)`), so it never reaches the `complete_on_provider`-Err path that
`try_same_provider_account_failover` watches. The anthropic runtime retry loop
therefore calls `jcode_base::provider::reactive_switch_on_rate_limit` directly to
switch to a sibling account with headroom (cache-only probe, per-provider
cooldown) and retry the same model. This coexists with the between-turns
`try_same_provider_account_failover` and the cross-provider countdown failover.

Two DIFFERENT TUI surfaces recover from a cross-provider outage; do not conflate
them (`crates/jcode-tui/src/tui/app/model_context.rs`). `PendingProviderFailover`
is the server-decided countdown, armed by `handle_provider_failover_prompt` only
when `parse_failover_prompt_message` matches a `[jcode-provider-failover]` marker.
`PendingFallbackOffer` is the reactive path for a plain terminal turn error (e.g.
a chatgpt-web 429), armed by `offer_fallback_after_error`. Both honor
`cross_provider_failover` and both now auto-take on a deadline for
countdown/remote sessions (`maybe_progress_*` run from `local.rs` + `remote.rs`;
remote drains `pending_route_selection` after firing); a plain 429 flows through
the offer path, not the marker path.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
