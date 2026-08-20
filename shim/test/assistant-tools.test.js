// Unit tests for the shared assistant MCP tool builder. Run with
// `node --test shim/test/assistant-tools.test.js`.

import test from 'node:test';
import assert from 'node:assert/strict';

import { buildAssistantTools, ALL_ASSISTANT_TOOLS } from '../src/assistant-tools.js';

test('includes only the enabled tools, in canonical order', () => {
  const { tools, allowedToolNames } = buildAssistantTools({
    enabled: ['save_memory', 'schedule_message'],
  });
  assert.equal(tools.length, 2);
  // Order follows ALL_ASSISTANT_TOOLS, not the enabled-set order.
  assert.deepEqual(allowedToolNames, [
    'mcp__assistant__schedule_message',
    'mcp__assistant__save_memory',
  ]);
});

test('omits send_message when no destinations are given', () => {
  const { allowedToolNames } = buildAssistantTools({ enabled: ['send_message'] });
  assert.deepEqual(allowedToolNames, []);
});

test('includes send_message when destinations are present', () => {
  const { tools, allowedToolNames, buffers } = buildAssistantTools({
    enabled: ['send_message'],
    destinations: [{ name: 'orchestrator', description: 'the Slack-wired agent' }],
  });
  assert.equal(tools.length, 1);
  assert.deepEqual(allowedToolNames, ['mcp__assistant__send_message']);
  assert.deepEqual(buffers.messages, []);
});

test('freeformTo includes send_message even with no destinations', () => {
  const { tools, allowedToolNames } = buildAssistantTools({
    enabled: ['send_message'],
    freeformTo: true,
  });
  assert.equal(tools.length, 1);
  assert.deepEqual(allowedToolNames, ['mcp__assistant__send_message']);
});

test('returns the full set of collection buffers', () => {
  const { buffers } = buildAssistantTools({ enabled: [] });
  for (const key of ['scheduled', 'cancellations', 'pauses', 'resumes', 'memories', 'messages']) {
    assert.deepEqual(buffers[key], [], `buffer ${key} should start empty`);
  }
});

test('an empty enable set yields no tools', () => {
  const { tools, allowedToolNames } = buildAssistantTools({ enabled: [] });
  assert.equal(tools.length, 0);
  assert.deepEqual(allowedToolNames, []);
});

test('ALL_ASSISTANT_TOOLS lists the six known tools', () => {
  assert.deepEqual(ALL_ASSISTANT_TOOLS, [
    'schedule_message',
    'cancel_schedule',
    'pause_schedule',
    'resume_schedule',
    'save_memory',
    'send_message',
  ]);
});
