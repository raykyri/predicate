// Claude and Codex can use different (and changing) tokenizers, so the UI
// deliberately reports an estimate rather than implying model-specific
// precision. Four ASCII characters per token is a useful average for prose and
// code; non-ASCII scripts tend to encode more densely in characters, so weight
// those code units more heavily. Sampling bounds the cost for large tool output
// while still accounting for its character mix.
const MAX_SAMPLED_CODE_UNITS = 2_048;

export function estimateTokenCount(text: string): number {
  if (text.length === 0) {
    return 0;
  }

  const sampleCount = Math.min(text.length, MAX_SAMPLED_CODE_UNITS);
  let tokenWeight = 0;
  for (let sampleIndex = 0; sampleIndex < sampleCount; sampleIndex += 1) {
    const textIndex = Math.floor(
      ((sampleIndex + 0.5) * text.length) / sampleCount,
    );
    const codeUnit = text.charCodeAt(textIndex);
    if (codeUnit <= 0x7f) {
      tokenWeight += 0.25;
    } else if (
      codeUnit <= 0x7ff ||
      (codeUnit >= 0xd800 && codeUnit <= 0xdfff)
    ) {
      // Count each half of an emoji's UTF-16 surrogate pair at half weight.
      tokenWeight += 0.5;
    } else {
      tokenWeight += 1;
    }
  }

  return Math.max(1, Math.round((tokenWeight * text.length) / sampleCount));
}

export function formatEstimatedTokenCount(text: string): string {
  const tokens = estimateTokenCount(text);
  if (tokens < 1_000) {
    return `~${tokens.toLocaleString()} tok`;
  }
  if (tokens < 100_000) {
    return `~${(tokens / 1_000).toFixed(1)}k tok`;
  }
  return `~${Math.round(tokens / 1_000).toLocaleString()}k tok`;
}
