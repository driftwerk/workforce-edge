// One persistent chat WebSocket per conversation, exposed as a streaming async
// generator. Speaks the edge wire protocol: server frames connected / typing /
// token / done / error; client sends { type: "chat", content } or, for a filled
// form, { type: "form_submit", content, submission }. The token rides the WS
// subprotocol (browsers can't set Authorization on a socket).

import type { ScreenContext } from "../protocol";
import type { SubmitValue } from "./rich/submitContext";

/** A frame the widget sends up the socket to drive one turn. */
type ClientFrame =
  | {
      type: "chat";
      content: string;
      context?: ScreenContext;
      attachments?: string[];
      /** This turn's client-minted reference; the server stamps it onto the
       *  answer so transcript recovery can match it exactly. */
      client_ref?: string;
    }
  | {
      type: "form_submit";
      content: string;
      submission: Record<string, SubmitValue>;
      client_ref?: string;
    };

/** A file the agent produced this turn. The widget downloads the bytes from the
 *  authenticated route using its own base/agent/token; the socket carries only
 *  this reference. */
export interface Attachment {
  artifact_id: string;
  filename: string;
  media_type: string;
}

interface ServerFrame {
  type:
    | "connected"
    | "typing"
    | "token"
    | "done"
    | "error"
    | "attachment"
    | "reconnect"
    | "ping"
    | "progress"
    | "resume"
    | "rewind";
  content?: string;
  session_id?: string;
  /** The assistant message's id — present only on `done`, so the widget can rate
   *  that exact answer. */
  message_id?: string;
  /** The downloadable file — present only on `attachment`. */
  attachment?: Attachment;
}

type Frame = (m: ServerFrame) => void;

/** How long a dropped mid-turn stream waits for the auto-reconnect before
 *  failing over to transcript recovery. Covers at least two backoff attempts. */
const RECOVERY_AFTER_DROP_MS = 25_000;
/** After a reconnect, the server replays a running turn immediately — so if no
 *  `resume` follows the `connected` within this window, none is coming. */
const RECOVERY_AFTER_CONNECT_MS = 4_000;

/** An error whose message came from the server and is safe to show verbatim. */
export type ServerError = Error & { fromServer: true };

/** Tag a server-composed message so the UI knows it may render it as-is —
 *  local plumbing errors ("connection failed", "not connected") must not leak
 *  into the transcript. */
function serverError(message: string): ServerError {
  return Object.assign(new Error(message), { fromServer: true as const });
}

/** Whether `e` carries a server-composed, customer-safe message. */
export function isServerError(e: unknown): e is ServerError {
  return e instanceof Error && (e as Partial<ServerError>).fromServer === true;
}

export class EdgeConnection {
  private ws: WebSocket | null = null;
  private ready: Promise<void> | null = null;
  private onFrame: Frame | null = null;
  /** Ends the in-flight turn's stream if the socket drops mid-turn, so it
   *  surfaces a reconnect error instead of hanging on frames that never come. */
  private onDrop: (() => void) | null = null;
  /** Set by `close()` (panel closed) so a deliberate close doesn't reconnect. */
  private closed = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectAttempts = 0;
  /** Server-assigned conversation id (from the `connected` greeting). */
  conversation: string | null;

  constructor(
    private base: string,
    private agent: string,
    private token: string,
    conversation?: string | null,
    /** Called with the server-assigned conversation id (from the `connected`
     *  greeting) so the app can persist it and resume this chat on reopen. */
    private onConversation?: (id: string) => void,
  ) {
    this.conversation = conversation ?? null;
  }

  private connect(): Promise<void> {
    if (this.ready) return this.ready;
    // Connecting now supersedes any scheduled retry.
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.ready = new Promise<void>((resolve, reject) => {
      const wsBase = this.base.replace(/^http/, "ws");
      const url = new URL(wsBase + "/public/ws/" + encodeURIComponent(this.agent));
      if (this.conversation) url.searchParams.set("conversation", this.conversation);
      const ws = new WebSocket(url.toString(), [this.token]);
      this.ws = ws;
      ws.onmessage = (e) => {
        let m: ServerFrame;
        try {
          m = JSON.parse(e.data);
        } catch {
          return;
        }
        if (m.type === "connected") {
          this.conversation = m.session_id ?? this.conversation;
          // Surface the id (new chats mint one server-side) so it survives a
          // panel close/reopen — otherwise every reopen starts a fresh chat.
          if (m.session_id) this.onConversation?.(m.session_id);
          this.reconnectAttempts = 0;
          resolve();
        }
        // The server sends `reconnect` while draining for a deploy, then closes;
        // `onclose` reconnects us, so don't leak the frame to the turn stream.
        if (m.type === "reconnect") return;
        this.onFrame?.(m);
      };
      ws.onerror = () => reject(new Error("connection failed"));
      ws.onclose = () => {
        this.ws = null;
        this.ready = null;
        // Fail an in-flight turn instead of hanging on a dead socket...
        this.onDrop?.();
        // ...and warm the socket back up unless the user closed the panel.
        if (!this.closed) this.scheduleReconnect();
      };
    });
    return this.ready;
  }

  /** Reconnect after an unexpected drop with capped exponential backoff; a
   *  successful `connected` resets the delay and `close()` cancels it. */
  private scheduleReconnect(): void {
    if (this.closed || this.reconnectTimer) return;
    const delay = Math.min(500 * 2 ** this.reconnectAttempts, 10_000);
    this.reconnectAttempts += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect().catch(() => {
        /* onclose re-arms the next attempt */
      });
    }, delay);
  }

  /** Send `text` and stream the reply as a growing accumulated string. When the
   *  turn completes, `onDone` fires once with the assistant `message_id` from the
   *  `done` frame (used to rate that answer); it never fires on error/abort.
   *  `context` (if given) rides the chat frame as UNTRUSTED background about the
   *  screen the user is viewing; omitted from the frame when absent. */
  async *stream(
    text: string,
    signal: AbortSignal,
    onDone?: (messageId?: string) => void,
    onAttachment?: (a: Attachment) => void,
    context?: ScreenContext,
    onProgress?: (label: string) => void,
    attachments?: string[],
    clientRef?: string,
  ): AsyncGenerator<string, void> {
    const frame: ClientFrame = { type: "chat", content: text };
    if (context) frame.context = context;
    // Ids only — the bytes were staged over HTTP and never ride this socket.
    if (attachments?.length) frame.attachments = attachments;
    if (clientRef) frame.client_ref = clientRef;
    yield* this.run(frame, signal, onDone, onAttachment, onProgress);
  }

  /** Deliver a filled form back to the running conversation: `summary` becomes the
   *  turn's user message, `values` the exact machine arguments. Streams and
   *  interrupts identically to {@link stream}. */
  async *submitForm(
    summary: string,
    values: Record<string, SubmitValue>,
    signal: AbortSignal,
    onDone?: (messageId?: string) => void,
    onAttachment?: (a: Attachment) => void,
    onProgress?: (label: string) => void,
    clientRef?: string,
  ): AsyncGenerator<string, void> {
    const frame: ClientFrame = { type: "form_submit", content: summary, submission: values };
    if (clientRef) frame.client_ref = clientRef;
    yield* this.run(frame, signal, onDone, onAttachment, onProgress);
  }

  /** Send one client frame and stream the reply. Shared by every turn kind so the
   *  accumulation / typing-boundary / drop / abort semantics live in one place. */
  private async *run(
    frame: ClientFrame,
    signal: AbortSignal,
    onDone?: (messageId?: string) => void,
    onAttachment?: (a: Attachment) => void,
    onProgress?: (label: string) => void,
  ): AsyncGenerator<string, void> {
    await this.connect();
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      throw new Error("not connected");
    }

    let acc = "";
    const queue: string[] = [];
    let finished = false;
    let error: Error | null = null;
    let wake: (() => void) | null = null;
    // Frames carry the conversation id but no turn id, so when a turn is
    // interrupted the previous turn's in-flight token/done frames can arrive on
    // THIS stream. The server emits exactly one `typing` at the start of each
    // turn, so treat it as the boundary: ignore token/done until we've seen this
    // turn's `typing` (an `error` — e.g. MCP init failed with no typing — is
    // always surfaced so the stream can't hang). A `resume` replay after a
    // reconnect is equally a boundary.
    let started = false;
    const bump = () => {
      wake?.();
      wake = null;
    };

    // A dropped socket doesn't fail the stream outright: the connection
    // auto-reconnects and the server replays a still-running turn (`resume`).
    // Only when no replay arrives in time does the stream fail with
    // `connection-lost`, handing recovery to the transcript poll.
    let recovery: ReturnType<typeof setTimeout> | null = null;
    const clearRecovery = () => {
      if (recovery) {
        clearTimeout(recovery);
        recovery = null;
      }
    };
    const failLost = () => {
      if (!finished) {
        error = new Error("connection-lost");
        finished = true;
        bump();
      }
    };
    const armRecovery = (ms: number) => {
      if (finished) return;
      clearRecovery();
      recovery = setTimeout(failLost, ms);
    };

    this.onFrame = (m) => {
      if (m.type === "connected") {
        // Reconnected mid-turn: a replay follows immediately, or never.
        if (recovery) armRecovery(RECOVERY_AFTER_CONNECT_MS);
        return;
      }
      if (m.type === "ping") return; // keepalive only
      if (m.type === "typing") {
        // Deliberately does NOT clear the recovery deadline: a replay always
        // leads with `typing`, and the `resume` that follows is only honoured
        // while recovering. Clearing here would drop every replay.
        started = true;
        return;
      }
      if (m.type === "resume") {
        // Only honour a replay while recovering from a drop. Otherwise it is a
        // re-attach to a DIFFERENT still-running turn on this conversation
        // (e.g. this turn was typed during another's recovery) whose partial
        // text must not seed our accumulator — the server's `done` carries the
        // authoritative full text regardless.
        if (!recovery) return;
        clearRecovery();
        started = true;
        acc = m.content ?? "";
        // A tool-phase replay carries no text yet. Yielding "" would render the
        // row as empty — dots and status gone — until the next gap timer, so
        // only publish real text; the re-attach itself is already recorded.
        if (acc) queue.push(acc);
        bump();
        return;
      }
      if (m.type === "error") {
        error = serverError(m.content ?? "The assistant hit an error.");
        finished = true;
        bump();
        return;
      }
      if (!started) return; // stray frame from a just-interrupted prior turn
      if (m.type === "progress") {
        clearRecovery();
        if (m.content) onProgress?.(m.content);
        return;
      }
      if (m.type === "attachment") {
        if (m.attachment) onAttachment?.(m.attachment);
        return;
      }
      if (m.type === "token") {
        clearRecovery();
        acc += m.content ?? "";
        queue.push(acc);
      } else if (m.type === "rewind") {
        // The reply is being regenerated: what it had streamed is taken back.
        clearRecovery();
        acc = m.content ?? "";
        queue.push(acc);
      } else if (m.type === "done") {
        clearRecovery();
        if (m.content) acc = m.content;
        queue.push(acc);
        finished = true;
        // Inside the `started` gate so a just-interrupted prior turn's stray
        // `done` can't attach the wrong id to this answer.
        onDone?.(m.message_id);
      }
      bump();
    };

    this.onDrop = () => {
      // A deliberate close never recovers; an unexpected drop waits for the
      // reconnect + replay before giving up.
      if (this.closed) failLost();
      else armRecovery(RECOVERY_AFTER_DROP_MS);
    };

    const onAbort = () => {
      finished = true;
      bump();
    };
    signal.addEventListener("abort", onAbort);
    this.ws.send(JSON.stringify(frame));

    try {
      for (;;) {
        while (queue.length) yield queue.shift()!;
        if (finished) {
          if (error) throw error;
          return;
        }
        await new Promise<void>((r) => {
          wake = r;
        });
      }
    } finally {
      clearRecovery();
      signal.removeEventListener("abort", onAbort);
      this.onFrame = null;
      this.onDrop = null;
    }
  }

  /** Push a refreshed auth token: update it for any future reconnect, and send it
   *  over the live socket so the server rotates the MCP token before it expires. */
  setToken(token: string): void {
    if (token === this.token) return;
    this.token = token;
    try {
      if (this.ws?.readyState === WebSocket.OPEN) {
        this.ws.send(JSON.stringify({ type: "token", content: token }));
      }
    } catch {
      /* socket closing; the next reconnect will use the updated token */
    }
  }

  close() {
    this.closed = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.ws?.close();
    this.ws = null;
    this.ready = null;
  }
}
