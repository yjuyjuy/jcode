# Compaction

jcode compacts the conversation when it approaches the provider context
window: older messages are summarized, the summary becomes the new history
prefix, and the model keeps working with recent messages + summary.

## Configuration

```toml
# ~/.jcode/config.toml
[compaction]
mode            = "reactive"       # reactive | proactive | semantic
# In-session action run synchronously BEFORE a proactive/soft-threshold
# compaction starts. Empty/unset: no action (default).
# pre_compact_action = "stow"
# Pause the turn loop until pre-action + compaction both complete before the
# next model call. Default: false (compaction continues in the background).
# blocking_compact  = false
```

Env overrides (always win; empty `JCODE_PRE_COMPACT_ACTION` disables a config
action):
`JCODE_PRE_COMPACT_ACTION`, `JCODE_BLOCKING_COMPACT`.

## Pre-compact action

A bare external lifecycle hook cannot run an in-session skill (a skill needs
the live model + session to sweep the conversation and write durable
knowledge). So the pre-compact action runs *inside* the session, synchronously,
before a proactive/soft-threshold compaction starts:

| Form | Meaning |
| --- | --- |
| `"stow"` or `"skill:stow"` | run the installed skill as a real sub-turn, exactly as if the user typed `/stow` (skill prompt loaded, `/stow` injected as the user message) |
| `"prompt:<text>"` | inject the text as a user message and process it as a turn |
| `"cmd:<command>"` | run the external command via the shell (receives `JCODE_HOOKS_DISABLED=1` and `JCODE_HOOK_EVENT=pre_compact`, like lifecycle hooks) |

A bare string that is not an installed skill name is injected as a prompt.
A failed or unresolvable action is logged and never blocks the compaction.

## Blocking compaction

Crossing the proactive/soft threshold pauses the turn loop: the pre-compact
action (if any) runs first, then the compaction runs to completion, and only
then does the next model call go out with the compacted context. The wait is
bounded (180s); on timeout the in-flight summary is applied by the regular
completion path. Default is the historical behavior (background compaction,
turn continues).

## Emergency boundary

The emergency hard-compact path (`ensure_context_fits` at the critical
threshold and context-limit recovery after a provider error) is never affected
by either knob. At or above the critical threshold the pre-compact flow does
nothing, so a context-limit emergency never blocks on a skill turn that itself
needs context.