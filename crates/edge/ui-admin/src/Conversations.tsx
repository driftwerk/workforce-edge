import { useEffect, useMemo, useRef, useState } from "react";

import * as api from "./api";
import { Markdown } from "./Markdown";

/** A stable colour per customer, derived from their owner key. */
function hueFor(key: string): string {
  let h = 0;
  for (let i = 0; i < key.length; i++) h = (h * 31 + key.charCodeAt(i)) % 360;
  return `linear-gradient(135deg, hsl(${h} 62% 55%), hsl(${(h + 40) % 360} 62% 48%))`;
}

/** Owner keys are `ext:{issuer}<US>{sub}`. Split into a readable name + issuer. */
function parseOwner(owner: string): { name: string; issuer: string } {
  const body = owner.startsWith("ext:") ? owner.slice(4) : owner;
  const [issuer, sub] = body.split("\u001f"); // U+001F unit separator
  if (!sub) return { name: body, issuer: "" };
  let host = issuer;
  try {
    host = new URL(issuer).host;
  } catch {
    /* keep raw */
  }
  return { name: sub, issuer: host };
}

interface Group {
  owner: string;
  name: string;
  /** True once `name` came from a captured `display_name` (vs the parsed key). */
  named: boolean;
  issuer: string;
  convos: api.Conversation[];
  updated: string;
}

/** Which conversation a deep-link opens directly: the exact one when the link
 *  names a conversation id that's in the loaded list, else the customer's sole
 *  conversation, else none (expand the group only — e.g. the row fell off the
 *  admin list cap, or the session was pruned). */
export function pickConversation(
  mine: api.Conversation[],
  conversation: string | null,
): api.Conversation | null {
  if (conversation) return mine.find((c) => c.conversation === conversation) ?? null;
  return mine.length === 1 ? mine[0] : null;
}

export function Conversations({ nav }: { nav: import("./main").Nav }) {
  // A deep-link `/admin/conversations/{agent}/{owner}/{conversation?}` (e.g. from
  // the Feedback/Turns "Open") pins the agent, expands that customer's group, and
  // opens the exact transcript when the conversation id is present (feedback/turn
  // rows carry the public conversation id, which `AdminConversation.conversation`
  // now exposes alongside the internal session id the transcript is keyed by).
  const deepAgent = nav.params[0] ?? null;
  const deepOwner = nav.params[1] ?? null;
  const deepConversation = nav.params[2] ?? null;

  const [agents, setAgents] = useState<api.Agent[] | null>(null);
  const [agent, setAgent] = useState<string>(deepAgent ?? "");
  const [convos, setConvos] = useState<api.Conversation[] | null>(null);
  const [open, setOpen] = useState<api.Conversation | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  // The customer whose erasure is in flight, so it cannot be sent twice.
  const [forgetting, setForgetting] = useState<string | null>(null);
  // The deep-link (owner + conversation) we've already handled, so backing out
  // to the list (which clears the URL params) doesn't re-trigger the expand/open,
  // while a fresh link to a different conversation of the same owner still does.
  const handledLink = useRef<string | null>(null);

  useEffect(() => {
    api
      .listAgents()
      .then((a) => {
        setAgents(a);
        // Don't clobber a deep-linked agent with the first in the list.
        if (!deepAgent && a[0]) setAgent(a[0].id);
      })
      .catch((e) => setError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const load = (id: string) => {
    setConvos(null);
    api.listConversations(id).then(setConvos).catch((e) => setError(String(e)));
  };

  useEffect(() => {
    if (!agent) return;
    setOpen(null);
    load(agent);
  }, [agent]);

  const forget = async (g: Group) => {
    if (
      !window.confirm(
        `Forget ${g.name}? Everything held about them is erased on every agent: their conversations, ` +
          "what the agents remember about them, their files, and the content of their ratings and " +
          "grades. Usage counts are kept without their name. This cannot be undone.",
      )
    )
      return;
    setError("");
    setNotice("");
    setForgetting(g.owner);
    try {
      const erasure = await api.forgetCustomer(g.owner);
      const line = api.describeErasure(erasure);
      if (api.erasureComplete(erasure)) setNotice(line);
      else setError(line);
      load(agent);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setForgetting(null);
    }
  };

  // Expand the deep-linked customer once their conversations load, and open the
  // exact transcript the link names (or their sole conversation).
  useEffect(() => {
    if (!deepOwner || !convos) return;
    const linkKey = JSON.stringify([deepOwner, deepConversation]);
    if (handledLink.current === linkKey) return;
    handledLink.current = linkKey;
    const mine = convos.filter((c) => c.owner === deepOwner);
    if (mine.length === 0) return;
    setExpanded((s) => new Set(s).add(deepOwner));
    const target = pickConversation(mine, deepConversation);
    if (target) setOpen(target);
  }, [convos, deepOwner, deepConversation]);

  const closeTranscript = () => {
    setOpen(null);
    // Drop the owner segment so the list is the source of truth on a manual back.
    if (deepOwner) nav.go(agent);
  };

  const groups = useMemo<Group[]>(() => {
    if (!convos) return [];
    const map = new Map<string, Group>();
    for (const c of convos) {
      const g = map.get(c.owner);
      const { name, issuer } = parseOwner(c.owner);
      if (g) {
        g.convos.push(c);
        if (!g.named && c.display_name) {
          g.name = c.display_name;
          g.named = true;
        }
        if (c.updated_at > g.updated) g.updated = c.updated_at;
      } else {
        map.set(c.owner, {
          owner: c.owner,
          name: c.display_name || name,
          named: !!c.display_name,
          issuer,
          convos: [c],
          updated: c.updated_at,
        });
      }
    }
    return [...map.values()].sort((a, b) => (a.updated < b.updated ? 1 : -1));
  }, [convos]);

  if (open) {
    return <TranscriptView agent={agent} convo={open} onBack={closeTranscript} />;
  }

  const toggle = (owner: string) =>
    setExpanded((s) => {
      const n = new Set(s);
      n.has(owner) ? n.delete(owner) : n.add(owner);
      return n;
    });

  return (
    <>
      <div className="head-row">
        <div>
          <h2>Conversations</h2>
          <p className="sub">Grouped by customer. {convos ? `${groups.length} customers` : ""}</p>
        </div>
        {agents && agents.length > 1 && (
          <select className="select" value={agent} onChange={(e) => setAgent(e.target.value)}>
            {agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.display_name}
              </option>
            ))}
          </select>
        )}
      </div>
      {error && <p className="err">{error}</p>}
      {notice && <p className="muted">{notice}</p>}
      {!convos && !error && <p className="spinner">Loading…</p>}
      {convos?.length === 0 && (
        <div className="empty">
          <div className="icon">💬</div>
          <div className="empty-title">No conversations yet</div>
          Customer chats will appear here once the widget is used.
        </div>
      )}
      {groups.map((g) => {
        const isOpen = expanded.has(g.owner);
        return (
          <div className="customer-group" key={g.owner}>
            <button className="customer-head" aria-expanded={isOpen} onClick={() => toggle(g.owner)}>
              <span className="avatar" style={{ background: hueFor(g.owner) }}>
                {g.name.slice(0, 1)}
              </span>
              <span className="who">
                <span className="name">{g.name}</span>
                <span className="iss">{g.issuer || "customer"}</span>
              </span>
              <span className="grow" />
              <span className="count-badge">
                {g.convos.length} chat{g.convos.length === 1 ? "" : "s"}
              </span>
              <svg className="chev" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={1.5}>
                <path d="M6 4l4 4-4 4" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </button>
            {isOpen && (
              <div className="convo-list">
                {g.convos.map((c) => (
                  <button className="convo-row" key={c.session} onClick={() => setOpen(c)}>
                    <span className="convo-main">
                      <span className="convo-title">{c.title}</span>
                    </span>
                    <span className="convo-meta">
                      {c.messages} msg · {api.relativeTime(c.updated_at)}
                    </span>
                  </button>
                ))}
                <div className="convo-actions">
                  <button
                    className="btn-sm ghost danger"
                    disabled={forgetting !== null}
                    onClick={() => void forget(g)}
                  >
                    {forgetting === g.owner ? "Forgetting…" : "Forget customer"}
                  </button>
                </div>
              </div>
            )}
          </div>
        );
      })}
    </>
  );
}

function TranscriptView({
  agent,
  convo,
  onBack,
}: {
  agent: string;
  convo: api.Conversation;
  onBack: () => void;
}) {
  const [t, setT] = useState<api.Transcript | null>(null);
  const [error, setError] = useState("");
  const who = parseOwner(convo.owner);
  const name = convo.display_name || t?.display_name || who.name;

  useEffect(() => {
    api.getTranscript(agent, convo.session).then(setT).catch((e) => setError(String(e)));
  }, [agent, convo.session]);

  return (
    <>
      <div className="transcript-head">
        <button className="btn-sm ghost" onClick={onBack}>
          ← Back
        </button>
        <span className="avatar" style={{ width: 34, height: 34, borderRadius: "50%", color: "#fff", fontWeight: 700, display: "flex", alignItems: "center", justifyContent: "center", background: hueFor(convo.owner) }}>
          {name.slice(0, 1).toUpperCase()}
        </span>
        <div>
          <h2 style={{ marginBottom: 1 }}>{convo.title}</h2>
          <p className="sub" style={{ marginBottom: 0 }}>
            {name} · {who.issuer} · {convo.messages} messages
          </p>
        </div>
      </div>
      {error && <p className="err">{error}</p>}
      {!t && !error && <p className="spinner">Loading…</p>}
      <div className="transcript">
        {t?.messages.map((m, i) => (
          <div className={`chat-row ${m.role}`} key={i}>
            <div className="chat-bubble">
              {m.role === "assistant" ? <Markdown text={m.content} /> : m.content}
            </div>
          </div>
        ))}
        {t?.messages.length === 0 && <p className="muted">No messages.</p>}
      </div>
    </>
  );
}
