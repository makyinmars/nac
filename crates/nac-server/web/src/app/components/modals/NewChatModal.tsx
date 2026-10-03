import { useCallback, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";

import { Button, ButtonVariant, Loader, LoaderSize, Modal, ModalSize } from "@/app/atoms";
import {
  ConfigurationsPanel,
  type ConfigurationsPanelInitial,
  type LaunchModelSelection,
} from "@/app/components/modals/ConfigurationsPanel";
import { LightModelSection, type LightSelection } from "@/app/components/modals/LightModelSection";
import { AgentRuntimePicker, type AgentRuntime } from "@/app/components/modals/AgentRuntimePicker";
import {
  ClaudeLaunchSettings,
  type ClaudeLaunchSelection,
} from "@/app/components/modals/ClaudeLaunchSettings";
import { PrimaryModelSection } from "@/app/components/modals/PrimaryModelSection";
import { SessionBehaviorPicker } from "@/app/components/modals/SessionBehaviorPicker";
import { useExitTransition } from "@/app/hooks/useExitTransition";
import { inheritPrimaryCredential } from "@/app/lib/modelConfig";
import {
  newestCreatedPrimarySessionForProject,
  newestPrimarySessionForProject,
} from "@/app/lib/projects";
import { humanErrorText, toRunError } from "@/app/lib/providerError";
import { routes } from "@/app/lib/routes";
import { useToast } from "@/app/providers/ToastProvider";
import { api } from "@/app/services/api";
import {
  useCreateModelConfig,
  useCreateSession,
  useModelConfigs,
  useProjects,
  useSessionConfig,
  useSessions,
} from "@/app/services/queries";
import type {
  BackendKind,
  CreateSessionRequest,
  LightModelSettings,
  ModelConfigurationRecord,
  RawSessionConfig,
  SessionBehavior,
  SshTarget,
} from "@/app/types/api";

interface InheritedModelSelection {
  initial: ConfigurationsPanelInitial;
  light: LightModelSettings | null;
}

function parseHeaders(json: string | null | undefined): Record<string, string> {
  if (!json) return {};
  try {
    const parsed: unknown = JSON.parse(json);
    if (Object(parsed) !== parsed || Array.isArray(parsed)) return {};
    return parsed as Record<string, string>;
  } catch {
    return {};
  }
}

function fromSavedConfiguration(record: ModelConfigurationRecord): InheritedModelSelection {
  return {
    initial: {
      // SAFETY: model configurations are created through the backend's typed
      // BackendKind request even though the stored compatibility record is a string.
      backend: record.backend as BackendKind,
      model: record.model,
      base_url: record.base_url,
      allow_insecure_http: record.allow_insecure_http,
      api_key_env: record.api_key_env ?? null,
      reasoning_effort: record.reasoning_effort ?? null,
      extra_headers: record.extra_headers,
    },
    light: record.light_model ?? null,
  };
}

function fromSessionConfiguration(
  config: RawSessionConfig,
  summaryBackend: string,
): InheritedModelSelection {
  return {
    initial: {
      // SAFETY: both values originate from the server's validated BackendKind
      // persistence path; the raw compatibility projection permits null/string.
      backend: (config.backend ?? summaryBackend) as BackendKind,
      model: config.model,
      base_url: config.base_url,
      allow_insecure_http: config.allow_insecure_http,
      api_key_env: config.api_key_env ?? null,
      reasoning_effort: config.reasoning_effort ?? null,
      extra_headers: parseHeaders(config.extra_headers_json),
    },
    light: config.light_model ?? null,
  };
}

export function NewChatModal({
  projectId,
  firstChat = false,
  onClose,
}: {
  projectId: string | null;
  firstChat?: boolean;
  onClose: () => void;
}) {
  const mounted = useExitTransition(projectId !== null);
  if (!mounted || projectId === null) return null;
  return <NewChatForm projectId={projectId} firstChat={firstChat} onClose={onClose} />;
}

function NewChatForm({
  projectId,
  firstChat,
  onClose,
}: {
  projectId: string;
  firstChat: boolean;
  onClose: () => void;
}) {
  const navigate = useNavigate();
  const toast = useToast();
  const createSession = useCreateSession();
  const createModelConfig = useCreateModelConfig();
  const projects = useProjects();
  const sessions = useSessions();
  const modelConfigs = useModelConfigs();
  const [behavior, setBehavior] = useState<SessionBehavior>("orchestrator");
  const [runtime, setRuntime] = useState<AgentRuntime>("nac");
  const [claude, setClaude] = useState<ClaudeLaunchSelection>({
    executable: "claude",
    model: "",
    configDir: "",
    trustedWorkspace: false,
  });
  const [selection, setSelection] = useState<LaunchModelSelection | null>(null);
  const [light, setLight] = useState<LightSelection>({ mode: "single", light: null });
  const [error, setError] = useState("");
  const [advanced, setAdvanced] = useState(false);

  const project = projects.data?.projects.find((entry) => entry.project_id === projectId) ?? null;
  const claudeHost: SshTarget | null = project?.ssh_host
    ? {
        ssh_host: project.ssh_host,
        ssh_port: project.ssh_port,
        ssh_identity_file: project.ssh_identity_file,
      }
    : null;
  const sibling = newestCreatedPrimarySessionForProject(sessions.data ?? [], projectId);
  const defaultConfig = project?.default_model_config_id
    ? (modelConfigs.data?.configurations.find(
        (record) => record.config_id === project.default_model_config_id,
      ) ?? null)
    : null;
  const siblingConfig = useSessionConfig(
    project?.default_model_config_id ? null : (sibling?.summary.session_id ?? null),
  );
  const inherited = useMemo<InheritedModelSelection | null>(() => {
    if (defaultConfig) return fromSavedConfiguration(defaultConfig);
    if (siblingConfig.data && sibling) {
      return fromSessionConfiguration(siblingConfig.data, sibling.summary.backend);
    }
    return null;
  }, [defaultConfig, siblingConfig.data, sibling]);
  const inheritancePending =
    projects.isPending ||
    sessions.isPending ||
    modelConfigs.isPending ||
    (Boolean(sibling) && !project?.default_model_config_id && siblingConfig.isPending);
  const busy = createSession.isPending || createModelConfig.isPending;

  const onSelection = useCallback((next: LaunchModelSelection | null) => {
    setSelection(next);
    setError("");
  }, []);

  const onLight = useCallback((next: LightSelection) => {
    setLight(next);
    setError("");
  }, []);

  // A saved setup selected inside the primary picker owns its light-model
  // default. Catalog/file selections carry no opinion, so they preserve the
  // project's effective inherited choice until the user changes it.
  const selectedLight =
    selection?.kind === "resolved" && selection.light_model !== undefined
      ? selection.light_model
      : inherited?.light;
  const selectedLightKey = JSON.stringify(selectedLight);

  const submit = async () => {
    if (busy || (runtime === "nac" && inheritancePending)) return;
    if (runtime === "claude-agent" && !claude.trustedWorkspace) {
      setError("Confirm that you trust this workspace before starting Claude Code.");
      return;
    }
    if (runtime === "nac" && !selection) {
      setError("Choose the primary model before creating this chat.");
      return;
    }
    if (runtime === "nac" && light.mode === "dual" && !light.light) {
      setError("Pick the light model before creating this chat.");
      return;
    }
    try {
      if (firstChat) {
        const [projects, sessions] = await Promise.all([
          api.listProjects(),
          api.listSessions({ projectId }),
        ]);
        if (!projects.projects.some((project) => project.project_id === projectId)) {
          onClose();
          navigate(routes.list(), { replace: true });
          return;
        }
        const existing = newestPrimarySessionForProject(sessions, projectId);
        if (existing) {
          onClose();
          navigate(routes.session(existing.summary.session_id), { replace: true });
          return;
        }
      }

      if (runtime === "claude-agent") {
        const request: CreateSessionRequest = {
          project_id: projectId,
          behavior: "direct" as const,
          first_chat: firstChat,
          agent_runtime: runtime,
          claude_executable: claude.executable.trim() || "claude",
          claude_model: claude.model.trim() || null,
          claude_config_dir: claude.configDir.trim() || null,
          claude_trusted_workspace: true,
        };
        const snapshot = await createSession.mutateAsync(request);
        onClose();
        if (snapshot.metadata.session_id) navigate(routes.session(snapshot.metadata.session_id));
        return;
      }

      if (!selection) return;

      let selected: {
        backend: BackendKind;
        model: string;
        base_url: string;
        allow_insecure_http: boolean;
        api_key_env: string | null;
        reasoning_effort: string | null;
        extra_headers: Record<string, string> | null;
        orchestrator_compaction_threshold?: number | null;
      };
      if (selection.kind === "save") {
        const record = await createModelConfig.mutateAsync({
          ...selection.request,
          light_model: light.mode === "dual" ? light.light : null,
        });
        selected = {
          // SAFETY: the server echoes the BackendKind wire value it stored.
          backend: record.backend as BackendKind,
          model: record.model,
          base_url: record.base_url,
          allow_insecure_http: record.allow_insecure_http ?? false,
          api_key_env: record.api_key_env ?? null,
          reasoning_effort: record.reasoning_effort ?? null,
          extra_headers: record.extra_headers,
          orchestrator_compaction_threshold: record.orchestrator_compaction_threshold ?? null,
        };
      } else {
        selected = selection;
      }

      const finalLight =
        light.mode === "dual" && light.light
          ? inheritPrimaryCredential(light.light, selected.backend, selected.api_key_env)
          : null;
      const request: CreateSessionRequest = {
        project_id: projectId,
        behavior,
        first_chat: firstChat,
        backend: selected.backend,
        model: selected.model,
        base_url: selected.base_url,
        allow_insecure_http: selected.allow_insecure_http,
        api_key_env: selected.api_key_env,
        reasoning_effort: selected.reasoning_effort,
        extra_headers: selected.extra_headers,
        // Explicit null matters: it lets a user turn an inherited dual-model
        // project default into a single-model chat without changing the project.
        light_model: finalLight,
      };
      if (selected.orchestrator_compaction_threshold !== undefined) {
        request.orchestrator_compaction_threshold = selected.orchestrator_compaction_threshold;
      }
      const snapshot = await createSession.mutateAsync(request);
      const sessionId = snapshot.metadata.session_id;
      onClose();
      if (sessionId) navigate(routes.session(sessionId));
    } catch (error) {
      toast.error(`Failed to start a chat: ${humanErrorText(toRunError(error))}`);
    }
  };

  return (
    <Modal
      open
      onClose={onClose}
      size={ModalSize.Wide}
      flush
      className="h-[700px]"
      title="New Chat"
      subheader="Choose the runtime for this chat. Its settings do not change the project default."
      footer={
        <Button
          variant={ButtonVariant.Primary}
          loading={busy}
          disabled={
            busy ||
            (runtime === "nac" && (inheritancePending || !selection)) ||
            (runtime === "claude-agent" && (!project || !claude.trustedWorkspace))
          }
          onClick={() => void submit()}
        >
          Create chat
        </Button>
      }
    >
      <AgentRuntimePicker
        value={runtime}
        onChange={(next) => {
          setRuntime(next);
          if (next === "claude-agent") setBehavior("direct");
          setError("");
        }}
        disabled={busy}
      />
      {runtime === "nac" ? (
        <SessionBehaviorPicker value={behavior} onChange={setBehavior} disabled={busy} />
      ) : (
        <p className="text-micro text-basic-secondary">Claude Agent uses direct chat behavior.</p>
      )}
      {runtime === "claude-agent" && !project ? (
        <p role="status" className="text-micro text-basic-muted">
          Loading the project host…
        </p>
      ) : runtime === "claude-agent" ? (
        <ClaudeLaunchSettings value={claude} onChange={setClaude} host={claudeHost} />
      ) : inheritancePending ? (
        <div className="flex items-center gap-2 py-6 text-micro text-basic-muted" role="status">
          <Loader size={LoaderSize.Micro} />
          Loading the project's model settings…
        </div>
      ) : (
        <div className="flex flex-col gap-3">
          {advanced ? (
            <ConfigurationsPanel
              invalid={Boolean(error)}
              errorText={error || undefined}
              initial={inherited?.initial}
              onChange={onSelection}
            />
          ) : (
            <PrimaryModelSection initial={inherited?.initial} onChange={onSelection} />
          )}
          <Button
            variant={ButtonVariant.Secondary}
            onClick={() => {
              setAdvanced((value) => !value);
              setError("");
            }}
          >
            {advanced ? "Back to unified models" : "Advanced presets and provider setup"}
          </Button>
          <LightModelSection
            key={selectedLightKey}
            initial={selectedLight}
            behavior={behavior}
            onChange={onLight}
          />
          {error && !advanced ? (
            <p className="text-micro text-error-primary" role="alert">
              {error}
            </p>
          ) : null}
        </div>
      )}
    </Modal>
  );
}
