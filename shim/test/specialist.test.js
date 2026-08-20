// Unit tests for the generic specialist harness's one offline-testable helper:
// `specialistOptionsFromEnv`, which projects the host-supplied ASSISTANT_SPECIALIST_*
// env into the turn's options. The turn itself (runSpecialistTurn) drives the
// Agent SDK and is exercised only by the live smoke, never the offline gate. Run
// with `node --test shim/test/specialist.test.js`.

import test from 'node:test';
import assert from 'node:assert/strict';

import { specialistOptionsFromEnv } from '../src/specialist.js';

test('reads system prompt, tools, allowed tools, and max turns from env', () => {
  const opts = specialistOptionsFromEnv({
    ASSISTANT_SPECIALIST_SYSTEM_PROMPT: 'You are a web browsing specialist.',
    ASSISTANT_SPECIALIST_TOOLS: JSON.stringify(['Bash']),
    ASSISTANT_SPECIALIST_ALLOWED_TOOLS: JSON.stringify(['Bash(agent-browser:*)']),
    ASSISTANT_SPECIALIST_MAX_TURNS: '25',
  });
  assert.equal(opts.systemPrompt, 'You are a web browsing specialist.');
  assert.deepEqual(opts.tools, ['Bash']);
  assert.deepEqual(opts.allowedTools, ['Bash(agent-browser:*)']);
  assert.equal(opts.maxTurns, 25);
});

test('defaults a missing system prompt to empty and missing tool lists to empty', () => {
  const opts = specialistOptionsFromEnv({});
  assert.equal(opts.systemPrompt, '');
  assert.deepEqual(opts.tools, []);
  assert.deepEqual(opts.allowedTools, []);
  assert.deepEqual(opts.mcpTools, []);
  assert.deepEqual(opts.destinations, []);
});

test('reads assistant MCP tools and send_message destinations from env', () => {
  const opts = specialistOptionsFromEnv({
    ASSISTANT_SPECIALIST_MCP_TOOLS: JSON.stringify(['schedule_message', 'send_message']),
    ASSISTANT_SPECIALIST_DESTINATIONS: JSON.stringify([
      { name: 'orchestrator', description: 'the Slack-wired agent' },
    ]),
  });
  assert.deepEqual(opts.mcpTools, ['schedule_message', 'send_message']);
  assert.deepEqual(opts.destinations, [
    { name: 'orchestrator', description: 'the Slack-wired agent' },
  ]);
});

test('drops malformed destinations (missing name/description)', () => {
  const opts = specialistOptionsFromEnv({
    ASSISTANT_SPECIALIST_DESTINATIONS: JSON.stringify([
      { name: 'ok', description: 'fine' },
      { name: 'no-desc' },
      { description: 'no-name' },
      'garbage',
    ]),
  });
  assert.deepEqual(opts.destinations, [{ name: 'ok', description: 'fine' }]);
});

test('defaults max turns to 40 when absent or not a positive integer', () => {
  assert.equal(specialistOptionsFromEnv({}).maxTurns, 40);
  assert.equal(specialistOptionsFromEnv({ ASSISTANT_SPECIALIST_MAX_TURNS: 'nope' }).maxTurns, 40);
  assert.equal(specialistOptionsFromEnv({ ASSISTANT_SPECIALIST_MAX_TURNS: '0' }).maxTurns, 40);
  assert.equal(specialistOptionsFromEnv({ ASSISTANT_SPECIALIST_MAX_TURNS: '-3' }).maxTurns, 40);
});

test('falls back to empty lists for malformed or non-array tool JSON', () => {
  assert.deepEqual(specialistOptionsFromEnv({ ASSISTANT_SPECIALIST_TOOLS: 'not json' }).tools, []);
  assert.deepEqual(
    specialistOptionsFromEnv({ ASSISTANT_SPECIALIST_TOOLS: JSON.stringify({ a: 1 }) }).tools,
    [],
  );
  assert.deepEqual(
    specialistOptionsFromEnv({ ASSISTANT_SPECIALIST_ALLOWED_TOOLS: JSON.stringify([1, 2]) }).allowedTools,
    [],
  );
});
