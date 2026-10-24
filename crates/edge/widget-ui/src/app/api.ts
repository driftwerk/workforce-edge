// Owner-scoped conversation REST. The customer token rides `Authorization:
// Bearer`; every endpoint is bound server-side to the verified owner, so a
// guessed id only ever resolves to the caller's own data (404 otherwise).

import type { Attachment } from "./connection";
import type { TablePage } from "./rich/spec";

export interface ConversationSummary {
  conversation: string;
  title: string;
  updated_at: string;
  messages: number;
}

export interface TranscriptMessage {
  role: "user" | "assistant";
  content: string;
  at: string;
  /** Persisted id of an assistant message, so a restored transcript can still
   *  be rated; absent on user rows (and from servers predating it). */
  message_id?: string;
  /** Files the assistant produced on this turn, rehydrated from the transcript
   *  so download buttons survive a reload; absent when the turn produced none. */
  attachments?: Attachment[];
  /** The client-minted reference of the turn this assistant row answers, so a
   *  recovered answer can be matched to its question exactly. Absent on user
   *  rows, on answers saved before this existed, and from older servers — the
   *  positional fallback covers those. */
  reply_to?: string;
}

export interface TranscriptPage {
  messages: TranscriptMessage[];
  start: number;
  has_more: boolean;
}

function convUrl(base: string, agent: string, conversation?: string): string {
  const path = `/public/conversations/${encodeURIComponent(agent)}`;
  return base + (conversation ? `${path}/${encodeURIComponent(conversation)}` : path);
}

// Always bypass the HTTP cache: these responses are per-user and mutate as the
// customer chats, so a cached list/transcript would show stale conversations.
function get(url: string | URL, token: string): Promise<Response> {
  return fetch(url, {
    headers: { Authorization: `Bearer ${token}` },
    cache: "no-store",
  });
}

export async function listConversations(
  base: string,
  agent: string,
  token: string,
): Promise<ConversationSummary[]> {
  const res = await get(convUrl(base, agent), token);
  if (!res.ok) throw new Error(`conversation list failed (HTTP ${res.status})`);
  return res.json();
}

export async function getConversation(
  base: string,
  agent: string,
  token: string,
  conversation: string,
  opts: { limit?: number; before?: number } = {},
): Promise<TranscriptPage> {
  const url = new URL(convUrl(base, agent, conversation));
  if (opts.limit != null) url.searchParams.set("limit", String(opts.limit));
  if (opts.before != null) url.searchParams.set("before", String(opts.before));
  const res = await get(url, token);
  if (!res.ok) throw new Error(`transcript failed (HTTP ${res.status})`);
  return res.json();
}

export async function deleteConversation(
  base: string,
  agent: string,
  token: string,
  conversation: string,
): Promise<void> {
  const res = await fetch(convUrl(base, agent, conversation), {
    method: "DELETE",
    headers: { Authorization: `Bearer ${token}` },
    cache: "no-store",
  });
  if (!res.ok && res.status !== 204) throw new Error(`delete failed (HTTP ${res.status})`);
}

/** A thumbs rating on one assistant answer. `rating` is +1 (up) or -1 (down);
 *  `comment` is optional free text (the server caps it at 2000 chars). */
export interface FeedbackBody {
  message_id: string;
  rating: 1 | -1;
  comment?: string;
}

/** Rate an assistant answer. Owner-scoped server-side to the bearer token, so it
 *  only ever writes the caller's own conversation. The endpoint returns 204 on
 *  success (no body) and upserts, so re-rating just changes the vote. */
export async function postFeedback(
  base: string,
  agent: string,
  conversation: string,
  token: string,
  body: FeedbackBody,
): Promise<void> {
  const res = await fetch(`${convUrl(base, agent, conversation)}/feedback`, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
    cache: "no-store",
    body: JSON.stringify(body),
  });
  if (res.status !== 204) throw new Error(`feedback failed (HTTP ${res.status})`);
}

/** Download a file the agent generated this turn. Fetches it with the bearer
 *  token (kept out of the URL) and saves the returned blob under `filename`.
 *  Owner-scoped server-side, so a guessed id 404s. */
/** Fetch a staged file's bytes. The route is bearer-authenticated, so an
 *  `<img src>` cannot reach it directly — an inline image goes through here and
 *  renders from an object URL. */
export async function fetchFile(
  base: string,
  agent: string,
  token: string,
  artifactId: string,
): Promise<Blob> {
  const url = `${base}/public/files/${encodeURIComponent(agent)}/${encodeURIComponent(artifactId)}`;
  const res = await get(url, token);
  if (!res.ok) throw new Error(`download failed (HTTP ${res.status})`);
  return res.blob();
}

export async function downloadFile(
  base: string,
  agent: string,
  token: string,
  artifactId: string,
  filename: string,
): Promise<void> {
  const objectUrl = URL.createObjectURL(await fetchFile(base, agent, token, artifactId));
  const a = document.createElement("a");
  a.href = objectUrl;
  a.download = filename;
  a.click();
  // Revoke after the click's synchronous save-initiation, not before, or the
  // browser can cancel the in-flight download.
  setTimeout(() => URL.revokeObjectURL(objectUrl), 0);
}

/** "just now" / "5m" / "3h" / "2d" / date — compact relative time for the list. */
export function relativeTime(iso: string): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "";
  const s = Math.max(0, (Date.now() - then) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  if (s < 604800) return `${Math.floor(s / 86400)}d ago`;
  return new Date(then).toLocaleDateString();
}

/** One filter on a server-backed table page. */
export interface TableFilterParam {
  col: string;
  op: "contains" | "gte" | "lte";
  value: string | number;
}

/** A page request for a server-backed (artifact) table. */
export interface TableParams {
  offset: number;
  limit: number;
  sort?: { column: string; desc: boolean };
  search?: string;
  filters?: TableFilterParam[];
}

/** The staged artifact is gone (TTL-swept) — the caller shows an expiry note. */
export class TableExpired extends Error {}

/** The query string for a table page (pure, so it's unit-tested). */
export function tableQueryString(p: TableParams): string {
  const q = new URLSearchParams();
  q.set("offset", String(p.offset));
  q.set("limit", String(p.limit));
  if (p.sort) {
    q.set("sort", p.sort.column);
    q.set("dir", p.sort.desc ? "desc" : "asc");
  }
  if (p.search) q.set("q", p.search);
  if (p.filters && p.filters.length) q.set("filters", JSON.stringify(p.filters));
  return q.toString();
}

/** Fetch one filtered/sorted page of a staged table. Owner-scoped server-side;
 *  a 404 means the artifact expired, surfaced as {@link TableExpired}. */
export async function fetchTablePage(
  base: string,
  agent: string,
  token: string,
  artifactId: string,
  params: TableParams,
): Promise<TablePage> {
  const url =
    `${base}/public/tables/${encodeURIComponent(agent)}/${encodeURIComponent(artifactId)}` +
    `?${tableQueryString(params)}`;
  const res = await get(url, token);
  if (res.status === 404) throw new TableExpired("this table is no longer available");
  if (!res.ok) throw new Error(`table page failed (HTTP ${res.status})`);
  return res.json();
}

/** One file staged by the edge, ready to ride a turn as an id. */
export interface StagedFile {
  artifact_id: string;
  filename: string;
  media_type: string;
  bytes: number;
}

/** Stage files for the next message. The server re-checks type, size and the
 *  agent's allowlist — the client's own checks only save a round trip — and
 *  answers a refusal with a message meant for the customer, which is what the
 *  caller surfaces rather than inventing its own wording. */
export async function uploadFiles(
  base: string,
  agent: string,
  token: string,
  files: File[],
): Promise<StagedFile[]> {
  const body = new FormData();
  for (const f of files) body.append("file", f, f.name);
  const res = await fetch(`${base}/public/upload/${encodeURIComponent(agent)}`, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}` },
    body,
  });
  if (!res.ok) {
    const said = await res
      .json()
      .then((j: { error?: string }) => j.error)
      .catch(() => undefined);
    throw new Error(said ?? "The file couldn't be uploaded.");
  }
  const json = (await res.json()) as { files?: StagedFile[] };
  return json.files ?? [];
}
