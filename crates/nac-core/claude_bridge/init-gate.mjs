// SDK output is unusable until NAC has durably saved the native session ID.
export function createInitGate() {
  let initialized = false;
  return (message) => {
    const isInit = message.type === 'system' && message.subtype === 'init';
    if (isInit) {
      if (initialized || typeof message.session_id !== 'string' || !message.session_id) {
        throw Error('Claude returned an invalid or duplicate native session identity');
      }
      initialized = true;
      return 'init';
    }
    if (!initialized) {
      // Claude settings may start trusted SessionStart hooks before the SDK
      // emits init. Their lifecycle notices cannot be attached to a durable
      // NAC session yet, so discard them without forwarding any tool/output.
      if (message.type === 'system' &&
          ['hook_started', 'hook_progress', 'hook_response'].includes(message.subtype)) {
        return 'defer';
      }
      if (message.type === 'rate_limit_event') return 'defer';
      throw Error(`Claude emitted ${message.type}/${message.subtype || ''} before native session identity`);
    }
    return 'event';
  };
}
