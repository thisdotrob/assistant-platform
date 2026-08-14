# Assistant Platform

Shared Rust platform workspace underpinning the assistant products.

Product repos should depend on this platform as a coordinated versioned unit. Product-specific identity, profile wording, and default channel choices live in the product repos.

## TODO

- **SDK-native session resumption for conversation history.** Today each Claude turn
  (`shim/src/claude.js`) is a fresh, stateless Agent SDK `query()` — the SDK has no
  `messages`/history input, so prior turns are embedded into the prompt string as a
  transcript. A cleaner approach: capture the SDK session id returned by `query()`,
  persist it keyed by Slack thread (the container already owns per-thread SQLite state),
  and pass `options.resume: <sessionId>` on the next turn in that thread. Blocker to solve
  first: the SDK's session files live in `~/.claude/projects/` inside the container, which
  runs with `--rm` (ephemeral), so they must survive container respawns (e.g. bind-mount
  the projects dir into the session dir) or resume will fail after any reap. Deferred while
  the prompt-embedding approach works; revisit if prompt size or fidelity becomes a problem.

