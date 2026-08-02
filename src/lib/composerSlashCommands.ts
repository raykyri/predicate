export type ComposerSlashCommandName = "fork" | "worktree" | "btw";

/** What a command does when submitted. Fork-only fields (useWorktree) are
 * ignored for other kinds. */
export type ComposerSlashCommandKind = "fork" | "btw";

export interface ComposerSlashCommand {
  name: ComposerSlashCommandName;
  token: `/${ComposerSlashCommandName}`;
  description: string;
  kind: ComposerSlashCommandKind;
  useWorktree: boolean;
  rightPaneOnly?: boolean;
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
  {
    name: "btw",
    token: "/btw",
    description: "Fork this session below the current terminal and send immediately",
    kind: "btw",
    useWorktree: false,
    rightPaneOnly: true,
  },
];

export interface ComposerSlashCommandParseOptions {
  surface?: "rightPane" | "globalLauncher";
}

function commandsForSurface(options?: ComposerSlashCommandParseOptions) {
  return options?.surface === "globalLauncher"
    ? COMPOSER_SLASH_COMMANDS.filter((command) => !command.rightPaneOnly)
    : COMPOSER_SLASH_COMMANDS;
}

export type ParsedComposerSlashCommand =
  | { kind: "none" }
  | { kind: "incomplete"; command: ComposerSlashCommand }
  | { kind: "ready"; command: ComposerSlashCommand; prompt: string };

/** Commands are recognized only at byte zero and only when their exact token is
 * followed by a space or tab. Unknown slash commands remain ordinary agent input. */
export function parseComposerSlashCommand(
  value: string,
  options?: ComposerSlashCommandParseOptions,
): ParsedComposerSlashCommand {
  if (!value.startsWith("/")) {
    return { kind: "none" };
  }

  for (const command of commandsForSurface(options)) {
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
  options?: ComposerSlashCommandParseOptions,
): readonly ComposerSlashCommand[] {
  if (!/^\/[^\s]*$/.test(value)) {
    return [];
  }
  return commandsForSurface(options).filter((command) => command.token.startsWith(value));
}

export function completeComposerSlashCommand(command: ComposerSlashCommand): string {
  return `${command.token} `;
}
