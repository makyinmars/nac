import assert from 'node:assert/strict';
import test from 'node:test';
import { assertSubscriptionInit, cleanEnvironment } from './credential-env.mjs';

test('SDK subprocess inherits no API or Claude token override', () => {
  const source = {
    PATH: '/bin',
    CLAUDE_CONFIG_DIR: '/host-config',
    ANTHROPIC_API_KEY: 'api-secret',
    CLAUDE_CODE_OAUTH_TOKEN: 'oauth-secret',
    CLAUDE_CODE_SESSION_ACCESS_TOKEN: 'session-secret',
    CLAUDE_CODE_FUTURE_TOKEN: 'future-secret',
    CLAUDE_CODE_HOST_SESSION_ID: 'host-session',
    CLAUDE_CODE_USE_BEDROCK: '1',
  };
  assert.deepEqual(cleanEnvironment(source), { PATH: '/bin', CLAUDE_CONFIG_DIR: '/host-config' });
  assert.deepEqual(cleanEnvironment(source, true), { PATH: '/bin' });
});

test('native init rejects an API-key credential source before NAC ack', () => {
  assert.doesNotThrow(() => assertSubscriptionInit({ apiKeySource: 'none' }));
  assert.throws(() => assertSubscriptionInit({ apiKeySource: 'ANTHROPIC_API_KEY' }), /API-key/);
  assert.throws(() => assertSubscriptionInit({ apiKeySource: 'apiKeyHelper' }), /API-key/);
});
