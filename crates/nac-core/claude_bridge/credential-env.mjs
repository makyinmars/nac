// The selected host's Claude Code installation supplies subscription login.
// Never pass credentials from the NAC process into the Agent SDK or SSH.
export function isCredentialOverride(key) {
  return key.startsWith('ANTHROPIC_') ||
    (key.startsWith('CLAUDE_CODE_') && key.endsWith('_TOKEN')) ||
    key === 'CLAUDE_CODE_HOST_SESSION_ID' ||
    key === 'CLAUDE_CODE_USE_BEDROCK' || key === 'CLAUDE_CODE_USE_VERTEX' ||
    key === 'CLAUDE_CODE_USE_FOUNDRY';
}

export function cleanEnvironment(source, remote = false) {
  return Object.fromEntries(Object.entries(source).filter(([key]) =>
    !isCredentialOverride(key) && !(remote && key === 'CLAUDE_CONFIG_DIR')));
}

export function assertSubscriptionInit(message) {
  if (!['none', 'oauth'].includes(message.apiKeySource)) {
    throw Error('Claude selected an API-key credential source instead of the host subscription login');
  }
}
