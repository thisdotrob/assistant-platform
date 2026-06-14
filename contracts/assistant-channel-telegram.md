# assistant-channel-telegram Contract

## Public API
Telegram `ChannelAdapter`: connection lifecycle, sender pairing, polling/webhook transport policy, sender-identity resolution, message/file send, and card rendering with fallback.

## Persistence Ownership
Owns Telegram channel extension and readiness-state tables (including pairing state) via central DB migrations; does not mutate core or router base tables.

## Config
Reads the Telegram bot token and transport (polling vs webhook) settings from assistant-config (secrets via env overlay).

## Events
Receives Telegram updates and emits normalized inbound messages into the router.

## CLI/Web Surfaces
None directly; setup steps and readiness are surfaced through assistant-setup and assistant-web.

## Prompt Fragments
Owns Telegram-specific rendering/format fragments declared as a capability; none are platform-shared in the first release.

## Readiness Checks
Verifies the bot token is valid and the chosen transport (polling or webhook) is reachable.

## Conformance Tests
Normalized inbound and outbound messages satisfy the ChannelAdapter boundary; pairing binds a chat to a stable sender identity; cards degrade gracefully when unsupported.
