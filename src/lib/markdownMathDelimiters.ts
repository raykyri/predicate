interface MarkdownFence {
  marker: "`" | "~";
  length: number;
}

function markerRunAtLineStart(line: string): { marker: "`" | "~"; length: number } | null {
  const match = /^ {0,3}(`{3,}|~{3,})/.exec(line);
  const run = match?.[1];
  if (!run) {
    return null;
  }
  return { marker: run[0] as "`" | "~", length: run.length };
}

function closesFence(line: string, fence: MarkdownFence): boolean {
  const indentation = /^ {0,3}/.exec(line)?.[0].length ?? 0;
  let cursor = indentation;
  while (line[cursor] === fence.marker) {
    cursor += 1;
  }
  return cursor - indentation >= fence.length && /^[\t \r]*$/.test(line.slice(cursor));
}

function backtickRunLength(source: string, start: number): number {
  let end = start;
  while (source[end] === "`") {
    end += 1;
  }
  return end - start;
}

function matchingBacktickRunEnd(source: string, start: number, length: number): number | null {
  let cursor = start;
  while (cursor < source.length) {
    const candidate = source.indexOf("`", cursor);
    if (candidate === -1) {
      return null;
    }
    const candidateLength = backtickRunLength(source, candidate);
    if (candidateLength === length) {
      return candidate + length;
    }
    cursor = candidate + candidateLength;
  }
  return null;
}

function isUnescapedDelimiter(source: string, index: number, delimiter: string): boolean {
  return source.startsWith(delimiter, index) && (index === 0 || source[index - 1] !== "\\");
}

function closingDelimiterIndex(source: string, start: number, delimiter: string): number | null {
  let cursor = start;
  while (cursor < source.length) {
    const candidate = source.indexOf(delimiter, cursor);
    if (candidate === -1) {
      return null;
    }
    if (isUnescapedDelimiter(source, candidate, delimiter)) {
      return candidate;
    }
    cursor = candidate + delimiter.length;
  }
  return null;
}

/**
 * Convert LaTeX's alternate math delimiters to the dollar delimiters consumed
 * by remark-math. The scan runs before Markdown parsing, so TeX punctuation is
 * protected from emphasis/link parsing, but skips Markdown code spans, fenced
 * code blocks, and indented code lines where the delimiters must stay literal.
 * Unmatched delimiters are also left untouched.
 */
export function normalizeLatexMathDelimiters(source: string): string {
  if (!source.includes("\\[") && !source.includes("\\(")) {
    return source;
  }

  let output = "";
  let cursor = 0;
  let fence: MarkdownFence | null = null;

  while (cursor < source.length) {
    const lineStart = cursor === 0 || source[cursor - 1] === "\n";
    if (lineStart) {
      const newline = source.indexOf("\n", cursor);
      const lineEnd = newline === -1 ? source.length : newline;
      const line = source.slice(cursor, lineEnd);

      if (fence) {
        output += source.slice(cursor, newline === -1 ? lineEnd : lineEnd + 1);
        if (closesFence(line, fence)) {
          fence = null;
        }
        cursor = newline === -1 ? lineEnd : lineEnd + 1;
        continue;
      }

      const openingFence = markerRunAtLineStart(line);
      if (openingFence) {
        fence = openingFence;
        output += source.slice(cursor, newline === -1 ? lineEnd : lineEnd + 1);
        cursor = newline === -1 ? lineEnd : lineEnd + 1;
        continue;
      }

      // Conservatively preserve indented-code lines. This avoids interpreting
      // documentation examples as math without needing to duplicate all of
      // CommonMark's block-continuation rules.
      if (line.startsWith("    ") || line.startsWith("\t")) {
        output += source.slice(cursor, newline === -1 ? lineEnd : lineEnd + 1);
        cursor = newline === -1 ? lineEnd : lineEnd + 1;
        continue;
      }
    }

    if (source[cursor] === "`") {
      const runLength = backtickRunLength(source, cursor);
      const spanEnd = matchingBacktickRunEnd(source, cursor + runLength, runLength);
      if (spanEnd !== null) {
        output += source.slice(cursor, spanEnd);
        cursor = spanEnd;
        continue;
      }
      output += source.slice(cursor, cursor + runLength);
      cursor += runLength;
      continue;
    }

    const delimiter = isUnescapedDelimiter(source, cursor, "\\[")
      ? { close: "\\]", replacement: "$$" }
      : isUnescapedDelimiter(source, cursor, "\\(")
        ? { close: "\\)", replacement: "$" }
        : null;
    if (delimiter) {
      const close = closingDelimiterIndex(source, cursor + 2, delimiter.close);
      if (close !== null) {
        output += delimiter.replacement;
        output += source.slice(cursor + 2, close);
        output += delimiter.replacement;
        cursor = close + 2;
        continue;
      }
    }

    output += source[cursor];
    cursor += 1;
  }

  return output;
}
