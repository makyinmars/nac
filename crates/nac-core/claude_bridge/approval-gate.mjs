// PreToolUse runs before Claude's rules and modes, including auto-allow. NAC
// decides once per toolUseID; canUseTool only replays that exact decision.
export function createApprovalGate(askNac) {
  const approved = new Map();
  return {
    hooks: {
      PreToolUse: [{ hooks: [async (input, toolUseID, context) => {
        const name = input.tool_name;
        const toolInput = input.tool_input;
        const callId = toolUseID || input.tool_use_id;
        if (!callId || (toolUseID && input.tool_use_id && toolUseID !== input.tool_use_id)) {
          return { hookSpecificOutput: {
            hookEventName: 'PreToolUse',
            permissionDecision: 'deny',
            permissionDecisionReason: 'Claude tool call identity is missing or inconsistent',
          } };
        }
        let answer;
        try {
          answer = await askNac(name, toolInput, callId, context.signal);
        } catch {
          answer = { allow: false, reason: 'NAC approval channel failed' };
        }
        const allowed = answer.allow && Boolean(callId);
        if (allowed) {
          approved.set(callId, JSON.stringify([name, toolInput]));
          if (approved.size > 64) approved.delete(approved.keys().next().value);
        }
        return { hookSpecificOutput: {
          hookEventName: 'PreToolUse',
          permissionDecision: allowed ? 'allow' : 'deny',
          permissionDecisionReason: allowed ? 'NAC approved this invocation' :
            (answer.reason || 'NAC denied this invocation'),
        } };
      }] }],
      PostToolUse: [{ hooks: [async (input, toolUseID) => {
        approved.delete(toolUseID || input.tool_use_id);
        return {};
      }] }],
      PostToolUseFailure: [{ hooks: [async (input, toolUseID) => {
        approved.delete(toolUseID || input.tool_use_id);
        return {};
      }] }],
    },
    canUseTool: async (name, input, context) => {
      const callId = context.toolUseID;
      const match = callId && approved.get(callId) === JSON.stringify([name, input]);
      if (callId) approved.delete(callId);
      return match ? { behavior: 'allow', updatedInput: input }
        : { behavior: 'deny', message: 'NAC did not approve this exact tool invocation' };
    },
  };
}
