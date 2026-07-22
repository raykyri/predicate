import assert from "node:assert/strict";
import test from "node:test";
import {
  stripTaggedInstructionBlocks,
  stripTaggedInstructionBlocksForPreview,
  stripTaggedUserInstructionBlocks,
} from "../src/lib/taggedInstructions";

test("removes embedded tagged instruction blocks from copied user messages", () => {
  const message = [
    "Please fix the copy action.",
    "",
    "<system-reminder>",
    "Do not include this injected instruction.",
    "</system-reminder>",
    "",
    "Keep this second paragraph.",
  ].join("\n");

  const copied = stripTaggedUserInstructionBlocks(message);

  assert.equal(copied.includes("<system-reminder>"), false);
  assert.equal(copied.includes("Do not include this injected instruction."), false);
  assert.equal(copied.includes("Please fix the copy action."), true);
  assert.equal(copied.includes("Keep this second paragraph."), true);
});

test("removes nested and consecutive tagged instruction blocks", () => {
  const message = [
    "<environment_context>",
    "<cwd>/private/project</cwd>",
    "</environment_context>",
    "<permissions>",
    "secret policy",
    "</permissions>",
    "User-authored message",
  ].join("\n");

  assert.equal(stripTaggedUserInstructionBlocks(message), "\nUser-authored message");
});

test("preserves inline tag examples that are part of user-authored prose", () => {
  const message = "Explain how <strong>important</strong> is rendered.";

  assert.equal(stripTaggedUserInstructionBlocks(message), message);
});

test("user-message stripping preserves fenced and indented code", () => {
  const message = [
    "Please review this hook file:",
    "",
    "```xml",
    "<system-reminder>",
    "Literal fenced XML the user pasted.",
    "</system-reminder>",
    "```",
    "",
    "    <config>",
    "    Literal indented XML.",
    "    </config>",
    "",
    "<system-reminder>",
    "Actually injected instructions.",
    "</system-reminder>",
    "",
    "What does it do?",
  ].join("\n");

  const copied = stripTaggedUserInstructionBlocks(message);

  assert.equal(copied.includes("Literal fenced XML the user pasted."), true);
  assert.equal(copied.includes("Literal indented XML."), true);
  assert.equal(copied.includes("Actually injected instructions."), false);
  assert.equal(copied.includes("What does it do?"), true);
});

test("preview stripping drops indented slash-command marker blocks", () => {
  const message = ["<command-message>copy</command-message>", "        <command-args></command-args>"].join(
    "\n",
  );

  assert.equal(stripTaggedInstructionBlocksForPreview(message).trim(), "");
});

test("preview stripping keeps real prose after a marker block", () => {
  const message = [
    "<command-name>run</command-name>",
    "        <command-args>--fast</command-args>",
    "Deploy the staging build.",
  ].join("\n");

  assert.equal(
    stripTaggedInstructionBlocksForPreview(message).trim(),
    "Deploy the staging build.",
  );
});

test("generic tagged-block stripping preserves preceding Markdown headings", () => {
  const message = [
    "# Visible answer",
    "",
    "<system-reminder>",
    "Hidden instructions.",
    "</system-reminder>",
    "",
    "Keep this conclusion.",
  ].join("\n");

  assert.equal(
    stripTaggedInstructionBlocks(message),
    "# Visible answer\n\n\n\nKeep this conclusion.",
  );
});

test("generic tagged-block stripping preserves fenced and indented code", () => {
  const message = [
    "Examples:",
    "",
    "```xml",
    "<system-reminder>",
    "Literal fenced XML.",
    "</system-reminder>",
    "```",
    "",
    "    <user-instructions>",
    "    Literal indented XML.",
    "    </user-instructions>",
    "",
    "<system-reminder>",
    "Hidden instructions.",
    "</system-reminder>",
    "",
    "Visible conclusion.",
  ].join("\n");

  assert.equal(
    stripTaggedInstructionBlocks(message),
    [
      "Examples:",
      "",
      "```xml",
      "<system-reminder>",
      "Literal fenced XML.",
      "</system-reminder>",
      "```",
      "",
      "    <user-instructions>",
      "    Literal indented XML.",
      "    </user-instructions>",
      "",
      "",
      "",
      "Visible conclusion.",
    ].join("\n"),
  );
});

// The backend wraps the custom research launch instruction (the settings
// dialog's "Research instructions" text) in a <research-instructions> tagged
// block — leading normally, trailing when the prompt begins with a slash
// command. Both forms must strip back to the user's own words in the
// user-message display/copy path and in previews, like any other
// qmux-injected instruction block.
test("strips the research launch instruction block the backend prepends", () => {
  const sent = [
    "<research-instructions>",
    "Answer concisely, in a few short paragraphs.",
    "</research-instructions>",
    "",
    "Why is the sky blue?",
  ].join("\n");

  assert.equal(stripTaggedUserInstructionBlocks(sent).trim(), "Why is the sky blue?");
  assert.equal(stripTaggedInstructionBlocksForPreview(sent).trim(), "Why is the sky blue?");
});

test("strips the research launch instruction block appended after slash commands", () => {
  const sent = [
    "/qmux:deep-research Why is the sky blue?",
    "",
    "<research-instructions>",
    "Answer concisely.",
    "</research-instructions>",
  ].join("\n");

  assert.equal(
    stripTaggedUserInstructionBlocks(sent).trim(),
    "/qmux:deep-research Why is the sky blue?",
  );
  assert.equal(
    stripTaggedInstructionBlocksForPreview(sent).trim(),
    "/qmux:deep-research Why is the sky blue?",
  );
});
