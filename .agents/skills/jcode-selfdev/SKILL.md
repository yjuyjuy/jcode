---
name: jcode-selfdev
description: "Develop jcode itself: build the agent from source, publish the build into the reloadable channel store, promote it onto the shared-server channel, reload the running daemon onto it, and confirm the live process is the new binary. Also covers inspecting a running daemon through the debug socket. Use whenever a task edits the jcode repository and the change has to show up in a running jcode."
user-invocable: false
---

# jcode-selfdev

Building, publishing, reloading, and debugging jcode itself.

`jcode` is both the tool you are running inside and the codebase you are editing. That is the whole
difficulty: a change is not real until the daemon that serves live sessions has exec'd into the new
binary, and nothing in `--help` tells you that compiling is only the first of four steps.

## Which checkout do I build from?

The build and run checkout is `/root/.jcode/source/jcode`. Its remotes are `origin` (upstream,
`1jehuang/jcode`) and `fork` (ours, `yjuyjuy/jcode`). The fleet helper `build-jcode` operates on this
directory and hard-resets it to `fork/master` unless you pass `--no-pull`.

Project clones under `projects/jcode` and disposable task worktrees are for *delivering* changes:
branch, commit, push, open the pull request there. You can compile inside a worktree to check your
own change, but publishing from a worktree writes a `-dirty-<fingerprint>` version into the shared
channel store, so a worktree build is for verification and is not how a change reaches the fleet.

## When to reach for what

- **Just checking that it compiles** in a worktree: `scripts/dev_cargo.sh build --profile selfdev`.
  Fastest loop, and it never touches any channel.
- **Making a build the fleet can pick up**: `build-jcode`. It compiles through the publish path,
  verifies the published hash, and repoints the `shared-server` channel. Never substitute a raw
  `cargo build` for this (see the trap below).
- **Applying a build to the running daemon**: `jcode server reload`. This is a graceful handoff that
  preserves live sessions. `jcode server stop --force` kills them, so use it only on a wedged daemon.
- **Inspecting a running daemon**: `jcode debug`. Read-only verbs (`server:info`, `sessions`,
  `state`, `client:frame`) are safe from inside a live agent session.
- **Changing a live session's model** from automation: `jcode session set-model`, never by typing
  `/model` into a composer (the autocomplete popup races and can silently no-op).
- **Profiles**: `selfdev` is the profile to use. `release-lto` and signoff builds cost many minutes
  and buy nothing for a behavior check.

## The trap: compiling is not publishing

`cargo build` writes `target/selfdev/jcode` and stops. It does not write
`~/.jcode/builds/current`, so `/reload` and `jcode server reload` see no newer candidate and report
"already running the newest binary; no reload needed" - and you conclude, wrongly, that your change
is live.

Observed directly:

```bash
$ ./scripts/dev_cargo.sh build --profile selfdev -p jcode --bin jcode
    Finished `selfdev` profile [unoptimized] target(s) in 2.54s
$ cat "$JCODE_HOME/builds/current-version"        # unchanged by the cargo build
96848bcbe-dirty-4a56c3050cbc
$ jcode server reload --json
  "reloaded": false,
  "detail": "jcode server is already running the newest binary; no reload needed."
```

Publishing is `jcode self-dev --build`, which compiles and then installs the binary into
`~/.jcode/builds/versions/<version>/` and repoints the `current` channel at it.

There is a second, separate gap after that. `self-dev --build` writes only `current`, while the
daemon resolves its reload target from the `shared-server` channel. So a publish alone still leaves
`server reload` with nothing strictly newer. `jcode server promote` moves `shared-server` onto an
installed version, and its own output says so: promotion and reload are deliberately separate
operations, because an update must never overwrite a build you promoted on purpose.

Full chain: **edit -> `self-dev --build` (compile + publish `current`) -> `server promote` (point
`shared-server` at it) -> `server reload` (daemon execs into it) -> verify the live process.**
`build-jcode` collapses the first three; `--reload` adds the fourth.

## Workflows

### 1. Compile-only check inside a task worktree

```bash
cd <your worktree>
./scripts/dev_cargo.sh build --profile selfdev -p jcode --bin jcode
./target/selfdev/jcode version
```

Use `scripts/dev_cargo.sh` rather than bare `cargo`: it carries this repo's linker, memory, and
toolchain policy and records per-action build timings. A cold worktree build is roughly 3.5 minutes
on this box, an incremental one about 3 seconds.

### 2. Exercise your build without disturbing the fleet

The binary you just compiled is inert until it is published, so do not test a change by running the
`jcode` on `PATH`. Run your own binary against its own socket:

```bash
./target/selfdev/jcode run --no-update \
  --socket "$JCODE_SCRATCH_DIR/jcode-mytest.sock" 'reply with exactly: SELFDEV_OK'
```

This is a real end-to-end turn through your code and it touches no shared state.

### 3. Full private rehearsal of the publish/promote/reload chain

To walk the whole chain without publishing over what the fleet is running, redirect both the channel
store and the runtime directory. `JCODE_HOME` moves `builds/`, and `JCODE_RUNTIME_DIR` moves the
socket and the single-daemon lock - without the second one a private `jcode serve` aborts with
"Another jcode server process is already running for runtime dir ...".

```bash
export JCODE_HOME="$JCODE_SCRATCH_DIR/selfdev-home"
export JCODE_RUNTIME_DIR="$JCODE_SCRATCH_DIR/selfdev-rt"
mkdir -p "$JCODE_HOME" "$JCODE_RUNTIME_DIR"
SOCK="$JCODE_RUNTIME_DIR/jcode.sock"

# publish once, then run a daemon of your own from that published version
./target/selfdev/jcode self-dev --build --no-update </dev/null
V=$(cat "$JCODE_HOME/builds/current-version")
setsid "$JCODE_HOME/builds/versions/$V/jcode" serve --socket "$SOCK" --no-update &

# edit, then compile + publish + promote + reload
./target/selfdev/jcode self-dev --build --no-update </dev/null
./target/selfdev/jcode server promote --json --no-update
./target/selfdev/jcode server reload --json --no-update --socket "$SOCK"

# verify the LIVE process, not the channel marker
readlink -f /proc/$(pgrep -f "serve --socket $SOCK" | head -1)/exe
```

Three details that decide whether this proves anything:

- Drive the chain with `./target/selfdev/jcode`, not the `jcode` on `PATH`. A jcode binary resolves
  the repository to build from its own compile-time manifest directory first, so the installed
  launcher rebuilds `/root/.jcode/source/jcode` and cheerfully publishes a version that does not
  contain your edit. (`JCODE_REPO_DIR` overrides that resolution if you need it.)

- `self-dev --build` publishes and then tries to open a session. Headless it exits non-zero on
  "requires an interactive terminal" *after* a successful publish, so its exit code is not the
  signal. Check `builds/current-version` instead, exactly as `build-jcode` does.
- Verify by resolving `/proc/<pid>/exe`, not by reading a version string. Start the daemon from a
  published `versions/<v>/jcode` path, because a daemon started from `target/selfdev/jcode` has its
  inode unlinked by the next build and then reports its own path as `... (deleted)`.

Confirmed transcript of the reload step:

```
publish1=96848bcbe-dirty-740fe30143ea
pid=2486607 exe=.../builds/versions/96848bcbe-dirty-740fe30143ea/jcode
publish2=96848bcbe-dirty-6481d3788d05
"detail": "shared-server channel <unset> -> 96848bcbe-dirty-6481d3788d05."
"reloaded": true, "already_current": false, "handoff_ready": true
pid_after=2486607 exe=.../builds/versions/96848bcbe-dirty-6481d3788d05/jcode
```

The process id does not change across a reload: the daemon execs in place and hands its live
sessions to the new image.

### 4. Ship a build to the fleet

```bash
build-jcode              # fetch fork/master, hard-reset, compile, publish, repoint shared-server
build-jcode --no-pull    # same, from the source tree exactly as it stands
build-jcode --reload     # also restart the shared server onto it
```

A plain `build-jcode` is the safe form: it makes the new binary ready and every session picks it up
on its own `/reload`. Build output goes to `/tmp/build-jcode.log`.

### 5. Inspect a running daemon

```bash
jcode debug list                 # which servers exist and whether the debug socket is enabled
jcode debug server:info          # id, version, uptime, session count, has_update
jcode debug sessions             # live sessions with full metadata
jcode session list               # the same sessions as provider/account/model/effort/context
jcode debug help                 # the whole verb surface, including the client: and swarm: families
```

`jcode debug` requires `[display] debug_socket = true` in `~/.jcode/config.toml`. For UI work,
`client:frame`, `client:layout`, and `client:anomalies` return what the TUI actually rendered, which
beats reasoning about layout code.

### 6. Instrumenting a code path

`crate::logging::info` writes to `~/.jcode/logs/jcode-<date>.log`, not to stderr, so it produces
nothing visible under `--trace`. Use `eprintln!` for throwaway diagnostics and delete it before
committing.

## Fleet conventions

- **Never restart the shared server while other agents are live.** `build-jcode --reload`,
  `jcode server stop`, and a promote-plus-reload against the fleet socket all touch every running
  session. Inside an agent session, stay on the private-socket rehearsal above.
- **Which binary am I looking at?** `~/.local/bin/jcode` is a launcher symlink and
  `~/.jcode/builds/shared-server/jcode` is a symlink into `versions/<v>/`. Running `strings` on
  either reads a 70-byte symlink, not a program. Resolve with `readlink -f` first.
- **`~/.local/bin` must precede `~/.cargo/bin` on `PATH`**, otherwise a stale cargo-installed jcode
  shadows the launcher.
- **`build-jcode` is a loose script in `~/.local/bin`, tracked in no repository.** Treat it as fleet
  infrastructure; a fix to it does not travel with a jcode pull request.
- **Deliver from a project clone or task worktree, on your own branch, with the pull request against
  `yjuyjuy/jcode`.** `origin` in the source checkout is upstream, which we do not own.
- Read the repository's own `AGENTS.md` before changing subsystems: it carries the invariants that
  are expensive to rediscover (context-limit refresh after an account switch, provider-unavailability
  mark scoping, session persistence intent).

## Non-goals

- Not a jcode user manual. Slash commands, providers, and model selection belong to using jcode, not
  to developing it.
- Not a flag reference. Every command here takes `--help`; this skill only covers the ordering and
  the conventions `--help` cannot know.
- Not a release process. Publishing to the `stable` channel and cutting versioned releases are
  separate, captain-owned operations.
- Not a substitute for reading the repository's `AGENTS.md`, which owns the subsystem-level
  invariants.
- No hard-retiring a daemon. `jcode server stop --force` is for a wedged server, and on a shared box
  that is a human's call.
