# claw-channel-slack Contract

## Public API
Slack `ChannelAdapter`. The outbound half is implemented: Web API auth
lifecycle (`start` runs `auth.test` and captures the bot identity), threaded
`chat.postMessage` delivery rendered through Slack mrkdwn with card/question
fallback, sender-identity resolution, and health. The Web API surface is
injected as a `SlackApi` trait (real `CurlSlackApi` shells `curl`; tests use a
fake) so the adapter is covered offline. The inbound half is a Socket Mode
listener (`run_listener`) over an injected `SocketOpener`/`SocketConn` transport
seam (real `TungsteniteOpener` gated behind the non-default `socket-mode`
feature; tests script a fake connection): it pumps frames, acks them, normalizes
each to a `RoutingEvent` for a caller-supplied sink, and reconnects on a dropped
connection. The listener also takes a `tick` hook run between frames — when a
read returns the non-error `SocketError::Idle` yield (no frame within the
transport's read window), the loop runs `tick` and keeps reading, giving the
caller a single-threaded slot for periodic work (the host's scheduler tick)
without a side thread. File send and reactions are not yet implemented.

## Persistence Ownership
Owns Slack channel extension and readiness-state tables via central DB migrations, keyed to core IDs; does not mutate core or router base tables.

## Config
Reads Slack app/bot tokens and workspace settings from claw-config (secrets via
env overlay). The bot token is presented to Slack only through curl's stdin
config, never on the process argument list, in a log, or on disk.

## Events
Normalizes Slack event payloads into the router's neutral inbound `RoutingEvent`
(messages, mentions, threads, edits/deletes, reactions, self-author filtering).
The `socket-mode`-gated transport receives those events live over a websocket;
the real `TungsteniteConn` sets a short read timeout on the underlying socket and
maps a `WouldBlock`/`TimedOut` read to `SocketError::Idle` (a retryable yield, not
a fault) so the listener can interleave periodic work between frames.

## CLI/Web Surfaces
None directly; setup steps and readiness are surfaced through claw-setup and claw-web.

## Prompt Fragments
Owns Slack-specific rendering/format fragments declared as a capability; none are platform-shared in the first release.

## Readiness Checks
Verifies Socket Mode connectivity, token validity, and required scopes/channels are accessible.

## Conformance Tests
Inbound normalization maps messages, mentions, threads, edits/deletes, and
reactions to the correct routing/dedupe keys, and flags the bot's own messages.
The outbound adapter (over a fake `SlackApi`) authenticates and connects on
`start`, refuses delivery before start, renders text/card/question to escaped
mrkdwn and forwards the channel + thread target, and maps auth/API failures to
`Setup`/`Delivery` channel errors; cards degrade to a readable mrkdwn layout.
Rendering translates the model's GitHub-flavored Markdown into Slack mrkdwn
(`**bold**`→`*bold*`, `# heading`→`*heading*`, `- ` bullets→`• `,
`[text](url)`→`<url|text>`, `~~strike~~`→`~strike~`) while leaving inline/fenced
code and bare `*`/`_` emphasis verbatim so identifiers and arithmetic survive.
A thematic break (`---`/`***`/`___`) is dropped, since Slack mrkdwn has no
horizontal rule and would otherwise show the literal characters. The
Socket Mode listener (over a scripted connection) acks and routes frames, and a
`SocketError::Idle` read runs the `tick` hook then keeps reading rather than
treating the yield as a fault or a delivered frame.
