# assistant-channel-cli Contract

## Public API
Local CLI `ChannelAdapter`: start/stop, normalize terminal inbound messages, resolve a local sender identity, and send chat/file output for local and test flows.

## Persistence Ownership
Owns CLI-channel extension/readiness state via central DB migrations only where setup/readiness requires it; does not mutate core or router tables.

## Config
Reads the local channel enablement flag from assistant-config.

## Events
Receives platform events and emits normalized inbound messages into the router.

## CLI/Web Surfaces
None beyond the local terminal channel itself; operator commands belong to assistant-cli.

## Prompt Fragments
None in the first release.

## Readiness Checks
Verifies the local channel is enabled and can read and write terminal/test message streams.

## Conformance Tests
Normalized inbound messages satisfy the ChannelAdapter boundary; sender identity is stable for the local user; output and graceful card fallback render in a plain terminal.
