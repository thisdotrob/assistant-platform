# assistant-base: the slim shared agent container image. The session DB protocol
# uses Node's built-in `node:sqlite` (no native build), so the base needs only
# Node + the Claude Code CLI + the shim source. Specialists that need extra
# binaries (e.g. the browser specialist's Chromium + agent-browser) ship their
# own image `FROM assistant-base:<ver>` that adds only those binaries; see
# crates/assistant-specialist-browser/image/Dockerfile. Built outside the sandbox
# (npm needs network); the run-loop spawns it per session and bind-mounts /session.

FROM node:22-slim

# The Agent SDK shells out to the Claude Code CLI. Package name/version is
# pinned during the live smoke; expose it as a build arg so it can be corrected
# or pinned without editing this file.
ARG CLAUDE_CLI_PACKAGE=@anthropic-ai/claude-code

WORKDIR /app

# Dependencies first for layer caching, then source.
COPY package.json ./
RUN npm install --omit=dev \
    && npm install -g "${CLAUDE_CLI_PACKAGE}"
COPY src ./src

# The host bind-mounts the per-session DBs and heartbeat here.
ENV ASSISTANT_SESSION_DIR=/session

# `--experimental-sqlite` is required on Node 22 and harmlessly accepted on newer.
ENTRYPOINT ["node", "--experimental-sqlite", "src/index.js"]
