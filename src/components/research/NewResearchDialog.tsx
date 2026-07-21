import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { AgentAdapterMetadata } from "../../types";
import { LauncherSelect, type LauncherSelectOption } from "../LauncherSelect";
import {
  ComposerSubmitShortcutGlyph,
  isComposerSubmitShortcut,
} from "../ComposerSubmitShortcut";
import { ADAPTER_ICON_BY_ID, adapterIconClassName } from "../../lib/adapterIcons";
import { CLAUDE_ADAPTER_ID, CLAUDE_EFFORT_OPTIONS } from "../../adapters/claude";
import { CODEX_ADAPTER_ID, CODEX_REASONING_OPTIONS } from "../../adapters/codex";

// Model presets per adapter; "custom" reveals a free-form input. Adapters
// without a curated list only offer "custom".
const MODEL_PRESETS_BY_ADAPTER: Record<string, string[]> = {
  [CLAUDE_ADAPTER_ID]: ["fable", "opus", "sonnet", "custom"],
  [CODEX_ADAPTER_ID]: [
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "custom",
  ],
};

const CUSTOM_MODEL = "custom";

// GPT-5.4 stops at extra high; every other Codex preset (and a custom model,
// whose ceiling is unknown here) offers the full range and lets the CLI
// reject a level the model does not support.
const GPT_5_4_REASONING_LEVELS = ["", "low", "medium", "high", "xhigh"];

function modelPresetsFor(adapter: string): string[] {
  return MODEL_PRESETS_BY_ADAPTER[adapter] ?? [CUSTOM_MODEL];
}

// The reasoning/effort levels the selected model supports, or null for
// adapters without a reasoning-effort launch option. Every Claude model
// (Fable, Opus, Sonnet) shares one range; Codex ranges vary by model.
function effortOptionsFor(adapter: string, model: string): LauncherSelectOption[] | null {
  if (adapter === CLAUDE_ADAPTER_ID) {
    return CLAUDE_EFFORT_OPTIONS;
  }
  if (adapter === CODEX_ADAPTER_ID) {
    if (model === "gpt-5.4") {
      return CODEX_REASONING_OPTIONS.filter((option) =>
        GPT_5_4_REASONING_LEVELS.includes(option.value),
      );
    }
    return CODEX_REASONING_OPTIONS;
  }
  return null;
}

interface NewResearchDialogProps {
  open: boolean;
  inline?: boolean;
  adapters: AgentAdapterMetadata[];
  requireCmdEnterToSend: boolean;
  workspaceId: string | null;
  onClose: () => void;
  onCreate: (input: {
    prompt: string;
    adapter: string;
    model: string | null;
    effort: string | null;
    workspaceId: string | null;
  }) => Promise<void>;
}

export default function NewResearchDialog({
  open,
  inline = false,
  adapters: allAdapters,
  requireCmdEnterToSend,
  workspaceId,
  onClose,
  onCreate,
}: NewResearchDialogProps) {
  const [prompt, setPrompt] = useState("");
  const [adapter, setAdapter] = useState("");
  const [modelChoice, setModelChoice] = useState<string | null>(null);
  const [customModel, setCustomModel] = useState("");
  const [effortChoice, setEffortChoice] = useState("");
  const [submitting, setSubmitting] = useState(false);
  // Shown inside the dialog: a global banner renders behind the modal
  // backdrop, so a failed launch (bad model name, missing folder…) looked
  // like an unresponsive Start button. Fields are kept for the retry.
  const [error, setError] = useState<string | null>(null);
  const promptRef = useRef<HTMLTextAreaElement | null>(null);
  // Research is built on branching follow-ups, which require the adapter's
  // native fork command; offering a non-forkable adapter here would only be
  // discovered when the first follow-up fails after a completed root run.
  const adapters = useMemo(
    () => allAdapters.filter((candidate) => candidate.supportsFork),
    [allAdapters],
  );

  useEffect(() => {
    if (!open) {
      return;
    }
    setPrompt("");
    setModelChoice(null);
    setCustomModel("");
    setEffortChoice("");
    setError(null);
    setAdapter(adapters.find((candidate) => candidate.default)?.id ?? adapters[0]?.id ?? "");
  }, [open]);

  useEffect(() => {
    if (!open || adapters.some((candidate) => candidate.id === adapter)) {
      return;
    }
    setAdapter(adapters.find((candidate) => candidate.default)?.id ?? adapters[0]?.id ?? "");
  }, [adapter, adapters, open]);

  // Grow the textarea to fit its content, like the Home launcher: multi-line
  // prompts expand the composer until the CSS max-height caps it.
  const growPromptInput = useCallback(() => {
    const textarea = promptRef.current;
    if (!textarea) {
      return;
    }
    textarea.style.height = "auto";
    textarea.style.height = `${textarea.scrollHeight}px`;
  }, []);
  useLayoutEffect(() => {
    if (open) {
      growPromptInput();
    }
  }, [growPromptInput, open]);

  if (!open) {
    return null;
  }

  // A stale choice (left over from another adapter) silently falls back to the
  // adapter's first preset, so the trigger always shows what will launch.
  const modelPresets = modelPresetsFor(adapter);
  const selectedModel =
    modelChoice && modelPresets.includes(modelChoice) ? modelChoice : modelPresets[0];
  const resolvedModel =
    selectedModel === CUSTOM_MODEL ? customModel.trim() || null : selectedModel;
  // Same stale-choice contract as the model picker: a level left over from
  // another adapter or model silently falls back to the default, so the
  // trigger always shows what will launch.
  const effortOptions = effortOptionsFor(adapter, selectedModel);
  const selectedEffort =
    effortOptions && effortOptions.some((option) => option.value === effortChoice)
      ? effortChoice
      : "";
  const resolvedEffort = selectedEffort || null;

  async function submit() {
    if (!prompt.trim() || !adapter || submitting) {
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await onCreate({
        prompt: prompt.trim(),
        adapter,
        model: resolvedModel,
        effort: resolvedEffort,
        workspaceId,
      });
      onClose();
    } catch (err) {
      // Surfaced here, where the user is looking; the dialog stays open with
      // every field intact for the retry.
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  }

  const adapterOptions: LauncherSelectOption[] = adapters.map((candidate) => ({
    value: candidate.id,
    label: candidate.label,
    iconSrc: ADAPTER_ICON_BY_ID[candidate.id],
    iconClassName: adapterIconClassName(candidate.id),
  }));

  const launcher = (
    <form
      className="command-launcher new-research-launcher"
      role={inline ? undefined : "dialog"}
      aria-modal={inline ? undefined : true}
      aria-label="New research"
      onKeyDown={(event) => {
        if (!inline && event.key === "Escape" && !submitting) {
          onClose();
        }
      }}
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <div className="new-research-composer">
        <textarea
          ref={promptRef}
          autoFocus
          className="command-launcher-input"
          rows={2}
          value={prompt}
          placeholder="What would you like to investigate?"
          onChange={(event) => {
            setPrompt(event.currentTarget.value);
            growPromptInput();
          }}
          onKeyDown={(event) => {
            if (isComposerSubmitShortcut(event, requireCmdEnterToSend)) {
              event.preventDefault();
              void submit();
            }
          }}
        />
        <div className="command-launcher-overlay">
          <div className="command-launcher-overlay-group">
            <div className="command-launcher-options new-research-model-controls">
              <LauncherSelect
                value={selectedModel}
                options={modelPresets.map((preset) => ({ value: preset, label: preset }))}
                ariaLabel="Model"
                onChange={setModelChoice}
              />
              {selectedModel === CUSTOM_MODEL ? (
                <input
                  type="text"
                  value={customModel}
                  placeholder="Model name"
                  aria-label="Custom model"
                  onChange={(event) => setCustomModel(event.currentTarget.value)}
                />
              ) : null}
              {effortOptions ? (
                <LauncherSelect
                  value={selectedEffort}
                  options={effortOptions}
                  ariaLabel="Reasoning effort"
                  onChange={setEffortChoice}
                />
              ) : null}
            </div>
          </div>
          <div className="command-launcher-controls">
            <div className="command-launcher-adapter-select">
              <LauncherSelect
                value={adapter}
                options={adapterOptions}
                ariaLabel="Agent"
                onChange={setAdapter}
              />
            </div>
            <button
              type="submit"
              className="control-button command-launcher-send new-research-send"
              disabled={!prompt.trim() || !adapter || submitting}
              aria-label={submitting ? "Starting research" : "Start research"}
              title={submitting ? "Starting research" : "Start research"}
            >
              <ComposerSubmitShortcutGlyph
                requireCmdEnter={requireCmdEnterToSend}
                ariaHidden
              />
            </button>
          </div>
        </div>
      </div>
      {adapters.length === 0 || error ? (
        <div className="new-research-footer">
          {adapters.length === 0 ? (
            <p className="new-research-unavailable" role="alert">
              No installed agent supports research follow-ups.
            </p>
          ) : null}
          {error ? (
            <p className="new-research-error" role="alert">
              {error}
            </p>
          ) : null}
        </div>
      ) : null}
    </form>
  );

  if (inline) {
    return launcher;
  }

  return (
    <div
      className="confirm-dialog-backdrop new-research-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !submitting) {
          onClose();
        }
      }}
    >
      {launcher}
    </div>
  );
}
