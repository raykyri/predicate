// Client-side mirrors of the backend's research-document rules
// (src-tauri/src/research.rs), so the composer can validate and derive a
// title preview without a round trip. The backend remains authoritative, and
// both sides pin the shared cases in tests (tests/researchDocuments.test.ts,
// research.rs document_* tests) so drift is caught.

export const RESEARCH_DOCUMENT_WORD_LIMIT = 10_000;
export const RESEARCH_DOCUMENT_BYTE_LIMIT = 10 * 1024 * 1024;

/** Finder and the other native file managers provide absolute paths for file
 * drops. Keep this deliberately narrow: document imports accept the two
 * conventional Markdown extensions, while similarly named files remain inert. */
export function isMarkdownDocumentPath(path: string): boolean {
  return /\.(?:md|markdown)$/iu.test(path);
}

/** A dropped claude.ai/ChatGPT export: the archive .zip, or an extracted
 * conversations.json. The backend re-validates on staging — this only decides
 * whether a drop opens the conversation-import dialog. */
export function isConversationArchivePath(path: string): boolean {
  return /\.(?:zip|json)$/iu.test(path);
}

/** A dropped Claude Code session transcript (.jsonl). */
export function isConversationTranscriptPath(path: string): boolean {
  return /\.jsonl$/iu.test(path);
}

// Rust's split_whitespace uses the Unicode White_Space property. Spell the
// small, stable set out so counting can scan UTF-16 code units without regex
// match objects; all White_Space code points are in the BMP. Notably, NEL is
// whitespace and FEFF is not, unlike JavaScript's \s.
function isDocumentWhitespace(codeUnit: number): boolean {
  return (
    (codeUnit >= 0x0009 && codeUnit <= 0x000d) ||
    codeUnit === 0x0020 ||
    codeUnit === 0x0085 ||
    codeUnit === 0x00a0 ||
    codeUnit === 0x1680 ||
    (codeUnit >= 0x2000 && codeUnit <= 0x200a) ||
    codeUnit === 0x2028 ||
    codeUnit === 0x2029 ||
    codeUnit === 0x202f ||
    codeUnit === 0x205f ||
    codeUnit === 0x3000
  );
}

/** Trims leading whitespace using the same Unicode White_Space set as the
 * backend, rather than JavaScript's `\s`/`trimStart` — the two disagree on
 * U+FEFF (JS whitespace, White_Space is not) and U+0085 (the reverse), which
 * is exactly what makes the title preview drift from the persisted title on a
 * BOM-prefixed import. */
function trimDocumentWhitespaceStart(value: string): string {
  let index = 0;
  while (index < value.length && isDocumentWhitespace(value.charCodeAt(index))) {
    index += 1;
  }
  return value.slice(index);
}

export class ResearchDocumentWordLimitExceeded extends Error {
  readonly limit: number;
  readonly count: number;

  constructor(limit: number, count: number) {
    super(`Documents are limited to ${limit} words for now`);
    this.name = "ResearchDocumentWordLimitExceeded";
    this.limit = limit;
    this.count = count;
  }
}

/** Counts without retaining the matches. When a limit is supplied, stop at
 * the first word over it so a dense 10 MB import cannot monopolize or exhaust
 * the renderer merely to establish that submission is disabled. */
export function countResearchDocumentWords(
  markdown: string,
  limit = Number.POSITIVE_INFINITY,
): number {
  let count = 0;
  let insideWord = false;
  for (let index = 0; index < markdown.length; index += 1) {
    if (isDocumentWhitespace(markdown.charCodeAt(index))) {
      insideWord = false;
    } else if (!insideWord) {
      insideWord = true;
      count += 1;
      if (count > limit) {
        throw new ResearchDocumentWordLimitExceeded(limit, count);
      }
    }
  }
  return count;
}

const DOCUMENT_TITLE_MAX_CHARS = 72;

/** Title for a document without an explicit one: the first line with content,
 * ATX heading markers stripped, truncated like a prompt-derived title. Scans
 * line by line rather than splitting the whole document — this feeds a
 * composer placeholder recomputed as the user types. */
export function deriveResearchDocumentTitle(markdown: string): string {
  let start = 0;
  while (start <= markdown.length) {
    const newline = markdown.indexOf("\n", start);
    const rawLine = newline === -1 ? markdown.slice(start) : markdown.slice(start, newline);
    let line = trimDocumentWhitespaceStart(rawLine);
    let markerEnd = 0;
    while (line.charCodeAt(markerEnd) === 35) {
      markerEnd += 1;
    }
    line = trimDocumentWhitespaceStart(line.slice(markerEnd));
    if (line) {
      // Normalize only as far as the title can display. The old split/spread
      // path materialized a full-line token array and then a full code-point
      // array, which made a permitted single-line 10 MB file hazardous.
      const characters: string[] = [];
      let pendingSpace = false;
      for (const character of line) {
        if (isDocumentWhitespace(character.charCodeAt(0))) {
          pendingSpace = characters.length > 0;
          continue;
        }
        if (pendingSpace) {
          characters.push(" ");
          pendingSpace = false;
        }
        characters.push(character);
        if (characters.length > DOCUMENT_TITLE_MAX_CHARS) {
          return `${characters.slice(0, DOCUMENT_TITLE_MAX_CHARS).join("").trimEnd()}…`;
        }
      }
      if (characters.length > 0) {
        return characters.join("");
      }
    }
    if (newline === -1) {
      break;
    }
    start = newline + 1;
  }
  return "Untitled document";
}
