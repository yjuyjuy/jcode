# Ticket: Bump spoofed Claude Code version to support new models (Opus 5.5)

## Problem

Requests to newer Anthropic models (e.g. Opus 5.5) fail with:

```
Anthropic API error (400 Bad Request): "Claude Code 2.1.257 does not support
this model; version 2.1.280 or newer is required. Run 'claude update' ..."
error_code: claude_code_version_too_old
```

Anthropic gates model access on the `claude-cli` version we impersonate over the
OAuth path. We hardcoded `2.1.257`, which is now below the `2.1.280` floor the
API enforces for the new model.

## Root cause

The impersonated Claude Code version is hardcoded in three places:

- `crates/jcode-base/src/provider/anthropic.rs`
  - `CLAUDE_CLI_USER_AGENT = "claude-cli/2.1.257 (external, sdk-cli)"`
- `crates/jcode-provider-anthropic-runtime/src/lib.rs`
  - OAuth preflight `app_version: "2.1.257"`
- `crates/jcode-provider-anthropic/src/lib.rs`
  - `OAUTH_BILLING_HEADER = "cc_version=2.1.257; cc_entrypoint=sdk-cli; cch=33f85;"`

## Fix

Bump all three to `2.1.280` (current latest on npm and exactly the enforced
floor). Keep them in lockstep so we never present a mismatched fleet of
version strings to the API.

## Follow-up (done)

The three literals are now unified into a single source of truth in
`crates/jcode-provider-core/src/anthropic.rs`:

- `CLAUDE_CODE_VERSION` const (the one line to bump).
- `claude_code_version()` honors a `JCODE_CLAUDE_CLI_VERSION` env override for
  emergency bumps without a rebuild.
- `claude_cli_user_agent()` and `claude_oauth_billing_header()` derive the user
  agent and billing header from it.

The three call sites (base user agent, runtime OAuth `app_version`, provider
billing header) now reference these helpers. The `cch=33f85;` cache hash may
still be version-coupled; verify against a captured official request if a future
bump alone is insufficient.
