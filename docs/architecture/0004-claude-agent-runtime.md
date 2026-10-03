# 0004 — Claude Agent runtime integration

Status: implemented for personal use; live SSH validation pending

## Goal and scope

Let NAC control an installed Claude Code agent using the user's existing login
on the machine where it runs. This plan targets the user's personal NAC
installation; NAC does not provide a Claude login, proxy credentials, or pool
subscription access. The first implementation supports (1) top-level Claude
Agent chats and (2) Claude workers dispatched by a NAC orchestrator, on local
and SSH workspaces. Existing NAC sessions, worker dispatches, model providers,
and the three public session behaviors retain their current defaults and
meanings.

This is a separate agent runtime, not another `ModelClient` backend. Claude
Code owns its model loop, context, built-in tools, and native session files;
NAC owns admission, workspace selection, presentation, durable run identity,
completion delivery, and process supervision. A top-level Claude chat uses
`direct` behavior. A Claude worker remains an orchestrator thread, not a
traditional child session or managed orchestrator.

Podman and managed-host execution are outside this implementation. Reject those
combinations at creation or dispatch rather than running Claude on the host
while presenting a sandboxed or managed workspace.

## Product and durable contracts

1. Add an immutable `agent_runtime` session field: `nac` by default for every
   existing row and omitted create request; `claude-agent` only with top-level
   `direct` behavior. Keep `orchestrator`, `direct`, and
   `direct-with-orchestrator` unchanged. A backward-compatible migration adds
   the field and Claude-specific metadata without rewriting existing identity
   or model configuration. Expose the runtime in session summaries, snapshots,
   and the generated OpenAPI contract.
2. Give Claude sessions their own configuration: executable selection on the
   execution host, optional Claude model, optional `CLAUDE_CONFIG_DIR`, and
   pinned host/workspace identity. Do not require an Anthropic API key or
   synthesize a NAC `anthropic-messages` backend. Keep NAC model settings for
   `nac` sessions; route Claude settings through a distinct validated path.
3. Add optional `agent: "nac" | "claude"` to the `thread` tool, defaulting to
   `nac`. Store the chosen agent with each named thread and reject switching an
   existing thread to another agent. Store that thread's Claude session ID and
   bind it to the parent session, SSH host identity, workspace, and config
   directory. Keep existing dispatch IDs, episode status, retained handoff,
   steering, timeout, and generation semantics. The current `weight` field
   selects NAC's light/heavy model only for `agent: "nac"`; a Claude dispatch
   does not inherit the parent's model or require `weight`.
4. NAC's transcript and events remain the UI/recovery record. Claude's native
   session ID is the context-resume handle. Persist it as soon as Claude emits
   initialization, before later tool calls or results. Resume only when the
   exact host/config/workspace binding and native transcript are available;
   report an actionable repair error otherwise. Never reconstruct Claude
   context by silently replaying NAC's displayed transcript.

## Execution seam

Create a narrow internal `SessionEngine` seam for starting a turn, streaming
events, interrupting it, and returning a typed result. The existing NAC Agent
and a new Claude process adapter satisfy it. Keep leases, inbox admission,
run generations, settlement, recovery, and event publication in
`nac-core::session_service`; do not duplicate them in HTTP handlers or the
Claude adapter. Factor only the agent-specific calls needed by this second
implementation, since `SessionService` currently owns an `Agent` directly.

Implement the Claude adapter in `nac-core` with a separately supervised Agent
SDK bridge. The installed Claude Code 2.0.31 CLI lacks
`--permission-prompt-tool`, so raw `-p` streaming is not an approval transport.
The bridge uses the pinned Agent SDK, a host-installed Claude Code CLI at
version 2.1.280 or newer, and a bounded newline-delimited JSON protocol over
standard input and output. Test local and SSH streaming, session ID capture,
resume, cancellation, and tool approval against this protocol. Do not depend
on undocumented CLI control messages.

Run Claude in the selected workspace on the selected host. Local launches use
`nac-process` process-tree supervision. SSH launches reuse NAC's SSH command,
quoting, remote process identity, and cleanup conventions; they must not fall
back to a local Claude process. The remote host needs its own Claude Code
installation and login. Verify this at launch without copying credentials to
NAC's database, logs, or another host. Explicitly detect API-key environment
overrides so a chat selected as subscription-backed cannot silently bill an
API key. Do not use `--bare`: Anthropic documents that it ignores subscription
OAuth credentials. Require an explicit trusted-workspace gate before the first
launch and retain that binding for resume. The bridge sets SDK
`settingSources: []`. A live test showed that project settings can inject
`ANTHROPIC_API_KEY` after the SDK process environment is scrubbed, switching
the request away from subscription login. This necessary design change disables
user, project, and local filesystem settings, including project `CLAUDE.md`,
hooks, and MCP configuration from those sources. The bridge also rejects an
SDK init that reports an API-key credential source. The trust gate remains for
workspace and host execution. SDK lifecycle notices may precede init; the
bridge discards them and forwards no ordinary output until NAC acknowledges
the durable native session ID.

Translate Claude text, tool activity, usage, errors, and final results to
NAC's existing bounded event/transcript forms. Preserve Claude tool IDs and
subagent parent IDs where available; do not invent NAC worker/child records for
Claude's internal subagents. Bound and redact captured output. SDK tool events
and usage still stream. NAC buffers Claude text deltas privately
and shows assistant text after a complete assistant message, since a credential
can be split across adjacent deltas. It redacts the combined partial text
before retaining it on cancellation or failure. Commit a worker
handoff before reporting success, using its dispatch ID and existing episode
watermark so crash/retry cannot produce a second success or an error over a
committed answer. Persist top-level user input before starting Claude and
settle a run exactly once after a final result or failure.

## Permissions and cancellation

Claude's built-in tools are executed by Claude Code, so NAC's prepared native
tool grants cannot authorize them. The bridge exposes only `Read`, `Glob`,
`Grep`, `Edit`, and `Write`. This list limits capabilities; it does not approve
any invocation. An SDK `PreToolUse` hook asks NAC about each invocation. NAC
binds the request to the active session, run or dispatch generation, and
Claude tool ID. An explicit `allow_once` answer lets `PreToolUse` grant that
invocation. If the SDK separately calls `canUseTool`, it rejects any prompt
that does not match the approved tool name, input, and ID. A missing interactive approver,
closed channel, cancellation, or timeout denies the invocation. NAC native
remembered grants and auto-approval never apply. The bridge does not pass
`--dangerously-skip-permissions`. Its hard approval policy covers the exposed
file tool invocations. User, project, and local Claude settings are disabled
for subscription safety. The bridge does not expose Bash as a model tool.

For `Read`, `Glob`, `Grep`, `Edit`, and `Write`, the broker resolves the requested
path on the selected execution host before showing an approval and again after
`allow_once`. It rejects workspace escape, symbolic links below the workspace,
and direct `Edit`/`Write` targets in Git metadata or the local NAC session
store. Claude Code executes the file operation after the final check. It does
not use NAC's prepared no-follow handles, revision-checked atomic mutation, or
execution-time resource binding. A concurrent process can replace a path
between NAC's final check and Claude's operation, potentially redirecting a
read or write outside the checked resource. This is a known policy difference
for both local and SSH sessions, even in a workspace that passed the trust
gate. Trusting the workspace must include the users and processes that can
mutate its paths; the approval UI does not remove this race.

For SSH, the SDK control protocol travels through the authenticated,
supervised SSH standard input and output channel. No reverse tunnel or MCP
permission endpoint is used. The adapter retains a remote process identity
handle before launch and uses SSH identity-checked cleanup on cancellation or
connection loss. An approval never changes the selected local or SSH target.

Cancellation must stop the Claude process and descendants on that target,
close pending approvals, retain partial output, and settle the matching run or
dispatch generation once. On server restart, reconcile NAC's durable run
marker with the native session ID and remote process identity before accepting
another turn. A lost SSH connection must not leave an untracked Claude process
editing the workspace. Start with queue-after-completion semantics for input
sent during a Claude turn; add mid-turn steering only after the adapter proves
an acknowledgment protocol with the same durability guarantees as NAC's
direct inbox.

## User interface and API

Add a runtime choice before behavior/model controls in New Chat and project
first-chat creation. Choosing Claude Agent selects `direct`, shows the selected
host's CLI/login status and Claude model choice, and hides NAC-only model and
light-model fields. In an orchestrator thread dispatch, expose the optional
Claude agent choice and display it on the thread card. Keep the default NAC
choice in both paths. Claude session Settings show the immutable runtime,
host/config binding, and supported Claude settings; NAC-only goals, native
permission grants, compaction, and child/orchestrator controls must be hidden
or explicitly rejected for Claude sessions until their semantics are designed.

Use Rust delivery schemas as the source of truth and regenerate OpenAPI/TS via
`make generate-api-contract`. Rebuild committed web assets with the web build.
Document that Claude also stores native transcripts on the execution host;
deleting a NAC session/thread does not claim to erase those files unless an
exact, supported native deletion operation is implemented.

## Implementation order and acceptance gates

1. **Protocol and account verification.** Record tested local/SSH CLI
   versions, auth precedence, SDK bridge approval transport, streaming fixtures, and
   cancellation behavior. Confirm that each process uses the user's installed
   CLI and existing subscription login on its execution host, with no API-key
   override or credential transfer.
2. **Durable runtime identity.** Add schema, codecs, session/worker selection,
   and migration tests. Verify all legacy sessions and omitted requests still
   use NAC; reject incompatible behavior/backend combinations before writing.
3. **Claude process adapter.** Implement local and SSH spawn, preflight,
   SDK bridge parsing, session-ID capture/resume, event mapping, output limits,
   `PreToolUse` approval, cancellation, and cleanup. Test fake processes and a
   controlled SSH host before live Claude runs.
4. **Top-level service path.** Integrate Claude with existing admission,
   transcript, snapshots, run settlement, recovery, queued input, and idle
   attachment. Characterize crashes after prompt commit, native init, partial
   output, final result, and NAC settlement.
5. **Worker path.** Add `thread.agent`, per-thread native identity, source
   episode delivery, retained handoff, timeout/steering/cancel handling, and
   mixed NAC/Claude thread tests. A repeated dispatch resumes the same Claude
   thread; a different named thread never shares its native session.
6. **Delivery and UI.** Add API, generated contract, controls, status/error
   presentation, and production E2E journeys for local and SSH sessions and
   Claude worker dispatch.

Run `make setup` in each fresh worktree and after lockfile changes. Required
checks include focused core store/session_service/thread/permission/process
tests, migration and restart/peer tests, local and SSH live smoke tests,
`make test-durability`, `make crate-check CRATE=nac-core`,
`make crate-test CRATE=nac-server`, `make test-api-contract`,
`make test-assets`, `make test-e2e`, `make format-check`, and
`make test-source-size`. Report missing SSH/container infrastructure as a
coverage gap. Treat policy parity, lost-connection cleanup, and restart-safe
resume as acceptance gates for the personal-use implementation.

If NAC later offers this as a general subscription-login feature to other
users, review Anthropic's published restriction on third-party products before
that distribution. The restriction does not turn on whether NAC is sold, and
T3 Code's implementation does not establish permission for NAC. This plan
treats distribution as a separate decision, not an implementation gate for
the user's own locally authenticated setup.

## References

- [T3 Code's Claude provider guide](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-claude.md)
  and [Claude adapter at the inspected revision](https://github.com/pingdotgg/t3code/blob/de251fc2971a884cb5b1305ba4daf309dc8cccb0/apps/server/src/provider/Layers/ClaudeAdapter.ts)
  show the installed-binary and local-login approach. These are read-only
  design references; no code or dependency is imported from T3 Code.
- [Anthropic's Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview)
  states the third-party product restriction relevant to wider distribution.
- [Anthropic's programmatic CLI guide](https://code.claude.com/docs/en/headless)
  documents streaming output, native session IDs, and the `--bare` login
  behavior. The [Agent SDK permission guide](https://platform.claude.com/docs/en/agent-sdk/permissions)
  documents `PreToolUse` and `canUseTool`.
