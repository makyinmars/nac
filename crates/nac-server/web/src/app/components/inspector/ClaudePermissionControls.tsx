import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { Button, ButtonSize, ButtonVariant, Modal, ModalSize } from "@/app/atoms";
import { errorMessage, useToast } from "@/app/providers/ToastProvider";
import { toRunError } from "@/app/lib/providerError";
import { api } from "@/app/services/api";

/** Claude Code approval is scoped to the active run, never a NAC native tool grant. */
export function ClaudePermissionControls({ sessionId }: { sessionId: string }) {
  const toast = useToast();
  const client = useQueryClient();
  const queryKey = ["session", sessionId, "claude-permissions"];
  const permissions = useQuery({
    queryKey,
    queryFn: ({ signal }) => api.getClaudePermissions(sessionId, signal),
    refetchInterval: 1_000,
    retry: false,
  });
  const reply = useMutation({
    mutationFn: ({
      requestId,
      runId,
      generation,
      decision,
    }: {
      requestId: string;
      runId: string;
      generation: number;
      decision: "allow_once" | "deny";
    }) => api.replyClaudePermission(sessionId, requestId, runId, generation, decision),
    onSuccess: () => client.invalidateQueries({ queryKey }),
  });
  const [dismissed, setDismissed] = useState<string | null>(null);
  const [manuallyOpen, setManuallyOpen] = useState(false);
  const active = permissions.data?.[0] ?? null;
  const open = manuallyOpen || Boolean(active && active.id !== dismissed);

  const answer = async (decision: "allow_once" | "deny") => {
    if (!active || reply.isPending) return;
    try {
      await reply.mutateAsync({
        requestId: active.id,
        runId: active.run_id,
        generation: active.generation,
        decision,
      });
      setDismissed(active.id);
      setManuallyOpen(false);
    } catch (error) {
      toast.error(`Unable to answer Claude permission: ${errorMessage(toRunError(error))}`);
    }
  };

  return (
    <>
      <Button
        size={ButtonSize.Small}
        variant={active ? ButtonVariant.GhostHighlightedAccent : ButtonVariant.Ghost}
        aria-label={`Claude Agent permissions${active ? ` (${permissions.data?.length})` : ""}`}
        onClick={() => setManuallyOpen(true)}
      >
        Claude approvals{active ? ` (${permissions.data?.length})` : ""}
      </Button>
      <Modal
        open={open}
        onClose={() => {
          setDismissed(active?.id ?? null);
          setManuallyOpen(false);
        }}
        size={ModalSize.Wide}
        title={active ? "Claude Agent permission required" : "Claude Agent permissions"}
        subheader="Claude Code requested access for its own tool. This decision applies only to the current run and execution host."
        footer={
          active ? (
            <div className="flex justify-end gap-2">
              <Button
                variant={ButtonVariant.SecondaryDestructive}
                disabled={reply.isPending}
                onClick={() => void answer("deny")}
              >
                Deny
              </Button>
              <Button
                variant={ButtonVariant.Primary}
                disabled={reply.isPending}
                onClick={() => void answer("allow_once")}
              >
                Allow once
              </Button>
            </div>
          ) : null
        }
      >
        {active ? (
          <div className="flex flex-col gap-3 text-small text-basic-primary">
            <p>
              <strong>Tool:</strong> {active.tool_name}
            </p>
            <pre className="max-h-72 overflow-auto whitespace-pre-wrap break-all rounded bg-elevation-level-2 p-3 text-micro">
              {active.input_preview}
            </pre>
            <p className="text-micro text-basic-secondary">
              Request {active.id} · Run {active.run_id}
            </p>
          </div>
        ) : (
          <p className="text-micro text-basic-secondary">
            No Claude Agent permissions are waiting.
          </p>
        )}
      </Modal>
    </>
  );
}
