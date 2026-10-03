import { cn } from "@/app/lib/cn";
import type { AgentRuntime } from "@/app/types/api";

export type { AgentRuntime };

export function AgentRuntimePicker({
  value,
  onChange,
  disabled = false,
}: {
  value: AgentRuntime;
  onChange: (runtime: AgentRuntime) => void;
  disabled?: boolean;
}) {
  const choices: { id: AgentRuntime; label: string; detail: string }[] = [
    { id: "nac", label: "NAC", detail: "Use NAC's configured models and tools." },
    {
      id: "claude-agent",
      label: "Claude Agent",
      detail: "Use Claude Code and its subscription login on the execution host.",
    },
  ];
  return (
    <fieldset className="flex flex-col gap-2">
      <legend className="label-small text-basic-primary">Agent runtime</legend>
      <div className="grid grid-cols-1 gap-2 md:grid-cols-2" role="radiogroup">
        {choices.map((choice, index) => (
          <button
            key={choice.id}
            type="button"
            role="radio"
            aria-checked={value === choice.id}
            tabIndex={value === choice.id ? 0 : -1}
            disabled={disabled}
            onClick={() => onChange(choice.id)}
            onKeyDown={(event) => {
              const next =
                event.key === "ArrowRight" || event.key === "ArrowDown"
                  ? (index + 1) % choices.length
                  : event.key === "ArrowLeft" || event.key === "ArrowUp"
                    ? (index + choices.length - 1) % choices.length
                    : event.key === "Home"
                      ? 0
                      : event.key === "End"
                        ? choices.length - 1
                        : null;
              if (next == null) return;
              event.preventDefault();
              onChange(choices[next].id);
              const radios =
                event.currentTarget.parentElement?.querySelectorAll<HTMLElement>('[role="radio"]');
              radios?.[next]?.focus();
            }}
            className={cn(
              "rounded-[6px] border p-3 text-left",
              value === choice.id
                ? "border-accent-primary bg-accent-secondary"
                : "border-border-primary bg-elevation-level-1",
            )}
          >
            <span className="block text-small font-medium text-basic-primary">{choice.label}</span>
            <span className="text-xs text-basic-secondary">{choice.detail}</span>
          </button>
        ))}
      </div>
    </fieldset>
  );
}
