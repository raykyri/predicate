export type ComposerSlashCommandName = "fork" | "worktree";

/** What a command does when submitted. Fork-only fields (useWorktree) are
 * ignored for other kinds. */
export type ComposerSlashCommandKind = "fork";

export interface ComposerSlashCommand {
  name: ComposerSlashCommandName;
  token: `/${ComposerSlashCommandName}`;
  description: string;
  kind: ComposerSlashCommandKind;
  useWorktree: boolean;
}

export const COMPOSER_SLASH_COMMANDS: readonly ComposerSlashCommand[] = [
  {
    name: "fork",
    token: "/fork",
    description: "Fork this session and send the following message",
    kind: "fork",
    useWorktree: false,
  },
  {
    name: "worktree",
    token: "/worktree",
    description: "Fork in a new worktree and send the following message",
    kind: "fork",
    useWorktree: true,
  },
];

export type ParsedComposerSlashCommand =
  | { kind: "none" }
  | { kind: "incomplete"; command: ComposerSlashCommand }
  | { kind: "ready"; command: ComposerSlashCommand; prompt: string };

/** Commands are recognized only at byte zero and only when their exact token is
 * followed by a space or tab. Unknown slash commands remain ordinary agent input. */
export function parseComposerSlashCommand(
  value: string,
): ParsedComposerSlashCommand {
  if (!value.startsWith("/")) {
    return { kind: "none" };
  }

  for (const command of COMPOSER_SLASH_COMMANDS) {
    if (value === command.token) {
      return { kind: "incomplete", command };
    }
    if (!value.startsWith(command.token)) {
      continue;
    }
    const separator = value.charAt(command.token.length);
    if (separator !== " " && separator !== "\t") {
      continue;
    }
    const prompt = value.slice(command.token.length).trim();
    return prompt
      ? { kind: "ready", command, prompt }
      : { kind: "incomplete", command };
  }

  return { kind: "none" };
}

/** Returns prefix matches only while the entire draft is still the first slash
 * token. Once the user starts the message, the typeahead gets out of the way. */
export function matchingComposerSlashCommands(
  value: string,
): readonly ComposerSlashCommand[] {
  if (!/^\/[^\s]*$/.test(value)) {
    return [];
  }
  return COMPOSER_SLASH_COMMANDS.filter((command) => command.token.startsWith(value));
}

export function completeComposerSlashCommand(command: ComposerSlashCommand): string {
  return `${command.token} `;
}
