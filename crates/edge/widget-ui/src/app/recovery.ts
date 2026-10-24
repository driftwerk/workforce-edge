// Transcript-based turn recovery. A turn whose socket stream died keeps running
// server-side and saves its answer; these helpers poll the conversation REST
// until that answer appears (or the wait exceeds the server's own turn cap).

import { getConversation, type TranscriptMessage } from "./api";

const POLL_MS = 2500;
/** Longer than the server's 240s turn cap, so a recoverable answer always lands
 *  inside the window. */
const RECOVERY_WINDOW_MS = 270_000;

/** The assistant reply that answers this turn, if the transcript holds one.
 *
 *  Prefers the exact link: the server stamps each answer with the client-minted
 *  `clientRef` of the turn it answers, so the match survives any ordering.
 *
 *  Falls back to position — the message after the LAST user row matching `sent` —
 *  only for answers saved before that link existed, and REFUSES to guess when the
 *  matched question sits in a run of consecutive user rows. That shape means two
 *  questions were outstanding at once (a dropped socket, or two replicas running
 *  turns concurrently), and answers are appended in completion order, so the row
 *  after the run is not reliably the answer to the last question in it. Returning
 *  the wrong answer confidently is far worse than returning none: the caller
 *  shows "couldn't recover, please resend" instead. */
export function recoveredReply(
  messages: TranscriptMessage[],
  sent: string,
  clientRef?: string,
  /** Whether `messages` begins at the conversation's true start (`page.start === 0`).
   *  When it does not, the row before the first one is simply off the page, and
   *  the ambiguity check below cannot see what it needs to. */
  atConversationStart = true,
): TranscriptMessage | null {
  if (clientRef) {
    const exact = messages.find((m) => m.role === "assistant" && m.reply_to === clientRef);
    if (exact) return exact;
  }
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m.role === "user" && m.content === sent) {
      const next = messages[i + 1];
      if (next?.role !== "assistant") return null;
      // We know references are in play for this turn and this answer carries a
      // different one, so it is not ours — the exact match above already had its
      // chance. Without a reference of our own (a reload lost it with the page)
      // a stamped answer is still the best candidate position can offer.
      if (clientRef && next.reply_to) return null;
      // Two questions outstanding at once means answers append in completion order
      // and position cannot say which is which. Refuse rather than risk handing
      // back the other question's answer — and refuse equally at the page edge,
      // where the row that would prove it ambiguous may just be out of view.
      const prior = messages[i - 1];
      const ambiguous = prior ? prior.role === "user" : !atConversationStart;
      return ambiguous ? null : next;
    }
  }
  return null;
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve();
    const t = setTimeout(done, ms);
    function done() {
      clearTimeout(t);
      signal.removeEventListener("abort", done);
      resolve();
    }
    signal.addEventListener("abort", done);
  });
}

/** Poll the transcript until an answer to `sent` is saved, the abort fires, or
 *  the window closes. Transient fetch failures keep polling. */
export async function pollForReply(
  base: string,
  agent: string,
  token: () => string,
  conversation: string,
  sent: string,
  signal: AbortSignal,
  clientRef?: string,
): Promise<TranscriptMessage | null> {
  const deadline = Date.now() + RECOVERY_WINDOW_MS;
  while (Date.now() < deadline && !signal.aborted) {
    try {
      const page = await getConversation(base, agent, token(), conversation, { limit: 10 });
      const reply = recoveredReply(page.messages, sent, clientRef, page.start === 0);
      if (reply) return reply;
    } catch {
      /* transient — poll again */
    }
    await sleep(POLL_MS, signal);
  }
  return null;
}
