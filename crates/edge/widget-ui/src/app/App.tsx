import {
  Component,
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import Markdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";

import { dockAt, dragOrigin, dragStep, nextCorner, type Anchor, type Corner } from "../anchor";
import type { FrameState, ScreenContext, ThemeTokens, WidgetEvent, WidgetFeatures } from "../protocol";
import { EdgeConnection, isServerError, type Attachment } from "./connection";
import { fenceInfo, richKind } from "./rich/fence";
import { RichBlock } from "./rich/RichBlock";
import { EdgeApiContext } from "./rich/apiContext";
import { EdgeSubmitContext, type SubmitValue } from "./rich/submitContext";
import {
  deleteConversation,
  downloadFile,
  fetchFile,
  uploadFiles,
  getConversation,
  listConversations,
  postFeedback,
  relativeTime,
  type ConversationSummary,
  type TranscriptMessage,
} from "./api";
import {
  AttachChips,
  DrawingCanvas,
  acceptAttribute,
  attachHint,
  localRefusal,
  pendingKey,
  useDropTarget,
  type Pending,
} from "./Attach";
import { InlineImage, isInlineImage } from "./InlineImage";
import { pollForReply } from "./recovery";
import { mergeAttachments, withAttachment } from "./attachments";

const PANEL_W = 400;
const PANEL_H = 660;
const MIN_W = 320;
const MIN_H = 420;
const MAX_PANEL = 3000;
const PAGE = 30;
/** Files the composer will hold for one message; the server caps this too. */
const MAX_ATTACHMENTS = 5;

const clampSize = (v: number, lo: number) => Math.max(lo, Math.min(MAX_PANEL, v));

export interface AppProps {
  base: string;
  agent: string;
  token: string | null;
  theme: ThemeTokens;
  /** Agent greeting shown as the first assistant bubble of a new chat. */
  welcome: string;
  /** Which wf-* fenced blocks render as interactive components. */
  features: WidgetFeatures;
  resize: (state: FrameState, width?: number, height?: number) => void;
  /** Where the loader has actually placed the widget — drives the launcher inset,
   *  the resize grip's corner, and which way both of them grow. */
  anchor: Anchor;
  /** Ask the loader to move the widget by a drag step, in screen px. */
  move: (dx: number, dy: number) => void;
  /** Ask the loader to place the widget at an exact anchor (the corner cycle). */
  reanchor: (anchor: Anchor) => void;
  /** Report a usage gesture for the host page's analytics. Fire-and-forget. */
  emit: (event: WidgetEvent) => void;
  /** Latest UNTRUSTED screen context from the host page (null if none sent). Sent
   *  with each chat turn as background about what the user is viewing. */
  context: ScreenContext | null;
}

export interface Msg {
  id: number;
  role: "user" | "assistant";
  text: string;
  /** Assistant reply is mid-stream and currently working (e.g. running a tool) —
   *  show the typing indicator after whatever text has arrived so far. */
  working?: boolean;
  /** The assistant `message_id`, from the `done` frame on a live turn or from
   *  the transcript on a restored one — present so this answer can be rated;
   *  absent only mid-stream. */
  messageId?: string;
  /** The conversation this answer belongs to, captured at completion (the socket
   *  knows it even on a new chat's first turn, before the id reaches app state). */
  conversation?: string;
  /** Files the agent produced this turn, rendered as download buttons. Restored
   *  from the transcript on reload too; the underlying artifacts are TTL-swept
   *  server-side, so an old button may 404 when clicked (the button surfaces it). */
  attachments?: Attachment[];
  /** The reply is still streaming — an unparseable wf-* fence shows a skeleton
   *  (it may complete) instead of falling back to a code block. */
  streaming?: boolean;
  /** What the agent is working on right now (server `progress` frames), shown
   *  beside the typing dots; cleared as soon as answer text arrives. */
  status?: string;
  /** This row's socket died and its answer is being recovered from the
   *  transcript. Recovery outlives the turn that started it, so the turn's own
   *  cleanup must not clear this row's working state. */
  recovering?: boolean;
  /** When this row started working, for the elapsed-time indicator. */
  startedAt?: number;
}

/** How recently the user's newest conversation must have been touched for a
 *  pointer-less surface to adopt it (ENG-111). Long enough to survive a
 *  Hub → SuperHub hop or a new tab mid-task; short enough that opening the
 *  widget tomorrow starts fresh rather than resurrecting an old thread. */
const ADOPT_WINDOW_MS = 30 * 60_000;

/** How long a turn must run before the elapsed counter appears, so the common
 *  case stays uncluttered. */
const ELAPSED_AFTER_S = 10;

/** Counts up beside the working indicator on a slow turn.
 *
 *  A silent three-minute wait reads as a dead assistant, and a customer who
 *  believes it is dead asks again — which is exactly what used to discard the
 *  previous answer. */
function Elapsed({ since }: { since: number }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const secs = Math.max(0, Math.round((now - since) / 1000));
  if (secs < ELAPSED_AFTER_S) return null;
  return (
    <span className="wf-elapsed">
      {secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m ${secs % 60}s`}
    </span>
  );
}

/** A fresh reference for one turn. Sent with the question and stamped by the
 *  server onto the answer, so a recovery can claim its own answer even when the
 *  transcript holds several outstanding questions. `randomUUID` needs a secure
 *  context; the fallback keeps older/insecure embeds working (recovery then
 *  degrades to the positional match, which refuses ambiguous cases). */
function newTurnRef(): string {
  return (
    globalThis.crypto?.randomUUID?.() ?? `t-${Date.now()}-${Math.random().toString(36).slice(2)}`
  );
}

/** Shown while a dropped stream waits for the transcript to yield the answer.
 *
 *  Deliberately not "reconnecting": by the time this appears the socket usually
 *  IS back, and what the customer is actually waiting on is a turn still running
 *  server-side — possibly on a replica this socket cannot be attached to. Saying
 *  the work continues, and that the answer lands here, is what stops them
 *  retyping the question, which is the thing that used to lose it. */
const RECOVERING_NOTE = "_Connection dropped — still working. Your answer will appear here._";
const RECOVERY_FAILED_NOTE =
  "_Connection interrupted — I couldn't recover the reply. Please resend your message._";

/** Settle a recovering assistant row once its transcript poll returns: the saved
 *  answer if found, else the resend note after whatever had already streamed.
 *  Shared by in-turn and mount-time recovery. */
function applyRecoveredReply(x: Msg, reply: TranscriptMessage | null, streamed = ""): Msg {
  if (reply) {
    return {
      ...x,
      text: reply.content,
      attachments: mergeAttachments(x.attachments, reply.attachments),
      working: false,
      streaming: false,
    };
  }
  return {
    ...x,
    working: false,
    streaming: false,
    text: (streamed ? streamed + "\n\n" : "") + RECOVERY_FAILED_NOTE,
  };
}

/** Rate one assistant answer. Returns a promise the caller can await (from
 *  ChatPanel, closing over base/agent/token); the control owns its own UI state. */
type SubmitFeedback = (conversation: string, messageId: string, rating: 1 | -1, comment?: string) => Promise<void>;

/** One conversation turn: the user-bubble text plus the sender that streams the
 *  reply — `conn.stream` for a typed message, `conn.submitForm` for a form. The
 *  shared turn machinery in `runTurn` is written once and parameterised by this. */
interface Turn {
  display: string;
  /** Files the customer attached, shown in their own message bubble. Every chat
   *  surface shows you what you just sent; a caption-only bubble reads as if the
   *  file never went. */
  attachments?: Attachment[];
  send: (
    conn: EdgeConnection,
    signal: AbortSignal,
    onDone: (messageId?: string) => void,
    onAttachment: (a: Attachment) => void,
    onProgress: (label: string) => void,
    /** This turn's reference, echoed back onto the answer so a recovery started
     *  for THIS row can claim it without guessing from position. */
    clientRef: string,
  ) => AsyncGenerator<string, void>;
  /** Fires `true` once the turn's frame is on its way, or `false` if it is
   *  dropped from the interrupt queue before running — lets a form it backs
   *  reflect real delivery rather than optimistically reading as sent. */
  onDelivered?: (delivered: boolean) => void;
}

/** Renders a caught error as text rather than blanking the whole widget. */
class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };
  static getDerivedStateFromError(error: Error) {
    return { error };
  }
  render() {
    if (this.state.error) {
      // A caught render error is often a chunk that couldn't be fetched even
      // after retries (a stale chunk after a deploy, where only a fresh load of
      // the app pulls the new hashes). Offer a reload rather than dead-ending.
      return (
        <div className="wf-connecting">
          Something went wrong displaying the chat.{" "}
          <button type="button" className="wf-reload" onClick={() => window.location.reload()}>
            Reload
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

/** The send glyph. `arrow` (default) is the original dart; `plane` is a swept paper
 *  plane — concave back, rounded tip, and a slot cut toward the tip — which some
 *  brands specify instead. Per-agent, because a send icon is a brand choice and
 *  swapping it globally would restyle every install.
 *
 *  The plane is drawn a little larger and carries a hairline stroke of its own fill:
 *  it rounds the joins, which is what keeps it from reading as a hard triangle at
 *  the ~20px it renders at. */
function SendIcon({ kind }: { kind?: "arrow" | "plane" }) {
  const plane = kind === "plane";
  const size = plane ? 22 : 18;
  return (
    <svg viewBox="0 0 24 24" width={size} height={size} fill="none" aria-hidden="true">
      <path
        d={
          plane
            ? "M21.61 12C16.65 8.75 11.07 6.11 6.11 4.71C9.05 8.12 10.14 9.83 10.45 10.92L15.41 10.92Q16.5 12 15.41 13.08L10.45 13.08C10.14 14.17 9.05 15.88 6.11 19.29C11.07 17.89 16.65 15.25 21.61 12Z"
            : // Tip on the RIGHT — the original path put it at x=4, pointing backwards.
              "M20 12L4 4l6 8-6 8 16-8Z"
        }
        fill="currentColor"
        {...(plane
          ? { stroke: "currentColor", strokeWidth: 0.7, strokeLinejoin: "round" as const }
          : {})}
      />
    </svg>
  );
}

function ChatIcon() {
  return (
    <svg viewBox="0 0 24 24" width="26" height="26" fill="none" aria-hidden="true">
      <path
        d="M4 4h16a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H8l-4 4V5a1 1 0 0 1 1-1Z"
        fill="currentColor"
      />
    </svg>
  );
}

function TypingDots() {
  return (
    <span className="wf-typing" aria-label="Assistant is typing">
      <span className="wf-dot" />
      <span className="wf-dot" />
      <span className="wf-dot" />
    </span>
  );
}

function EmptyState({ theme }: { theme: ThemeTokens }) {
  return (
    <div className="wf-empty">
      {theme.logo_url ? (
        <img className="wf-empty-logo" src={theme.logo_url} alt="" />
      ) : (
        <div className="wf-empty-icon">
          <ChatIcon />
        </div>
      )}
      <div className="wf-empty-title">{theme.title}</div>
      {theme.subtitle && <div className="wf-empty-sub">{theme.subtitle}</div>}
    </div>
  );
}

// Custom renderers: wrap tables so wide ones scroll instead of overflowing the
// bubble, and open links in a new tab. `node` is react-markdown's AST handle —
// strip it so it isn't forwarded onto the DOM element.
const MD_COMPONENTS: Components = {
  table: ({ node, ...props }) => {
    void node;
    return (
      <div className="wf-tablewrap">
        <table {...props} />
      </div>
    );
  },
  a: ({ node, ...props }) => {
    void node;
    return <a {...props} target="_blank" rel="noopener noreferrer" />;
  },
};

// While streaming, an as-yet-unclosed ``` fence would swallow the rest of the
// message as code. Balance it with a trailing fence so partial output renders.
function balanceFences(text: string): string {
  const fences = (text.match(/```/g) ?? []).length;
  return fences % 2 === 1 ? text + "\n```" : text;
}

function MarkdownMessage({
  text,
  features,
  streaming,
}: {
  text: string;
  features: WidgetFeatures;
  streaming: boolean;
}) {
  // Read `streaming` through a ref so the components map (and thus the `pre`
  // renderer identity) stays stable when a turn ends — otherwise a chart that
  // rendered mid-stream would remount and replay its animation. Re-render on the
  // final token still re-invokes the renderer, so an invalid block still swaps
  // from skeleton to its code-block fallback.
  const streamingRef = useRef(streaming);
  streamingRef.current = streaming;
  const components = useMemo<Components>(() => {
    if (
      !features.tables &&
      !features.charts &&
      !features.forms &&
      !features.maps &&
      !features.diagrams
    )
      return MD_COMPONENTS;
    return {
      ...MD_COMPONENTS,
      pre: ({ node, ...props }) => {
        const fence = fenceInfo(node);
        const kind = fence && richKind(fence.lang, features);
        if (!kind) return <pre {...props} />;
        return (
          <RichBlock
            kind={kind}
            payload={fence.text}
            streaming={streamingRef.current}
            fallback={<pre {...props} />}
            excalidraw={features.excalidraw}
          />
        );
      },
    };
  }, [features]);
  return (
    <div className="wf-md">
      <Markdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeSanitize]} components={components}>
        {balanceFences(text)}
      </Markdown>
    </div>
  );
}

function ThumbIcon({ up = false }: { up?: boolean }) {
  return (
    <svg
      viewBox="0 0 24 24"
      width="15"
      height="15"
      fill="currentColor"
      aria-hidden="true"
      style={up ? undefined : { transform: "rotate(180deg)" }}
    >
      <path d="M1 21h4V9H1v12zm22-11a2 2 0 0 0-2-2h-6.31l.95-4.57.03-.32a1.5 1.5 0 0 0-.44-1.06L14.17 1 7.59 7.59A2 2 0 0 0 7 9v10a2 2 0 0 0 2 2h9a2 2 0 0 0 1.84-1.22l3.02-7.05c.09-.23.14-.47.14-.73v-1z" />
    </svg>
  );
}

/** Unobtrusive 👍/👎 on a completed answer. The vote is recorded immediately;
 *  a down-vote also reveals an optional one-line comment. Upserts server-side,
 *  so changing the vote just re-POSTs. Never blocks the chat.
 *
 *  Every submit produces a visible acknowledgement (ENG-113): the vote keeps
 *  its filled icon as the durable record, the confirmation note names what
 *  just happened, and while the comment box is open the note reads as "sent,
 *  add detail if you like" so the Send button can't be mistaken for the real
 *  submission. Sending the comment closes the form and re-announces — the
 *  press the user believes is *the* submit must never produce zero change. */
function FeedbackControl({ submit }: { submit: (rating: 1 | -1, comment?: string) => Promise<void> }) {
  const [rating, setRating] = useState<1 | -1 | null>(null);
  const [status, setStatus] = useState<"idle" | "sending" | "done" | "error">("idle");
  const [showComment, setShowComment] = useState(false);
  const [comment, setComment] = useState("");
  // Counts submits, so a repeat acknowledgement remounts the note (re-running
  // its fade-in) even when the status string is unchanged.
  const [sends, setSends] = useState(0);

  const run = async (r: 1 | -1, note?: string, closeForm = false) => {
    setRating(r);
    setStatus("sending");
    try {
      await submit(r, note);
      if (closeForm) setShowComment(false);
      setSends((n) => n + 1);
      setStatus("done");
    } catch {
      setStatus("error");
    }
  };
  const up = () => {
    setShowComment(false);
    void run(1);
  };
  const down = () => {
    setShowComment(true);
    void run(-1, comment.trim() || undefined);
  };
  const sendComment = () => void run(-1, comment.trim() || undefined, true);

  const note = showComment
    ? "Feedback sent — add detail below if you like"
    : "Thanks — feedback sent";
  return (
    <div className="wf-fb">
      <div className="wf-fb-row">
        <button
          type="button"
          className={"wf-fb-btn" + (rating === 1 ? " wf-fb-on" : "")}
          aria-pressed={rating === 1}
          aria-label="Good response"
          disabled={status === "sending"}
          onClick={up}
        >
          <ThumbIcon up />
        </button>
        <button
          type="button"
          className={"wf-fb-btn wf-fb-down" + (rating === -1 ? " wf-fb-on" : "")}
          aria-pressed={rating === -1}
          aria-label="Bad response"
          disabled={status === "sending"}
          onClick={down}
        >
          <ThumbIcon />
        </button>
        {status === "done" && (
          <span className="wf-fb-note" role="status" key={sends}>
            {note}
          </span>
        )}
        {status === "error" && <span className="wf-fb-err" role="alert">Couldn't save. Try again.</span>}
      </div>
      {showComment && (
        <form
          className="wf-fb-comment"
          onSubmit={(e) => {
            e.preventDefault();
            sendComment();
          }}
        >
          <input
            className="wf-fb-input"
            value={comment}
            maxLength={2000}
            placeholder="What went wrong? (optional)"
            aria-label="Feedback comment"
            onChange={(e) => setComment(e.target.value)}
          />
          <button type="submit" className="wf-fb-send" disabled={status === "sending"}>
            Send
          </button>
        </form>
      )}
    </div>
  );
}

/** A download button for one agent-produced file; surfaces a retry on failure. */
function AttachmentButton({
  file,
  onDownload,
}: {
  file: Attachment;
  onDownload: (a: Attachment) => Promise<void>;
}) {
  const [state, setState] = useState<"idle" | "busy" | "error">("idle");
  return (
    <div className="wf-attachment-wrap">
      <button
        type="button"
        className="wf-attachment"
        disabled={state === "busy"}
        onClick={async () => {
          setState("busy");
          try {
            await onDownload(file);
            setState("idle");
          } catch {
            setState("error");
          }
        }}
      >
        {`⬇ ${file.filename}`}
      </button>
      {state === "error" && (
        <span className="wf-attachment-err" role="alert">
          Couldn&apos;t download — click to retry.
        </span>
      )}
    </div>
  );
}

export function MessageRow({
  m,
  features,
  avatar,
  onFeedback,
  onDownload,
  onLoadFile,
}: {
  m: Msg;
  features: WidgetFeatures;
  /** The brand avatar element, or undefined when the agent has none configured. */
  avatar?: ReactNode;
  onFeedback?: SubmitFeedback;
  onDownload?: (a: Attachment) => Promise<void>;
  /** Fetch an attachment's bytes, for rendering an image inline. */
  onLoadFile?: (a: Attachment) => Promise<Blob>;
}) {
  const bubble = (
    <div className={"wf-bubble wf-bubble-" + m.role}>
      {m.role === "assistant" ? (
        m.text ? (
          <ErrorBoundary>
            <MarkdownMessage text={m.text} features={features} streaming={!!m.streaming} />
            {m.working && (
              <div className="wf-working">
                <TypingDots />
                {m.status && <span className="wf-status">{m.status}</span>}
                {m.startedAt != null && <Elapsed since={m.startedAt} />}
              </div>
            )}
          </ErrorBoundary>
        ) : m.working ? (
          // Empty text but still working: the typing indicator. A settled row
          // with no text (e.g. an aborted recovery) renders nothing.
          <span className="wf-working">
            <TypingDots />
            {m.status && <span className="wf-status">{m.status}</span>}
            {m.startedAt != null && <Elapsed since={m.startedAt} />}
          </span>
        ) : null
      ) : (
        m.text
      )}
    </div>
  );
  // A file sent with no caption has nothing to put in a bubble; the attachment
  // itself is the message.
  const hasBubble = m.role === "assistant" || !!m.text;
  // Both roles: the agent's produced files, and the customer's own attachments.
  // Gating this to `assistant` is why a photo you sent vanished from your own
  // bubble — every chat surface shows you what you just sent.
  const files =
    !!m.attachments?.length && !!onDownload ? (
      <div className="wf-attachments">
        {m.attachments.map((a) =>
          features.inline_images && isInlineImage(a) && onLoadFile ? (
            <InlineImage
              key={a.artifact_id}
              file={a}
              load={onLoadFile}
              onDownload={() => void onDownload?.(a)}
              fallback={<AttachmentButton file={a} onDownload={onDownload} />}
            />
          ) : (
            <AttachmentButton key={a.artifact_id} file={a} onDownload={onDownload} />
          ),
        )}
      </div>
    ) : null;
  const canRate = m.role === "assistant" && !m.working && !!m.messageId && !!m.conversation && !!onFeedback;
  return (
    <div className={"wf-row wf-row-" + m.role}>
      {/* A SIBLING of the bubble, never a wrapper — the rich-output width rules in
          styles.css select `.wf-row-assistant > .wf-bubble` / `> .wf-msg` as direct
          children, and wrapping would silently squash charts, tables and maps. */}
      {m.role === "assistant" && avatar}
      {files || canRate ? (
        <div className="wf-msg">
          {hasBubble && bubble}
          {files}
          {canRate && (
            <FeedbackControl
              submit={(rating, note) => onFeedback!(m.conversation!, m.messageId!, rating, note)}
            />
          )}
        </div>
      ) : (
        bubble
      )}
    </div>
  );
}

/** The message list + composer for one conversation. Resumes an existing
 * conversation (newest page first, older pages on scroll-up) and streams new
 * turns straight from the edge WebSocket into the last assistant message.
 *
 * Memoised: a drag re-renders App on every pointer move, and re-rendering a long
 * transcript (every bubble, every markdown tree) at that rate visibly janks. Its
 * props are all stable between drag steps, so this skips the whole subtree. */
const ChatPanel = memo(function ChatPanel({
  base,
  agent,
  token,
  theme,
  welcome,
  features,
  conversationId,
  onConversationId,
  emit,
  context,
}: {
  base: string;
  agent: string;
  token: string;
  theme: ThemeTokens;
  welcome: string;
  features: WidgetFeatures;
  conversationId: string | null;
  onConversationId: (id: string) => void;
  emit: (event: WidgetEvent) => void;
  context: ScreenContext | null;
}) {
  // The authed endpoint a server-backed table fetches from. Token via a ref so a
  // refresh doesn't re-render (and re-fetch) any open table.
  const tokenRef = useRef(token);
  tokenRef.current = token;
  const edgeApi = useMemo(() => ({ base, agent, token: tokenRef }), [base, agent]);
  const [messages, setMessages] = useState<Msg[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [oldestStart, setOldestStart] = useState(0);
  const [hasMore, setHasMore] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);

  const conn = useRef<EdgeConnection | null>(null);
  const nextId = useRef(0);
  // The in-flight turn's aborter + a turn queued to interrupt it (answered next).
  const abortRef = useRef<AbortController | null>(null);
  const pendingTurn = useRef<Turn | null>(null);
  // Busy mirrored into a ref so the stable turn-start closures (composer + form
  // submit) read the current value without being recreated on every toggle.
  const busyRef = useRef(false);
  const viewport = useRef<HTMLDivElement>(null);
  const scrollContent = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // Autoscroll only when the user is already near the bottom; a prepend of older
  // history must instead restore the prior scroll position (advisor's two bugs).
  const stick = useRef(true);
  const restoreBelow = useRef<number | null>(null);
  // The latest host screen context, mirrored into a ref so the stable `runTurn`
  // closure (deps `[]`, to avoid remounting the socket) always sends the current
  // one — including on an interrupt-queued turn answered after navigation.
  const contextRef = useRef<ScreenContext | null>(context);
  useEffect(() => {
    contextRef.current = context;
  }, [context]);

  // Carry the persisted message id (assistant rows) and the conversation into
  // restored rows, so answers stay ratable after a reload, a resume, or a
  // History-drawer open — previously the thumbs silently vanished on restore.
  const toMsg = (m: TranscriptMessage): Msg => ({
    id: ++nextId.current,
    role: m.role,
    text: m.content,
    messageId: m.message_id,
    conversation: conversationId ?? undefined,
    attachments: m.attachments,
  });

  // Poll the transcript for one row's saved answer, on a lifetime of its OWN.
  //
  // Recovery deliberately does NOT share the turn's AbortController, and does not
  // hold `busy`. It used to do both, which meant the user's next question aborted
  // the previous question's recovery — and a user watching a stalled row for forty
  // seconds asks again almost every time. The answer was then discarded while
  // still being generated. Only unmount cancels a recovery now, so several can be
  // outstanding and each settles into its own row.
  const recoveriesRef = useRef(new Map<number, AbortController>());
  const beginRecovery = useCallback(
    (
      botId: number,
      conversation: string,
      sent: string,
      streamed?: string,
      clientRef?: string,
    ) => {
      const ctrl = new AbortController();
      recoveriesRef.current.set(botId, ctrl);
      setMessages((m) =>
        m.map((x) =>
          x.id === botId
            ? {
                ...x,
                recovering: true,
                working: true,
                streaming: true,
                status: undefined,
                startedAt: x.startedAt ?? Date.now(),
                text: (streamed ? streamed + "\n\n" : "") + RECOVERING_NOTE,
              }
            : x,
        ),
      );
      void (async () => {
        try {
          const reply = await pollForReply(
            base,
            agent,
            () => tokenRef.current,
            conversation,
            sent,
            ctrl.signal,
            clientRef,
          );
          setMessages((m) =>
            m.map((x) =>
              x.id === botId
                ? { ...applyRecoveredReply(x, reply, streamed), recovering: false }
                : x,
            ),
          );
        } finally {
          recoveriesRef.current.delete(botId);
        }
      })();
    },
    // Stable: `base`/`agent` are fixed for the panel's life and the token is read
    // through a ref, so a recovery started now stays valid for its whole poll.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  // A transcript ending on a user row means its turn is (or was) still running
  // when this panel loaded — a reload mid-turn, or a turn whose socket died.
  // Show a working row and recover its answer. The user can keep typing: this no
  // longer claims `busy`, so a new question runs alongside rather than killing it.
  const recoverPending = useCallback(
    (sent: string, conversation: string) => {
      const botId = ++nextId.current;
      stick.current = true;
      setMessages((m) => [...m, { id: botId, role: "assistant", text: "" }]);
      beginRecovery(botId, conversation, sent);
    },
    [beginRecovery],
  );

  // Grow the composer to fit what's been typed, up to the CSS max-height (past which
  // it scrolls). `rows={1}` alone pins it to one line, so a second line was simply
  // clipped out of view while typing.
  const autosize = useCallback(() => {
    const el = inputRef.current;
    if (!el) return;
    // An empty composer is exactly one row — never measure it. `scrollHeight` on an
    // empty textarea reflects the WRAPPED PLACEHOLDER, and the panel mounts while the
    // iframe is still launcher-narrow: "Type a message…" wrapped to 295px there, and
    // that height stuck until the first keystroke made it measurable again. Clearing
    // the inline height hands it back to `rows={1}`.
    if (!el.value) {
      el.style.height = "";
      return;
    }
    el.style.height = "auto";
    // `scrollHeight` covers content + padding but NOT the border, while the element is
    // `box-sizing: border-box` — so assigning it raw leaves the box a border short and
    // the text sitting 2px tight. Add the border back.
    const border = el.offsetHeight - el.clientHeight;
    el.style.height = `${el.scrollHeight + border}px`;
  }, []);
  useLayoutEffect(autosize, [input, autosize]);

  // …and re-measure whenever the composer's WIDTH changes. The panel mounts inside an
  // iframe that is still launcher-sized and only grows to its open box a frame or two
  // later; a height measured in that window is nonsense (measured at 295px for an
  // empty box) and, because the height effect only re-runs on input, it stuck until
  // the first keystroke — which is why the composer opened several lines tall and
  // then snapped to one. Width-gated so setting the height can't re-trigger this.
  useLayoutEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    let lastWidth = el.clientWidth;
    const ro = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width ?? 0;
      if (w === lastWidth) return;
      lastWidth = w;
      autosize();
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [autosize]);

  useEffect(() => {
    conn.current = new EdgeConnection(base, agent, token, conversationId ?? undefined, onConversationId);
    // `preventScroll`: this mount happens while the iframe is still launcher-sized,
    // where the composer sits below the 68px panel — so a plain focus() asks the
    // browser to scroll every ancestor to reveal it, including the panel itself.
    // Scroll position here is owned by the explicit stick-to-bottom effect below,
    // never by the focus.
    inputRef.current?.focus({ preventScroll: true });
    stick.current = true;
    let cancelled = false;
    // Captured now: the cleanup below must abort the same map this mount used,
    // not whatever `recoveriesRef.current` happens to hold when it runs.
    const recoveries = recoveriesRef.current;
    if (conversationId) {
      getConversation(base, agent, token, conversationId, { limit: PAGE })
        .then((page) => {
          if (cancelled) return;
          setMessages(page.messages.map(toMsg));
          setOldestStart(page.start);
          setHasMore(page.has_more);
          const last = page.messages[page.messages.length - 1];
          if (last?.role === "user") recoverPending(last.content, conversationId);
        })
        .catch((e) => {
          if (cancelled) return;
          console.error("[wf-edge] could not load conversation", conversationId, e);
          // A resumed id that no longer loads (e.g. deleted) must not wedge every
          // reopen — forget it so the next open starts clean.
          writeStoredConv(agent, null);
          setLoadFailed(true);
        });
    }
    return () => {
      cancelled = true;
      abortRef.current?.abort();
      // Recoveries outlive their turns, so unmount is the only thing that ends
      // them — without this they would poll on against a dead panel.
      for (const ctrl of recoveries.values()) ctrl.abort();
      recoveries.clear();
      conn.current?.close();
    };
    // Run ONCE per mount. ChatPanel is remounted (via `key={instance}`) whenever
    // the conversation genuinely changes, so the socket must NOT be recreated on
    // a token refresh or when we adopt our own server-assigned id — either would
    // mint a brand-new conversation and lose the thread.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The socket is created once per mount, so push token refreshes to it in place
  // (the host re-issues before the ~5-min Keycloak expiry) — the server rotates
  // the MCP token without dropping the conversation.
  useEffect(() => {
    if (token) conn.current?.setToken(token);
  }, [token]);

  // Position management: restore after a prepend, else stick to bottom.
  useLayoutEffect(() => {
    const v = viewport.current;
    if (!v) return;
    if (restoreBelow.current != null) {
      v.scrollTop = v.scrollHeight - restoreBelow.current;
      restoreBelow.current = null;
    } else if (stick.current) {
      v.scrollTop = v.scrollHeight;
    }
  }, [messages]);

  // Async content (lazy charts/tables, images) grows the thread after render, so
  // a one-shot scroll-on-load or -stream lands short. Keep the view pinned to
  // the bottom while it settles — unless the user has scrolled up or a prepend
  // is restoring position.
  useEffect(() => {
    const v = viewport.current;
    const content = scrollContent.current;
    if (!v || !content) return;
    const ro = new ResizeObserver(() => {
      if (stick.current && restoreBelow.current == null) v.scrollTop = v.scrollHeight;
    });
    ro.observe(content);
    return () => ro.disconnect();
  }, []);

  const loadOlder = useCallback(async () => {
    const v = viewport.current;
    if (!conversationId || loadingOlder || oldestStart <= 0) return;
    setLoadingOlder(true);
    try {
      const page = await getConversation(base, agent, token, conversationId, {
        limit: PAGE,
        before: oldestStart,
      });
      if (v) restoreBelow.current = v.scrollHeight - v.scrollTop;
      setMessages((m) => [...page.messages.map(toMsg), ...m]);
      setOldestStart(page.start);
      setHasMore(page.has_more);
    } catch {
      /* leave what's loaded */
    } finally {
      setLoadingOlder(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [base, agent, token, conversationId, oldestStart, loadingOlder]);

  const onScroll = () => {
    const v = viewport.current;
    if (!v) return;
    stick.current = v.scrollHeight - v.scrollTop - v.clientHeight < 80;
    if (v.scrollTop < 60 && hasMore && !loadingOlder) void loadOlder();
  };

  // Run one turn: append the user + a streaming assistant row, then fold tokens
  // into it. A gap with no new text (the agent is running a tool) re-shows the
  // typing dots so the user knows to keep waiting. Parameterised by a `Turn` so a
  // typed message and a form submit share the identical streaming machinery.
  const runTurn = useCallback(async (turn: Turn) => {
    if (!conn.current) {
      turn.onDelivered?.(false);
      return;
    }
    turn.onDelivered?.(true);
    const userId = ++nextId.current;
    const botId = ++nextId.current;
    stick.current = true;
    setMessages((m) => [
      ...m,
      { id: userId, role: "user", text: turn.display, attachments: turn.attachments },
      {
        id: botId,
        role: "assistant",
        text: "",
        working: true,
        streaming: true,
        startedAt: Date.now(),
      },
    ]);
    setBusy(true);
    busyRef.current = true;
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    let gap: ReturnType<typeof setTimeout> | undefined;
    const armGap = () => {
      clearTimeout(gap);
      // ~1.6s so normal between-token latency doesn't flicker the dots; a real
      // tool-call gap (many seconds) still surfaces them.
      gap = setTimeout(
        () => setMessages((m) => m.map((x) => (x.id === botId ? { ...x, working: true } : x))),
        1600,
      );
    };
    armGap();
    let messageId: string | undefined;
    let lastText = "";
    const clientRef = newTurnRef();
    try {
      for await (const acc of turn.send(
        conn.current,
        ctrl.signal,
        (id) => {
          messageId = id;
        },
        (a) => {
          setMessages((m) =>
            m.map((x) => (x.id === botId ? { ...x, attachments: withAttachment(x.attachments, a) } : x)),
          );
        },
        (label) => {
          setMessages((m) =>
            m.map((x) => (x.id === botId ? { ...x, status: label, working: true } : x)),
          );
        },
        clientRef,
      )) {
        armGap();
        lastText = acc;
        // An empty answer is a rewind that took everything back: still working.
        setMessages((m) =>
          m.map((x) => (x.id === botId ? { ...x, text: acc, working: !acc, status: undefined } : x)),
        );
      }
      // Attach the answer's id + conversation so it can be rated. The socket knows
      // the conversation even on a new chat's first turn (set on `connected`).
      const conversation = conn.current?.conversation ?? undefined;
      if (messageId && conversation) {
        const mid = messageId;
        setMessages((m) => m.map((x) => (x.id === botId ? { ...x, messageId: mid, conversation } : x)));
      }
    } catch (e) {
      const lost = e instanceof Error && e.message === "connection-lost";
      if (!lost) {
        // Prefer the server's own wording — it knows WHY the turn ended (took
        // too long, carried on in another window, …). Falling back to the
        // generic line meant every such message was composed and then thrown
        // away here, so the customer always read the same unhelpful sentence.
        // Server-tagged only: local plumbing errors ("connection failed") are
        // developer strings and must never reach the transcript.
        const detail =
          isServerError(e) && e.message.trim() ? e.message.trim() : "Sorry — I hit an error. Please try again.";
        setMessages((m) =>
          m.map((x) =>
            x.id === botId
              ? {
                  ...x,
                  working: false,
                  status: undefined,
                  text: (x.text ? x.text + "\n\n" : "") + `_${detail}_`,
                }
              : x,
          ),
        );
      } else {
        // The stream is gone but the turn is (probably) still running and will
        // save its answer — recover it from the transcript rather than asking the
        // user to resend. Deliberately NOT awaited: this turn ends here so `busy`
        // clears and the next question can start immediately, while the recovery
        // runs on its own lifetime and settles into this row whenever the answer
        // lands. Awaiting it here is what let the next question cancel it.
        clearTimeout(gap);
        const conversation = conn.current?.conversation;
        if (conversation) {
          beginRecovery(botId, conversation, turn.display, lastText, clientRef);
        } else {
          setMessages((m) =>
            m.map((x) =>
              x.id === botId
                ? applyRecoveredReply(x, null, lastText)
                : x,
            ),
          );
        }
      }
    } finally {
      clearTimeout(gap);
      // A recovering row keeps its working state: its answer is still coming and
      // the recovery that owns the row will settle it.
      setMessages((m) =>
        m.map((x) =>
          x.id === botId && !x.recovering ? { ...x, working: false, streaming: false } : x,
        ),
      );
      if (abortRef.current === ctrl) abortRef.current = null;
      setBusy(false);
      busyRef.current = false;
      inputRef.current?.focus();
      // A turn queued mid-flight (interrupt) is answered now that this one stopped.
      const queued = pendingTurn.current;
      pendingTurn.current = null;
      if (queued) void runTurn(queued);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Start a turn, or — when one is already running — queue it and interrupt the
  // current stream (type-to-interrupt, like Slack). The running turn's `finally`
  // replays the queued one; the server cancels the aborted turn and answers next.
  const startTurn = useCallback(
    (turn: Turn) => {
      if (!conn.current) {
        turn.onDelivered?.(false);
        return;
      }
      // One per user-initiated turn: composer send, form submit, interrupt.
      emit("message-sent");
      if (busyRef.current && abortRef.current) {
        // A still-queued turn we replace never runs — tell it so a form it
        // backs can unlock instead of falsely reading as submitted.
        pendingTurn.current?.onDelivered?.(false);
        pendingTurn.current = turn;
        abortRef.current.abort();
        return;
      }
      void runTurn(turn);
    },
    [runTurn, emit],
  );

  // Deliver a filled form as its own turn: the human summary is the user bubble,
  // `conn.submitForm` the sender. Stable, so the context value never churns.
  const submitForm = useCallback(
    (summary: string, values: Record<string, SubmitValue>) =>
      new Promise<boolean>((resolve) =>
        startTurn({
          display: summary,
          send: (c, signal, onDone, onAttachment, onProgress, clientRef) =>
            c.submitForm(summary, values, signal, onDone, onAttachment, onProgress, clientRef),
          onDelivered: resolve,
        }),
      ),
    [startTurn],
  );
  const submitContext = useMemo(() => ({ submitForm }), [submitForm]);

  const sendFeedback = useCallback<SubmitFeedback>(
    (conversation, messageId, rating, comment) =>
      postFeedback(base, agent, conversation, token, { message_id: messageId, rating, comment }),
    [base, agent, token],
  );

  const download = useCallback(
    (a: Attachment) => downloadFile(base, agent, token, a.artifact_id, a.filename),
    [base, agent, token],
  );

  const loadFile = useCallback(
    (a: Attachment) => fetchFile(base, agent, token, a.artifact_id),
    [base, agent, token],
  );

  // One element reused by every agent row — it is static for a given theme, and
  // `aria-hidden` because the row's role is already clear from its position and the
  // mark carries no information a screen reader needs.
  const avatar = useMemo(
    () =>
      theme.avatar ? (
        <div className="wf-avatar" aria-hidden="true">
          {theme.logo_url ? <img src={theme.logo_url} alt="" draggable={false} /> : <ChatIcon />}
        </div>
      ) : undefined,
    [theme.avatar, theme.logo_url],
  );

  const [pending, setPending] = useState<Pending[]>([]);
  const [drawing, setDrawing] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);

  /** Stage dropped/picked/drawn files. Each gets a chip immediately so the
   *  customer sees the file was taken, then resolves to staged or refused. */
  const attach = useCallback(
    (files: File[]) => {
      if (features.uploads.length === 0) return;
      const room = MAX_ATTACHMENTS - pending.length;
      const taken = files.slice(0, Math.max(0, room));
      const fresh: Pending[] = taken.map((f) => ({ key: pendingKey(), filename: f.name }));
      setPending((cur) => [...cur, ...fresh]);
      taken.forEach((file, i) => {
        const key = fresh[i].key;
        const refusal = localRefusal(file, features);
        if (refusal) {
          setPending((cur) => cur.map((p) => (p.key === key ? { ...p, error: refusal } : p)));
          return;
        }
        uploadFiles(base, agent, token ?? "", [file]).then(
          (staged) =>
            setPending((cur) =>
              cur.map((p) => (p.key === key ? { ...p, staged: staged[0] } : p)),
            ),
          (e: Error) =>
            setPending((cur) => cur.map((p) => (p.key === key ? { ...p, error: e.message } : p))),
        );
      });
    },
    [base, agent, token, features, pending.length],
  );

  const drop = useDropTarget(features.uploads.length > 0, attach);

  const send = useCallback(() => {
    const text = input.trim();
    // A file with no typed message is a real turn; the server accepts it too.
    const ids = pending.filter((p) => p.staged).map((p) => p.staged!.artifact_id);
    if ((!text && ids.length === 0) || !conn.current) return;
    // Nothing is sent while an upload is still in flight — the customer's file
    // must reach the turn it was attached to, not the next one.
    if (pending.some((p) => !p.staged && !p.error)) return;
    setInput("");
    setPending([]);
    const sent: Attachment[] = pending
      .filter((p) => p.staged)
      .map((p) => ({
        artifact_id: p.staged!.artifact_id,
        filename: p.staged!.filename,
        media_type: p.staged!.media_type,
      }));
    startTurn({
      display: text,
      attachments: sent.length ? sent : undefined,
      send: (c, signal, onDone, onAttachment, onProgress, clientRef) =>
        c.stream(
          text,
          signal,
          onDone,
          onAttachment,
          contextRef.current ?? undefined,
          onProgress,
          ids,
          clientRef,
        ),
    });
  }, [input, pending, startTurn]);

  return (
    <EdgeApiContext.Provider value={edgeApi}>
    <EdgeSubmitContext.Provider value={submitContext}>
    <div className="wf-thread">
      <div className="wf-viewport" ref={viewport} onScroll={onScroll}>
        <div className="wf-scroll" ref={scrollContent}>
        {loadingOlder && (
          <div className="wf-older">
            <TypingDots />
          </div>
        )}
        {messages.length === 0 ? (
          loadFailed ? (
            <div className="wf-empty">
              <div className="wf-empty-sub">Couldn't load this conversation. Please try again.</div>
            </div>
          ) : (
            <>
              <EmptyState theme={theme} />
              {!conversationId && welcome && (
                <MessageRow
                  m={{ id: 0, role: "assistant", text: welcome }}
                  features={features}
                  avatar={avatar}
                />
              )}
            </>
          )
        ) : (
          messages.map((m) => (
            <MessageRow
              key={m.id}
              m={m}
              features={features}
              avatar={avatar}
              onFeedback={sendFeedback}
              onDownload={download}
              onLoadFile={loadFile}
            />
          ))
        )}
        </div>
      </div>
      {drawing && (
        <DrawingCanvas
          onCancel={() => setDrawing(false)}
          onDone={(f) => {
            setDrawing(false);
            attach([f]);
          }}
        />
      )}
      <form
        className={"wf-composer" + (drop.over ? " wf-composer-drop" : "")}
        onSubmit={(e) => {
          e.preventDefault();
          void send();
        }}
        onDragOver={drop.onDragOver}
        onDragLeave={drop.onDragLeave}
        onDrop={drop.onDrop}
      >
        <AttachChips
          pending={pending}
          onRemove={(key) => setPending((cur) => cur.filter((p) => p.key !== key))}
        />
        <div className="wf-composer-row">
          {features.uploads.length > 0 && (
            <>
              <input
                ref={fileInput}
                type="file"
                multiple
                className="wf-file-input"
                accept={acceptAttribute(features.uploads)}
                onChange={(e) => {
                  attach([...(e.target.files ?? [])]);
                  e.target.value = "";
                }}
              />
              <button
                type="button"
                className="wf-attach-btn"
                aria-label={`Attach a file (${attachHint(features.uploads)})`}
                title={`Attach ${attachHint(features.uploads)}`}
                disabled={pending.length >= MAX_ATTACHMENTS}
                onClick={() => fileInput.current?.click()}
              >
                📎
              </button>
            </>
          )}
          {features.drawing && (
            <button
              type="button"
              className="wf-attach-btn"
              aria-label="Draw something"
              title="Draw something"
              disabled={drawing || pending.length >= MAX_ATTACHMENTS}
              onClick={() => setDrawing(true)}
            >
              ✎
            </button>
          )}
          <textarea
            ref={inputRef}
            className="wf-input"
            value={input}
            placeholder="Type a message…"
            rows={1}
            onChange={(e) => setInput(e.target.value)}
            onPaste={drop.onPaste}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void send();
              }
            }}
          />
          <button
            className="wf-send"
            type="submit"
            disabled={
              (!input.trim() && !pending.some((p) => p.staged)) ||
              pending.some((p) => !p.staged && !p.error)
            }
            aria-label={busy ? "Interrupt and send" : "Send message"}
          >
            <SendIcon kind={theme.send_icon} />
          </button>
        </div>
      </form>
    </div>
    </EdgeSubmitContext.Provider>
    </EdgeApiContext.Provider>
  );
});

/** The list of the caller's past conversations. */
function HistoryList({
  base,
  agent,
  token,
  activeId,
  onOpen,
}: {
  base: string;
  agent: string;
  token: string;
  activeId: string | null;
  onOpen: (id: string) => void;
}) {
  const [items, setItems] = useState<ConversationSummary[] | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setFailed(false);
    listConversations(base, agent, token)
      .then((x) => !cancelled && setItems(x))
      .catch((e) => {
        if (cancelled) return;
        console.error("[wf-edge] could not list conversations", e);
        setFailed(true);
      });
    return () => {
      cancelled = true;
    };
  }, [base, agent, token]);

  const remove = async (e: React.MouseEvent, id: string) => {
    e.stopPropagation();
    setItems((x) => x?.filter((i) => i.conversation !== id) ?? null);
    try {
      await deleteConversation(base, agent, token, id);
    } catch {
      /* best-effort; a reopen re-syncs the list */
    }
  };

  if (failed) return <div className="wf-connecting">Couldn't load your conversations.</div>;
  if (!items) return <div className="wf-connecting">Loading…</div>;
  if (items.length === 0) return <div className="wf-connecting">No conversations yet.</div>;

  return (
    <div className="wf-history">
      {items.map((c) => (
        <button
          key={c.conversation}
          className={"wf-hist-item" + (c.conversation === activeId ? " wf-hist-active" : "")}
          onClick={() => onOpen(c.conversation)}
        >
          <span className="wf-hist-main">
            <span className="wf-hist-title">{c.title}</span>
            <span className="wf-hist-meta">{relativeTime(c.updated_at)}</span>
          </span>
          <span
            className="wf-hist-del"
            role="button"
            aria-label="Delete conversation"
            title="Delete"
            onClick={(e) => remove(e, c.conversation)}
          >
            <svg viewBox="0 0 24 24" width="15" height="15" fill="none" aria-hidden="true">
              <path
                d="M6 7h12M9 7V5h6v2m-7 0v12h8V7"
                stroke="currentColor"
                strokeWidth="1.6"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
          </span>
        </button>
      ))}
    </div>
  );
}

function IconButton({
  title,
  onClick,
  children,
}: {
  title: string;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button className="wf-ico" title={title} aria-label={title} onClick={onClick}>
      {children}
    </button>
  );
}

/** Per-agent key for the last active conversation. sessionStorage so it resumes
 *  within the tab but not forever; wrapped because a sandboxed iframe without
 *  same-origin access throws on any storage access. Cross-tab and cross-app
 *  continuity comes from the server-backed adopt in `App`, not from storage —
 *  conversations are owner-scoped server-side, so identity follows the user.
 *
 *  `FRESH_CONV` marks a *deliberate* new chat: without it, "no pointer stored"
 *  (a brand-new tab, where adopting the latest conversation is wanted) is
 *  indistinguishable from "the user just pressed New chat" (where resurrecting
 *  the thread they walked away from would be a bug). */
const convKey = (agent: string) => "wf-conv:" + agent;
const FRESH_CONV = "fresh";
function readStoredConvRaw(agent: string): string | null {
  try {
    return sessionStorage.getItem(convKey(agent));
  } catch {
    return null;
  }
}
function readStoredConv(agent: string): string | null {
  const v = readStoredConvRaw(agent);
  return v === FRESH_CONV ? null : v;
}
function writeStoredConv(agent: string, id: string | null): void {
  try {
    if (id) sessionStorage.setItem(convKey(agent), id);
    else sessionStorage.removeItem(convKey(agent));
  } catch {
    /* storage unavailable (sandboxed) — reopen persistence still works via state */
  }
}
function markFreshChat(agent: string): void {
  try {
    sessionStorage.setItem(convKey(agent), FRESH_CONV);
  } catch {
    /* storage unavailable — worst case a reload adopts the recent thread */
  }
}

interface PanelSize {
  w: number;
  h: number;
}
const sizeKey = (agent: string) => "wf-size:" + agent;
function readStoredSize(agent: string): PanelSize | null {
  try {
    const s = JSON.parse(sessionStorage.getItem(sizeKey(agent)) ?? "null");
    return typeof s?.w === "number" && typeof s?.h === "number" ? s : null;
  } catch {
    return null;
  }
}
function writeStoredSize(agent: string, size: PanelSize): void {
  try {
    sessionStorage.setItem(sizeKey(agent), JSON.stringify(size));
  } catch {
    /* storage unavailable (sandboxed) — the size still holds for this session */
  }
}

/** Pointer-drag plumbing shared by the resize grip and the two move handles. It
 *  reports screen-coordinate deltas (the iframe moves and resizes under the cursor,
 *  so client coordinates would fight the drag's own feedback) and arms only after the
 *  press has travelled far enough to not be a click.
 *
 *  The press is followed on the WINDOW, not on the grab surface. Pointer capture is
 *  what keeps a drag alive once the cursor wanders off, but it cannot be taken until
 *  the press arms — take it on `pointerdown` and every click on a header button would
 *  retarget to the header instead. That leaves a gap of a few pixels with no capture,
 *  and the grab surfaces are small (a ~56px header strip, an 84px launcher, a 22px
 *  resize grip), so a brisk drag leaves the element before its first `pointermove`
 *  lands. Bound to the element, that press is never seen: it never arms, never
 *  captures, and does nothing at all — the drag "works sometimes", depending
 *  entirely on how fast the user moved. */
function useDrag(onStep: (dx: number, dy: number) => void, onDragEnd?: () => void) {
  const armed = useRef(false);
  const release = useRef<(() => void) | null>(null);
  const step = useRef(onStep);
  step.current = onStep;
  const dragEnd = useRef(onDragEnd);
  dragEnd.current = onDragEnd;

  // A press that never ends must not outlive the component.
  useEffect(() => () => release.current?.(), []);

  /** Whether the press that just ended was a drag, clearing the flag as it reports.
   *  A keyboard `click` arrives with no `pointerdown` to reset it, so leaving it set
   *  would make the launcher ignore every subsequent Enter/Space. */
  const consumeArmed = useCallback(() => {
    const was = armed.current;
    armed.current = false;
    return was;
  }, []);

  const onPointerDown = useCallback((e: React.PointerEvent<HTMLElement>) => {
    // Primary pointer, primary button. A second finger is never primary, so it
    // cannot re-origin a drag in flight and teleport the widget by the gap between
    // the two; a right-drag toward the context menu cannot move it either.
    if (!e.isPrimary || e.button !== 0) return;
    // A press released where no end event reached us leaves its listeners attached;
    // drop them rather than refusing the new press, which would wedge the widget.
    release.current?.();

    const surface = e.currentTarget;
    const pointerId = e.pointerId;
    let state = dragOrigin(e.screenX, e.screenY, e.pointerType);
    armed.current = false;

    const move = (ev: PointerEvent) => {
      if (ev.pointerId !== pointerId) return;
      if (ev.buttons === 0) return end();
      const next = dragStep(state, ev.screenX, ev.screenY);
      state = next.next;
      if (!state.armed) return;
      // Capture once armed, so an unarmed press still delivers its click to the
      // button underneath, and an armed one no longer does. From here the drag also
      // survives the cursor leaving the iframe entirely.
      if (!armed.current) {
        armed.current = true;
        surface.setPointerCapture(pointerId);
      }
      step.current(next.dx, next.dy);
    };
    const end = () => {
      release.current = null;
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", end);
      window.removeEventListener("pointercancel", end);
      if (surface.hasPointerCapture(pointerId)) surface.releasePointerCapture(pointerId);
      // Read, not consume: the trailing click still needs the flag to know it was
      // a drag. Only a press that armed is a completed gesture.
      if (armed.current) dragEnd.current?.();
    };

    release.current = end;
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", end);
    window.addEventListener("pointercancel", end);
  }, []);

  return { handlers: { onPointerDown }, consumeArmed };
}

/** A corner grip the user drags to resize the open panel, on the corner diagonally
 *  opposite the anchor — the panel grows away from wherever it is docked, so both
 *  drag axes invert with the anchor. */
function ResizeHandle({ corner, onResize }: { corner: Corner; onResize: (dw: number, dh: number) => void }) {
  const { handlers } = useDrag(
    useCallback(
      (dx: number, dy: number) => onResize(corner[1] === "r" ? -dx : dx, corner[0] === "b" ? -dy : dy),
      [corner, onResize],
    ),
  );
  return <div className="wf-resize" title="Drag to resize" aria-label="Resize chat window" {...handlers} />;
}

/** The two root classes that put every anchor-relative element (launcher, resize
 *  grip, panel entry animation) on the side the widget actually sits on. */
const axisClasses = (c: Corner) => `wf-y-${c[0]} wf-x-${c[1]}`;

const CORNER_LABEL: Record<Corner, string> = {
  br: "bottom right",
  bl: "bottom left",
  tl: "top left",
  tr: "top right",
};

export function App({ base, agent, token, theme, welcome, features, resize, anchor, move, reanchor, emit, context }: AppProps) {
  const [open, setOpen] = useState(false);
  const [full, setFull] = useState(false);
  const [view, setView] = useState<"chat" | "history">("chat");
  // Resume the last conversation on reopen (App stays mounted) and across a page
  // reload within the same tab (sessionStorage). Without this the widget minted a
  // fresh conversation every reopen and "forgot" everything said before.
  const [conversationId, setConversationId] = useState<string | null>(() => readStoredConv(agent));
  const [instance, setInstance] = useState(0);
  // The configured panel box and drag floor. The server has already clamped these
  // into a usable range and guaranteed `min <= open`; the fallbacks here are for a
  // stale server that ships no sizing at all, and the `Math.min` for one that ships
  // only some of the four.
  const box = useMemo(() => {
    const w = theme.width ?? PANEL_W;
    const h = theme.height ?? PANEL_H;
    return { w, h, minW: Math.min(theme.min_width ?? MIN_W, w), minH: Math.min(theme.min_height ?? MIN_H, h) };
  }, [theme.width, theme.height, theme.min_width, theme.min_height]);

  // User-chosen panel size, remembered for the session; a drag grows it and the
  // loader caps it to the host viewport. A stored size predates any config change,
  // so clamp it back into the configured range rather than honouring a size the
  // operator has since ruled out.
  const [size, setSize] = useState<PanelSize>(() => {
    const stored = readStoredSize(agent);
    if (!stored) return { w: box.w, h: box.h };
    return { w: clampSize(stored.w, box.minW), h: clampSize(stored.h, box.minH) };
  });

  // Persist only a real id: writing on null would erase the `FRESH_CONV`
  // sentinel `newChat` just stored (and re-arm the adopt below).
  useEffect(() => {
    if (conversationId) writeStoredConv(agent, conversationId);
  }, [agent, conversationId]);

  // Cross-surface resume (ENG-111): the pointer above is per-tab and
  // per-surface, but the conversation itself is owned by the signed-in user —
  // so when this surface has no pointer (a new tab, or the other host app),
  // adopt the user's most recent conversation if it is fresh enough to still
  // be "what I was just doing". Skipped when the user deliberately started a
  // new chat, and abandoned if a conversation appears meanwhile (e.g. the
  // user already started typing and the server minted one).
  const adoptAttempted = useRef(false);
  useEffect(() => {
    if (conversationId || adoptAttempted.current || !token) return;
    if (readStoredConvRaw(agent) === FRESH_CONV) return;
    adoptAttempted.current = true;
    let live = true;
    void (async () => {
      try {
        const items = await listConversations(base, agent, token);
        const newest = items[0];
        if (!live || !newest) return;
        const age = Date.now() - Date.parse(newest.updated_at);
        if (age >= 0 && age <= ADOPT_WINDOW_MS) {
          // Adopt atomically against the race where the user already started
          // a chat (the socket minted an id while the list was in flight) —
          // and only then remount the panel so it loads the adopted thread.
          let adopted = false;
          setConversationId((current) => {
            if (current) return current;
            adopted = true;
            return newest.conversation;
          });
          if (adopted) setInstance((n) => n + 1);
        }
      } catch {
        /* fresh chat is the graceful fallback */
      }
    })();
    return () => {
      live = false;
    };
  }, [conversationId, agent, base, token]);

  useEffect(() => {
    writeStoredSize(agent, size);
  }, [agent, size]);

  const resizePanel = useCallback(
    (dw: number, dh: number) =>
      setSize((s) => ({ w: clampSize(s.w + dw, box.minW), h: clampSize(s.h + dh, box.minH) })),
    [box.minW, box.minH],
  );

  // Adopt the server-assigned id for a new chat (does NOT remount the panel — it's
  // keyed by `instance`, and the effect runs once per mount — so no reconnect).
  const adoptConversation = useCallback((id: string) => setConversationId(id), []);

  // An `expand` launcher grows sideways on hover, but it is drawn INSIDE the iframe
  // and the loader sizes that frame to the launcher — so without asking for more room
  // the pill is simply clipped at the frame edge. Measure the expanded width and ask
  // for it while hovered, and only while hovered: the extra width is transparent, and
  // a permanently wide frame would swallow host-page clicks beside the launcher.
  const [pillWidth, setPillWidth] = useState(0);
  const launcher = useRef<HTMLButtonElement | null>(null);
  /** Widest label seen so far. Monotonic on purpose: the first measurement can be
   *  taken with the fallback font, and pinning `--wf-pill-label` to that narrower
   *  value stops the button resizing when the real font lands — so the observer
   *  never fires and the stale width sticks. Growing only can't get stuck. */
  const labelWidth = useRef(0);
  const measurePill = useCallback(() => {
    const el = launcher.current;
    if (!el || theme.launcher !== "expand") return;
    const label = el.querySelector<HTMLElement>(".wf-launch-label");
    if (!label) return;
    // `scrollWidth`, not `offsetWidth`: the label renders at max-width 0, so its
    // laid-out width is 0 while its CONTENT width is the natural label width.
    const text = label.scrollWidth;
    if (!text) return;
    if (text > labelWidth.current) labelWidth.current = text;
    // A few px of slack so a sub-pixel text width can't shave the last glyph.
    el.style.setProperty("--wf-pill-label", `${labelWidth.current + 6}px`);
    // While the label is revealed the button's own width is already the answer;
    // while it is collapsed, add the label and the gap the hover rule introduces.
    const revealed = label.offsetWidth > 0;
    const natural = revealed
      ? el.offsetWidth
      : el.offsetWidth + labelWidth.current + 9;
    // `scrollWidth` of the root catches the case the others miss: the button already
    // overflowing the frame, whatever the reason. Then ask for a little more than
    // that, so a rounded cap is never sliced flat against the frame's edge.
    const want = Math.max(natural, document.documentElement.scrollWidth) + 10;
    setPillWidth((w) => (want > w ? want : w));
  }, [theme.launcher]);
  const [pillOpen, setPillOpen] = useState(false);

  // Keep the measurement honest without depending on when a web font lands. A
  // ResizeObserver on the button fires through the reveal itself, so even if the
  // first measurement is taken with a fallback font — narrower than Poppins, which
  // left the first hover's pill wider than the frame and clipped on its left — the
  // frame catches up within the same animation.
  useLayoutEffect(() => {
    const el = launcher.current;
    if (!el || theme.launcher !== "expand") return;
    measurePill();
    const ro = new ResizeObserver(measurePill);
    ro.observe(el);
    document.fonts?.addEventListener?.("loadingdone", measurePill);
    return () => {
      ro.disconnect();
      document.fonts?.removeEventListener?.("loadingdone", measurePill);
    };
  }, [theme.launcher, theme.title, measurePill]);

  useEffect(() => {
    if (!open) {
      // `resize` clamps to the viewport, so an over-long title can't push the frame
      // off-screen. LAUNCHER (84) is the loader's floor for a closed frame.
      const w = pillOpen && pillWidth ? Math.max(pillWidth, 84) : 84;
      resize("closed", w, 84);
    } else if (full) resize("fullscreen");
    else resize("open", size.w, size.h);
  }, [open, full, size, resize, pillOpen, pillWidth]);

  const newChat = () => {
    // A deliberate fresh thread: mark it so neither this mount's adopt nor the
    // next load resurrects the conversation the user just walked away from.
    markFreshChat(agent);
    setConversationId(null);
    setView("chat");
    setInstance((n) => n + 1);
  };
  const openConversation = (id: string) => {
    setConversationId(id);
    setView("chat");
    setInstance((n) => n + 1);
  };

  // Dragging the launcher moves the widget; dragging the header moves the open
  // panel. Only one of the two is ever mounted, so they share a drag. Neither does
  // anything until the press arms, so a tap still opens the widget and a click still
  // reaches the header buttons. Fullscreen covers the viewport — nothing to move.
  const drag = useDrag(move, () => emit("moved"));
  const grabbable = theme.draggable && !full;
  const cornerClass = axisClasses(anchor.corner);

  if (!open) {
    return (
      <div className={"wf-root " + cornerClass}>
        <button
          ref={launcher}
          className={"wf-launch wf-launch-" + theme.launcher + (theme.draggable ? " wf-grab" : "")}
          onClick={() => {
            if (drag.consumeArmed()) return;
            setOpen(true);
            emit("opened");
            // `full` survives a close, so this reopen goes straight back to
            // fullscreen — report that entry, or fullscreen use undercounts.
            if (full) emit("fullscreen");
          }}
          // Focus counts as well as hover — the CSS reveals the label for both, so the
          // frame has to make room for both or a keyboard user sees a clipped pill.
          onMouseEnter={() => {
            measurePill();
            setPillOpen(true);
          }}
          onMouseLeave={() => setPillOpen(false)}
          onFocus={() => {
            measurePill();
            setPillOpen(true);
          }}
          onBlur={() => setPillOpen(false)}
          aria-label={"Open " + theme.title}
          {...(theme.draggable ? drag.handlers : {})}
        >
          {theme.logo_url ? <img src={theme.logo_url} alt="" draggable={false} /> : <ChatIcon />}
          {theme.launcher === "pill" && <span className="wf-launch-label">{theme.title}</span>}
          {/* `expand` reveals the same label on hover/focus, by animating its
              max-width from 0 — see the note in styles.css on why that shape is
              what makes the label measurable. */}
          {theme.launcher === "expand" && (
            <span className="wf-launch-label" aria-hidden="true">
              {theme.title}
            </span>
          )}
        </button>
      </div>
    );
  }

  return (
    <div className={"wf-root " + cornerClass + (full ? " wf-full" : "")}>
      <div className="wf-panel">
        {!full && <ResizeHandle corner={anchor.corner} onResize={resizePanel} />}
        <header
          className={"wf-head" + (grabbable ? " wf-grab" : "")}
          {...(grabbable ? drag.handlers : {})}
          // A drag that started on an action button must not also fire it. Browsers
          // are meant to retarget the click to this header once the drag captures the
          // pointer, but that is not something to depend on across engines.
          //
          // Always consume, and suppress only a POINTER click (`detail > 0`). If a
          // drag's trailing click never arrives the flag would otherwise still be set
          // when the user next reaches a header button by keyboard — where `click`
          // carries no pointerdown to clear it — and eat that activation.
          onClickCapture={(e) => {
            if (drag.consumeArmed() && grabbable && e.detail > 0) e.stopPropagation();
          }}
        >
          <div className="wf-head-l">
            {theme.logo_url && <img className="wf-logo" src={theme.logo_url} alt="" draggable={false} />}
            <div className="wf-head-text">
              <div className="wf-title">{view === "history" ? "Conversations" : theme.title}</div>
              {view !== "history" && theme.subtitle && <div className="wf-sub">{theme.subtitle}</div>}
            </div>
          </div>
          <div className="wf-acts">
            {grabbable && (
              <IconButton
                title={"Move to " + CORNER_LABEL[nextCorner(anchor.corner)]}
                onClick={() => {
                  reanchor(dockAt(nextCorner(anchor.corner), theme));
                  emit("moved");
                }}
              >
                <svg viewBox="0 0 24 24" width="17" height="17" fill="none" aria-hidden="true">
                  <path
                    d="M12 4v16M4 12h16M9 7l3-3 3 3M9 17l3 3 3-3M7 9l-3 3 3 3M17 9l3 3-3 3"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  />
                </svg>
              </IconButton>
            )}
            <IconButton
              title={view === "history" ? "Back to chat" : "Conversations"}
              onClick={() => {
                if (view === "chat") emit("conversation-list-opened");
                setView(view === "history" ? "chat" : "history");
              }}
            >
              {view === "history" ? (
                <svg viewBox="0 0 24 24" width="17" height="17" fill="none" aria-hidden="true">
                  <path d="M15 6l-6 6 6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                </svg>
              ) : (
                <svg viewBox="0 0 24 24" width="17" height="17" fill="none" aria-hidden="true">
                  <path d="M4 6h16M4 12h16M4 18h16" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                </svg>
              )}
            </IconButton>
            <IconButton title="New chat" onClick={newChat}>
              <svg viewBox="0 0 24 24" width="17" height="17" fill="none" aria-hidden="true">
                <path d="M12 5v14M5 12h14" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
              </svg>
            </IconButton>
            <IconButton
              title={full ? "Shrink" : "Expand"}
              onClick={() => {
                if (!full) emit("fullscreen");
                setFull((f) => !f);
              }}
            >
              {full ? "⤡" : "⤢"}
            </IconButton>
            <IconButton
              title="Close"
              onClick={() => {
                setOpen(false);
                emit("closed");
              }}
            >
              ✕
            </IconButton>
          </div>
        </header>
        {!token ? (
          <div className="wf-connecting">Connecting…</div>
        ) : view === "history" ? (
          <HistoryList base={base} agent={agent} token={token} activeId={conversationId} onOpen={openConversation} />
        ) : (
          <ChatPanel
            key={instance}
            base={base}
            agent={agent}
            token={token}
            theme={theme}
            welcome={welcome}
            features={features}
            conversationId={conversationId}
            onConversationId={adoptConversation}
            emit={emit}
            context={context}
          />
        )}
      </div>
    </div>
  );
}
