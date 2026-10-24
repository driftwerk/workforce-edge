// Agent-produced file references, deduped by artifact id — a reconnect replays a
// running turn's attachments from the start, so live and replayed sets overlap.

import type { Attachment } from "./connection";

/** Append `file` unless an attachment with the same artifact is already present. */
export function withAttachment(list: Attachment[] | undefined, file: Attachment): Attachment[] {
  return list?.some((f) => f.artifact_id === file.artifact_id) ? list : [...(list ?? []), file];
}

/** Union of live-received and transcript-recovered attachments, by artifact. */
export function mergeAttachments(
  live: Attachment[] | undefined,
  recovered: Attachment[] | undefined,
): Attachment[] | undefined {
  const merged = (recovered ?? []).reduce(withAttachment, live);
  return merged?.length ? merged : undefined;
}
