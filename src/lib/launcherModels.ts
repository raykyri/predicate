const CUSTOM_MODEL = "custom";

const MODEL_PRESETS_BY_ADAPTER: Record<string, string[]> = {
  claude: ["opus", "fable", "sonnet", CUSTOM_MODEL],
  codex: [
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    CUSTOM_MODEL,
  ],
};

export { CUSTOM_MODEL };

export function modelPresetsFor(adapter: string): string[] {
  return MODEL_PRESETS_BY_ADAPTER[adapter] ?? [CUSTOM_MODEL];
}

export function selectedModelPreset(adapter: string, choice: string | null | undefined): string {
  const presets = modelPresetsFor(adapter);
  return choice && presets.includes(choice) ? choice : presets[0];
}

export function nextModelPreset(adapter: string, choice: string | null | undefined): string {
  const presets = modelPresetsFor(adapter);
  const selected = selectedModelPreset(adapter, choice);
  const currentIndex = presets.indexOf(selected);
  return presets[(currentIndex + 1) % presets.length];
}

export function formatLauncherModelLabel(adapter: string, preset: string): string {
  if (preset === CUSTOM_MODEL) {
    return "Custom";
  }
  // Codex model ids are product identifiers, not prose. Preserve their exact
  // casing instead of turning `gpt-5.6-sol` into `Gpt-5.6-Sol`.
  if (adapter === "codex") {
    return preset;
  }
  return preset.replace(/[A-Za-z]+/g, (token) =>
    `${token.charAt(0).toUpperCase()}${token.slice(1).toLowerCase()}`,
  );
}
