export interface TaggedUserInstructionDetails {
  label: string;
  tags: string[];
}

export function stripTaggedUserInstructionBlocks(text: string): string {
  const unwrapped = unwrapUserQueryEnvelope(text);
  const leading = stripLeadingTaggedInstructionBlocks(unwrapped);
  // Protect fenced/indented code the same way the assistant path does: a
  // user message quoting XML-ish tags inside a code block (a pasted hook
  // file, a config sample) is content, not an injected instruction block,
  // and it flows into clipboard copies and published transcripts.
  const stripped = stripInlineTaggedInstructionBlocks(
    leading.text,
    markdownCodeRanges(leading.text),
  );
  return leading.removed || stripped.removed ? stripped.text : unwrapped;
}

// Like stripTaggedUserInstructionBlocks, but without protecting fenced/indented
// code. The home rail cards are compact previews, so any tagged block should be
// dropped — including a slash command's indented <command-args> block, which the
// copy-safe filter leaves in place because indentation reads as a code block.
export function stripTaggedInstructionBlocksForPreview(text: string): string {
  const unwrapped = unwrapUserQueryEnvelope(text);
  const leading = stripLeadingTaggedInstructionBlocks(unwrapped);
  const stripped = stripInlineTaggedInstructionBlocks(leading.text);
  return leading.removed || stripped.removed ? stripped.text : unwrapped;
}

// Remove line-leading tagged blocks without treating a preceding Markdown
// heading as an injected-label prefix. Assistant answers often use headings as
// real content, so their copy path must preserve those while cutting out any
// embedded system/instruction blocks.
export function stripTaggedInstructionBlocks(text: string): string {
  const stripped = stripInlineTaggedInstructionBlocks(text, markdownCodeRanges(text));
  return stripped.removed ? stripped.text : text;
}

export function taggedUserInstructionDetails(text: string): TaggedUserInstructionDetails | null {
  const unwrapped = unwrapUserQueryEnvelope(text);
  const contentStart = taggedInstructionContentStart(unwrapped);
  if (contentStart === null) {
    return null;
  }

  const tags = parseTaggedInstructionSequence(unwrapped.slice(contentStart));
  if (tags === null) {
    return null;
  }

  return {
    label: taggedInstructionLabel(tags),
    tags,
  };
}

// Cursor Agent (and Grok) wrap the user's words in a `<user_query>` envelope
// and may prefix a `<timestamp>` metadata chip. Those tags are not injected
// instructions — leaving them in place makes the detector collapse the whole
// turn into a `<timestamp> <user_query>` chip and never show the prompt.
export function unwrapUserQueryEnvelope(text: string): string {
  if (!text.includes("<timestamp>") && !text.includes("<user_query>")) {
    return text;
  }

  const withoutTimestamps = stripNamedTaggedBlocks(text, "<timestamp>", "</timestamp>");
  const unwrapped = unwrapNamedTaggedBlock(withoutTimestamps, "<user_query>", "</user_query>");
  return unwrapped.trim();
}

function stripNamedTaggedBlocks(text: string, open: string, close: string): string {
  let result = "";
  let index = 0;
  while (index < text.length) {
    const start = text.indexOf(open, index);
    if (start === -1) {
      result += text.slice(index);
      break;
    }
    const contentStart = start + open.length;
    const closeAt = text.indexOf(close, contentStart);
    if (closeAt === -1) {
      result += text.slice(index);
      break;
    }
    result += text.slice(index, start);
    index = closeAt + close.length;
  }
  return result;
}

function unwrapNamedTaggedBlock(text: string, open: string, close: string): string {
  const start = text.indexOf(open);
  if (start === -1) {
    return text;
  }
  const contentStart = start + open.length;
  const closeAt = text.indexOf(close, contentStart);
  if (closeAt === -1) {
    return text;
  }
  const inner = text.slice(contentStart, closeAt).replace(/^\r?\n/, "").replace(/\r?\n$/, "");
  return text.slice(0, start) + inner + text.slice(closeAt + close.length);
}

function parseTaggedInstructionSequence(content: string): string[] | null {
  const result = parseTaggedInstructionSequenceFrom(content, 0);
  return result !== null && result.tags.length > 0 ? result.tags : null;
}

function stripLeadingTaggedInstructionBlocks(text: string): { text: string; removed: boolean } {
  let current = text;
  let removed = false;

  while (true) {
    const contentStart = taggedInstructionContentStart(current);
    if (contentStart === null) {
      return { text: removed ? "" : current, removed };
    }

    const block = parseTaggedInstructionBlockAt(current, contentStart);
    if (block === null) {
      return { text: current, removed };
    }

    current = current.slice(block.end);
    removed = true;
  }
}

interface TextRange {
  start: number;
  end: number;
}

function stripInlineTaggedInstructionBlocks(
  text: string,
  protectedRanges: TextRange[] = [],
): { text: string; removed: boolean } {
  let result = "";
  let index = 0;
  let removed = false;

  while (index < text.length) {
    const nextTag = text.indexOf("<", index);
    if (nextTag === -1) {
      result += text.slice(index);
      break;
    }

    result += text.slice(index, nextTag);
    const block = !rangeContains(protectedRanges, nextTag) && isLineContentStart(text, nextTag)
      ? parseTaggedInstructionBlockAt(text, nextTag)
      : null;
    if (block === null) {
      result += text[nextTag];
      index = nextTag + 1;
      continue;
    }

    removed = true;
    index = block.end;
  }

  return { text: result, removed };
}

function markdownCodeRanges(text: string): TextRange[] {
  const ranges: TextRange[] = [];
  let fence: { marker: "`" | "~"; length: number } | null = null;
  let lineStart = 0;

  while (lineStart <= text.length) {
    const newline = text.indexOf("\n", lineStart);
    const lineEnd = newline === -1 ? text.length : newline;
    const line = stripTrailingCarriageReturn(text.slice(lineStart, lineEnd));
    const fenceRun = markdownFenceRun(line);
    const lineRange = { start: lineStart, end: newline === -1 ? lineEnd : lineEnd + 1 };

    if (fence) {
      ranges.push(lineRange);
      if (
        fenceRun?.marker === fence.marker &&
        fenceRun.length >= fence.length &&
        trimHorizontalWhitespace(line.slice(fenceRun.end)).length === 0
      ) {
        fence = null;
      }
    } else if (fenceRun) {
      ranges.push(lineRange);
      fence = { marker: fenceRun.marker, length: fenceRun.length };
    } else if (line.startsWith("\t") || line.startsWith("    ")) {
      ranges.push(lineRange);
    }

    if (newline === -1) {
      break;
    }
    lineStart = newline + 1;
  }

  return ranges;
}

function markdownFenceRun(
  line: string,
): { marker: "`" | "~"; length: number; end: number } | null {
  let start = 0;
  while (start < 3 && line[start] === " ") {
    start += 1;
  }
  const marker = line[start];
  if (marker !== "`" && marker !== "~") {
    return null;
  }
  let end = start;
  while (line[end] === marker) {
    end += 1;
  }
  const length = end - start;
  return length >= 3 ? { marker, length, end } : null;
}

function rangeContains(ranges: TextRange[], index: number) {
  return ranges.some((range) => index >= range.start && index < range.end);
}

function parseTaggedInstructionSequenceFrom(
  content: string,
  start: number,
): { tags: string[] } | null {
  const index = skipWhitespace(content, start);
  if (index >= content.length) {
    return { tags: [] };
  }

  const openingTag = parseOpeningTagAt(content, index);
  if (openingTag === null) {
    return null;
  }

  const closing = `</${openingTag.tag}>`;
  let closingStart = openingTag.end;
  while (closingStart < content.length) {
    closingStart = content.indexOf(closing, closingStart);
    if (closingStart === -1) {
      return null;
    }

    const rest = parseTaggedInstructionSequenceFrom(content, closingStart + closing.length);
    if (rest !== null) {
      return { tags: [openingTag.tag, ...rest.tags] };
    }
    closingStart += closing.length;
  }

  return null;
}

function parseTaggedInstructionBlockAt(
  content: string,
  start: number,
): { tag: string; end: number } | null {
  const openingTag = parseOpeningTagAt(content, start);
  if (openingTag === null) {
    return null;
  }

  const opening = `<${openingTag.tag}>`;
  const closing = `</${openingTag.tag}>`;
  let depth = 1;
  let index = openingTag.end;

  while (index < content.length) {
    const nextOpening = content.indexOf(opening, index);
    const nextClosing = content.indexOf(closing, index);
    if (nextClosing === -1) {
      return null;
    }

    if (nextOpening !== -1 && nextOpening < nextClosing) {
      depth += 1;
      index = nextOpening + opening.length;
      continue;
    }

    depth -= 1;
    index = nextClosing + closing.length;
    if (depth === 0) {
      return { tag: openingTag.tag, end: index };
    }
  }

  return null;
}

function isLineContentStart(value: string, index: number) {
  const previousLineEnd = value.lastIndexOf("\n", index - 1);
  const lineStart = previousLineEnd === -1 ? 0 : previousLineEnd + 1;
  for (let cursor = lineStart; cursor < index; cursor += 1) {
    if (!isHorizontalWhitespace(value[cursor])) {
      return false;
    }
  }
  return true;
}

function taggedInstructionLabel(tags: string[]) {
  const uniqueTags: string[] = [];
  const seen = new Set<string>();
  for (const tag of tags) {
    if (seen.has(tag)) {
      continue;
    }
    seen.add(tag);
    uniqueTags.push(tag);
  }
  return uniqueTags.map((tag) => `<${tag}>`).join(" ");
}

function taggedInstructionContentStart(text: string) {
  let start = 0;
  while (start < text.length) {
    const lineEnd = text.indexOf("\n", start);
    const end = lineEnd === -1 ? text.length : lineEnd;
    const line = stripTrailingCarriageReturn(text.slice(start, end));
    if (!isTaggedInstructionPrefixLine(line)) {
      return start;
    }
    if (lineEnd === -1) {
      return null;
    }
    start = lineEnd + 1;
  }
  return null;
}

function isTaggedInstructionPrefixLine(line: string) {
  return line.startsWith("# ") || trimHorizontalWhitespace(line).length === 0;
}

function stripTrailingCarriageReturn(value: string) {
  return value.endsWith("\r") ? value.slice(0, -1) : value;
}

function trimHorizontalWhitespace(value: string) {
  let start = 0;
  let end = value.length;
  while (start < end && isHorizontalWhitespace(value[start])) {
    start += 1;
  }
  while (end > start && isHorizontalWhitespace(value[end - 1])) {
    end -= 1;
  }
  return value.slice(start, end);
}

function isHorizontalWhitespace(char: string) {
  return char !== "\n" && char !== "\r" && char.trim() === "";
}

function skipWhitespace(value: string, index: number) {
  while (index < value.length && value[index].trim() === "") {
    index += 1;
  }
  return index;
}

function parseOpeningTagAt(value: string, start: number): { tag: string; end: number } | null {
  if (value[start] !== "<") {
    return null;
  }
  const openingEnd = value.indexOf(">", start + 1);
  if (openingEnd === -1) {
    return null;
  }
  const opening = value.slice(start + 1, openingEnd);
  const tag =
    opening === 'qmux_instruction source="agent_driver"' ? "qmux_instruction" : opening;
  return isInstructionTagName(tag) ? { tag, end: openingEnd + 1 } : null;
}

function isInstructionTagName(tag: string) {
  if (tag.length === 0) {
    return false;
  }
  for (const char of tag) {
    const code = char.charCodeAt(0);
    const isDigit = code >= 48 && code <= 57;
    const isUpper = code >= 65 && code <= 90;
    const isLower = code >= 97 && code <= 122;
    if (!isDigit && !isUpper && !isLower && char !== "_" && char !== "-") {
      return false;
    }
  }
  return true;
}
