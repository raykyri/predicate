import { absoluteLocalFilePath } from "./links";

const DEVIN_REF_TAG_PATTERN = /<ref_(file|snippet)\s+([^>]*?)\s*\/>/gu;
const XML_ATTRIBUTE_PATTERN = /([A-Za-z_][\w:-]*)\s*=\s*(?:"([^"]*)"|'([^']*)')/gu;
const LINE_RANGE_PATTERN = /^\d+(-\d+)?$/u;
const PATH_CONTROL_CHARACTER_PATTERN = /[\u0000-\u001f\u007f]/u;

/** Devin's self-closing citation tags. The right pane otherwise shows them as
 * literal XML; rewrite them to ordinary markdown file links (with a line range
 * on snippets) so they use the same click path as `[file](/abs/path:12)`. */
export function rewriteDevinFileRefs(source: string): string {
  if (!source.includes("<ref_")) {
    return source;
  }
  DEVIN_REF_TAG_PATTERN.lastIndex = 0;
  return source.replace(
    DEVIN_REF_TAG_PATTERN,
    (match, kind: string, rawAttributes: string) => {
      const attributes = xmlAttributes(rawAttributes);
      const file = attributes.file?.trim() ?? "";
      if (!file || PATH_CONTROL_CHARACTER_PATTERN.test(file)) {
        return match;
      }
      const path = absoluteLocalFilePath(file);
      if (!path) {
        return match;
      }
      const basename = fileBasename(path);
      if (kind === "file") {
        return markdownFileLink(basename, path);
      }
      const lines = attributes.lines?.trim() ?? "";
      if (!LINE_RANGE_PATTERN.test(lines)) {
        return match;
      }
      return markdownFileLink(`${basename}:${lines}`, `${path}:${lines}`);
    },
  );
}

function xmlAttributes(raw: string): Record<string, string> {
  const attributes: Record<string, string> = {};
  XML_ATTRIBUTE_PATTERN.lastIndex = 0;
  for (const match of raw.matchAll(XML_ATTRIBUTE_PATTERN)) {
    const name = match[1];
    const value = match[2] ?? match[3];
    if (name && value !== undefined && attributes[name] === undefined) {
      attributes[name] = value;
    }
  }
  return attributes;
}

function fileBasename(path: string): string {
  const name = path.replace(/\\/gu, "/").split("/").pop();
  return name && name.length > 0 ? name : path;
}

function markdownFileLink(label: string, destination: string): string {
  const escapedLabel = label.replaceAll("\\", "\\\\").replaceAll("]", "\\]");
  if (/[\s()]/.test(destination) && !/[<>]/.test(destination)) {
    return `[${escapedLabel}](<${destination}>)`;
  }
  return `[${escapedLabel}](${destination})`;
}
