import type { UserNotificationTone } from "../components/UserNotificationStack";

export interface NotificationLogEntry {
  id: string;
  title: string;
  body: string;
  tone: UserNotificationTone;
  paneId: string | null;
  createdAt: number;
  read: boolean;
}

function isTone(value: unknown): value is UserNotificationTone {
  return value === "info" || value === "success" || value === "warning" || value === "error";
}

function sanitizeEntry(value: unknown): NotificationLogEntry | null {
  if (typeof value !== "object" || value === null) {
    return null;
  }
  const raw = value as Record<string, unknown>;
  if (typeof raw.id !== "string" || !raw.id) {
    return null;
  }
  if (typeof raw.title !== "string" || typeof raw.body !== "string") {
    return null;
  }
  if (typeof raw.createdAt !== "number" || !Number.isFinite(raw.createdAt)) {
    return null;
  }
  return {
    id: raw.id,
    title: raw.title,
    body: raw.body,
    tone: isTone(raw.tone) ? raw.tone : "info",
    paneId: typeof raw.paneId === "string" && raw.paneId ? raw.paneId : null,
    createdAt: raw.createdAt,
    read: raw.read === true,
  };
}

/** Oldest first. Drops malformed rows and keeps the first occurrence of each id. */
export function normalizeNotificationLog(value: unknown): NotificationLogEntry[] {
  const entries = Array.isArray(value)
    ? value
    : value &&
        typeof value === "object" &&
        Array.isArray((value as { entries?: unknown }).entries)
      ? (value as { entries: unknown[] }).entries
      : [];
  const seen = new Set<string>();
  const out: NotificationLogEntry[] = [];
  for (const item of entries) {
    const entry = sanitizeEntry(item);
    if (!entry || seen.has(entry.id)) {
      continue;
    }
    seen.add(entry.id);
    out.push(entry);
  }
  return out;
}

export function notificationLogHasUnread(entries: NotificationLogEntry[]): boolean {
  return entries.some((entry) => !entry.read);
}
