import { useEffect, useState } from "react";

import * as api from "./api";
import {
  concreteTools,
  emptyServerRow,
  exposedName,
  GRANT_SOURCES,
  integrationEdit,
  integrationForm,
  names,
  toggleName,
  type GrantRow,
  type IntegrationForm,
  type McpConnectionRow,
  type McpServerRow,
} from "./agentForm";
import { AgentTraining } from "./AgentTraining";
import { History } from "./History";
import { contrast, resolveBrand, resolveGradient, themeMode, type Resolved } from "./brandTheme";
import { GradientBar, stopsOf } from "./GradientBar";
import { SurfacePaintFields } from "./SurfacePaintFields";

const lines = (v: string) =>
  v
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);

const TABS: [Tab, string][] = [
  ["persona", "Persona"],
  ["access", "Access & tools"],
  ["integration", "Integration"],
  ["appearance", "Appearance"],
  ["training", "Training"],
];

// "Sync from git" — hot-apply the config repo without a restart (git-managed
// installs). The edge also polls automatically; this is the instant option.
function SyncButton({ onSynced }: { onSynced: () => void }) {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const sync = async () => {
    setBusy(true);
    setMsg(null);
    try {
      const r = await api.syncGit();
      setMsg(
        r.changed
          ? `Applied ${r.commit.slice(0, 7)} — ${r.agents} agent(s), ${r.training} training doc(s)`
          : "Already up to date",
      );
      if (r.changed) onSynced();
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <span style={{ display: "flex", gap: 12, alignItems: "center" }}>
      {msg && (
        <span className="sub" style={{ fontSize: 12 }}>
          {msg}
        </span>
      )}
      <button className="btn-sm" onClick={sync} disabled={busy} title="Re-fetch the config repo and apply it now">
        {busy ? "Syncing…" : "Sync from git"}
      </button>
    </span>
  );
}

const HEX = /^#[0-9a-fA-F]{6}$/;

// The identity files, with operator-facing guidance for each.
const FILES: { key: keyof api.IdentityFiles; name: string; blurb: string; ph: string }[] = [
  { key: "soul", name: "SOUL.md", blurb: "Personality, values, and voice — who the agent is at its core.",
    ph: "You are warm, concise, and never condescending. You value clarity over jargon…" },
  { key: "identity", name: "IDENTITY.md", blurb: "Name, role, and remit — what it does and for whom.",
    ph: "You are the support assistant for Acme's ordering app. You help customers track and change orders…" },
  { key: "agents", name: "AGENTS.md", blurb: "Operational rules and guidelines — how it should behave.",
    ph: "Always confirm before taking an action. If you don't know, say so and offer to escalate…" },
  { key: "tools", name: "TOOLS.md", blurb: "Notes on the tools it can use and when.",
    ph: "Use list_orders to look up stock. Never expose internal ids to the user…" },
];

export function Agents({ nav }: { nav: import("./main").Nav }) {
  const [agents, setAgents] = useState<api.Agent[] | null>(null);
  const [error, setError] = useState("");
  const [history, setHistory] = useState<string | null>(null);

  // /admin/agents/<id> edits an agent; /admin/agents/new creates one.
  const param = nav.params[0] ?? null;
  const creating = param === "new";
  const editingId = param && !creating ? param : null;

  const load = () => api.listAgents().then(setAgents).catch((e) => setError(String(e)));
  useEffect(() => {
    void load();
  }, []);

  const done = () => {
    nav.go();
    void load();
  };

  if (creating) return <AgentEditor mode="create" onDone={done} onCancel={() => nav.go()} />;
  const edit = agents?.find((a) => a.id === editingId);
  if (edit) return <AgentEditor mode="edit" agent={edit} onDone={done} onCancel={() => nav.go()} />;

  return (
    <>
      <div className="head-row">
        <div>
          <h2>Agents</h2>
          <p className="sub">The customer-facing agents this install serves. Edits apply live.</p>
        </div>
        {api.isGitManaged() ? (
          <SyncButton onSynced={load} />
        ) : (
          <button className="btn-sm" onClick={() => nav.go("new")}>
            + New agent
          </button>
        )}
      </div>
      {api.isGitManaged() && (
        <p className="banner-info">
          Agents, playbooks, and training data are managed in git — view only. Edit them in the
          config repository; changes apply automatically, or hit “Sync from git”.
        </p>
      )}
      {error && <p className="err">Couldn't load agents.</p>}
      {!agents && !error && <p className="spinner">Loading…</p>}
      {history && (
        <History
          kind="agent"
          item={history}
          onClose={() => setHistory(null)}
          onRestored={() => {
            setHistory(null);
            void load();
          }}
        />
      )}
      {!history &&
        agents?.map((a) => (
          <AgentCard
            key={a.id}
            agent={a}
            solo={agents.length <= 1}
            onEdit={() => nav.go(a.id)}
            onHistory={() => setHistory(a.id)}
            onDeleted={() => void load()}
          />
        ))}
    </>
  );
}

function swatch(hex?: string | null) {
  return hex && HEX.test(hex) ? (
    <span style={{ display: "inline-block", width: 12, height: 12, borderRadius: 3, background: hex, border: "1px solid var(--border-2)", verticalAlign: "-1px", marginRight: 5 }} />
  ) : null;
}

function AgentCard({
  agent: a,
  solo,
  onEdit,
  onHistory,
  onDeleted,
}: {
  agent: api.Agent;
  solo: boolean;
  onEdit: () => void;
  onHistory: () => void;
  onDeleted: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const persona = a.identity.identity || a.identity.soul || "—";
  const primary = a.branding.primary_color ?? a.branding.accent_color;
  return (
    <div className="card">
      <div className="row">
        <h3>
          {swatch(primary)}
          {a.display_name}
        </h3>
        <span className="id">{a.id}</span>
      </div>
      <dl className="kv">
        <dt>Persona</dt>
        <dd className="clamp">{persona}</dd>
        <dt>Model</dt>
        <dd>{a.model ?? <span className="muted">install default</span>}</dd>
        <dt>Company tools</dt>
        <dd>{a.has_mcp ? "MCP connected" : <span className="muted">pure chat</span>}</dd>
        <dt>Access</dt>
        <dd>
          {a.allowed_origins.length ? `${a.allowed_origins.length} origin(s)` : <span className="muted">no origins</span>}
          {" · "}
          {a.allowed_emails.length ? `${a.allowed_emails.length} allow-listed` : "any verified email"}
        </dd>
      </dl>
      <div className="actions">
        <button className="btn-sm" onClick={onEdit}>
          Edit
        </button>
        <button className="btn-sm ghost" onClick={onHistory}>
          History
        </button>
        <button
          className="btn-sm ghost danger"
          disabled={busy || solo || api.isGitManaged()}
          title={
            api.isGitManaged()
              ? "Managed in git"
              : solo
                ? "An install needs at least one agent"
                : ""
          }
          onClick={async () => {
            if (!window.confirm(`Delete agent "${a.display_name}"? Its training data, learned lessons, and conversation memory are all removed too. This can't be undone.`)) return;
            setBusy(true);
            try {
              await api.deleteAgent(a.id);
              onDeleted();
            } catch (e) {
              alert(e instanceof Error ? e.message : String(e));
              setBusy(false);
            }
          }}
        >
          Delete
        </button>
      </div>
    </div>
  );
}

type Tab = "persona" | "access" | "integration" | "appearance" | "training";

/** Per-user write tiers: one row per grant, tools as free text. */
function GrantRows({
  rows,
  onChange,
  readOnly,
}: {
  rows: GrantRow[];
  onChange: (rows: GrantRow[]) => void;
  readOnly: boolean;
}) {
  const patch = (i: number, p: Partial<GrantRow>) => onChange(rows.map((g, j) => (j === i ? { ...g, ...p } : g)));
  return (
    <div>
      {rows.map((g, i) => (
        <div className="grant-row" key={i}>
          <select
            className="select"
            value={g.source}
            disabled={readOnly}
            onChange={(e) => patch(i, { source: e.target.value as api.GrantSource })}
          >
            {GRANT_SOURCES.map(([v, label]) => (
              <option key={v} value={v}>
                {label}
              </option>
            ))}
          </select>
          <input value={g.requires} placeholder="items:edit" onChange={(e) => patch(i, { requires: e.target.value })} />
          <input
            className="mono-area"
            value={g.tools}
            placeholder="tools unlocked, space-separated"
            onChange={(e) => patch(i, { tools: e.target.value })}
          />
          {!readOnly && (
            <button className="btn-sm ghost" type="button" onClick={() => onChange(rows.filter((_, j) => j !== i))}>
              Remove
            </button>
          )}
        </div>
      ))}
      {!readOnly && (
        <button
          className="btn-sm ghost"
          type="button"
          onClick={() => onChange([...rows, { source: "backend_permissions", requires: "", tools: "" }])}
        >
          + Add grant
        </button>
      )}
    </div>
  );
}

/** One MCP server's connection: URL, write-only credential, how it
 *  authenticates, and how its being unavailable is handled. Rendered as
 *  label/control pairs inside a `form-grid`. */
function McpConnectionFields({
  value,
  onChange,
  readOnly,
  urlLabel,
  urlHint,
}: {
  value: McpConnectionRow;
  onChange: (value: McpConnectionRow) => void;
  readOnly: boolean;
  urlLabel: string;
  urlHint: string;
}) {
  const set = <K extends keyof McpConnectionRow>(k: K, v: McpConnectionRow[K]) => onChange({ ...value, [k]: v });
  const off = readOnly || !value.url.trim();
  return (
    <>
      <label>
        {urlLabel}
        <span className="hint">{urlHint}</span>
      </label>
      <input value={value.url} disabled={readOnly} onChange={(e) => set("url", e.target.value)} placeholder="https://mcp.company.com" />
      <label>
        Discovery credential
        <span className="hint">service account for listing tools at start; write-only</span>
      </label>
      <div>
        <div className="op-add" style={{ margin: 0 }}>
          <span className={`pill ${value.has_credential ? "pill-domain" : "pill-neutral"}`}>
            {value.has_credential ? "set" : "not set"}
          </span>
          <input
            type="password"
            autoComplete="off"
            value={value.credential}
            disabled={off || value.clear_credential}
            placeholder={value.has_credential ? "Replace on save" : "Set on save"}
            onChange={(e) => set("credential", e.target.value)}
          />
        </div>
        {value.has_credential && (
          <label className="check" style={{ marginTop: 8 }}>
            <input
              type="checkbox"
              checked={value.clear_credential}
              disabled={readOnly}
              onChange={(e) => set("clear_credential", e.target.checked)}
            />
            Clear the stored credential on save
          </label>
        )}
      </div>
      <label>
        Customer token
        <span className="hint">exchanged for this server's audience, or always the customer's own</span>
      </label>
      <div className="op-add" style={{ margin: 0 }}>
        <select
          className="select"
          value={value.auth}
          disabled={off}
          onChange={(e) => set("auth", e.target.value as api.McpAuth)}
        >
          <option value="exchanged">Exchanged</option>
          <option value="forwarded">Forwarded</option>
        </select>
        <input
          value={value.audience}
          disabled={off || value.auth !== "exchanged"}
          placeholder="audience (blank = the install's)"
          onChange={(e) => set("audience", e.target.value)}
        />
      </div>
      <label>
        When unavailable
        <span className="hint">required: the turn fails; otherwise only this server's tools are missing</span>
      </label>
      <div className="op-add" style={{ margin: 0 }}>
        <label className="check">
          <input
            type="checkbox"
            checked={value.required}
            disabled={off}
            onChange={(e) => set("required", e.target.checked)}
          />
          Required
        </label>
        <input
          type="number"
          min={0}
          value={value.unavailable_skip_secs}
          disabled={off || value.required}
          title="seconds a timed-out server sits out; 0 never skips"
          onChange={(e) => set("unavailable_skip_secs", e.target.value)}
        />
      </div>
    </>
  );
}

/** The additional MCP servers, one bordered row each, with a Discover button
 *  that lists the server's tools under its prefix for picking. */
function McpServerList({
  rows,
  onChange,
  readOnly,
}: {
  rows: McpServerRow[];
  onChange: (rows: McpServerRow[]) => void;
  readOnly: boolean;
}) {
  const [surveys, setSurveys] = useState<Record<number, api.McpSurvey | string>>({});
  const patch = (i: number, p: Partial<McpServerRow>) => onChange(rows.map((r, j) => (j === i ? { ...r, ...p } : r)));
  const discover = async (i: number) => {
    const row = rows[i];
    try {
      const survey = await api.discoverMcp({
        url: row.url.trim(),
        ...(row.credential.trim() ? { discovery_credential: row.credential.trim() } : {}),
      });
      setSurveys((s) => ({ ...s, [i]: survey }));
    } catch (err) {
      setSurveys((s) => ({ ...s, [i]: err instanceof Error ? err.message : String(err) }));
    }
  };
  return (
    <div>
      {rows.map((row, i) => {
        const survey = surveys[i];
        return (
          <div className="server-row" key={i}>
            <div className="form-grid">
              <label>
                Id<span className="hint">names the server in logs and errors</span>
              </label>
              <input value={row.id} disabled={readOnly} placeholder="docs" onChange={(e) => patch(i, { id: e.target.value })} />
              <McpConnectionFields
                value={row}
                onChange={(c) => patch(i, c)}
                readOnly={readOnly}
                urlLabel="MCP URL"
                urlHint="where this server's tools live"
              />
              <label>
                Tool prefix<span className="hint">optional; tools are named prefix.tool</span>
              </label>
              <input
                value={row.tool_prefix}
                disabled={readOnly}
                placeholder="docs"
                onChange={(e) => patch(i, { tool_prefix: e.target.value })}
              />
              <label>
                Allowed tools<span className="hint">in the names the agent sees; prefix.* allows them all</span>
              </label>
              <div>
                <textarea
                  className="mono-area"
                  rows={2}
                  value={row.allowed_tools}
                  disabled={readOnly}
                  onChange={(e) => patch(i, { allowed_tools: e.target.value })}
                />
                {!readOnly && (
                  <button
                    className="btn-sm ghost"
                    type="button"
                    disabled={!row.url.trim()}
                    onClick={() => void discover(i)}
                  >
                    Discover tools
                  </button>
                )}
                {typeof survey === "string" && <p className="err">{survey}</p>}
                {survey && typeof survey !== "string" && (
                  <div className="tool-list" style={{ marginTop: 8 }}>
                    {survey.tools.map((t) => {
                      const name = exposedName(row.tool_prefix, t.name);
                      return (
                        <label key={t.name} className="tool-row">
                          <input
                            type="checkbox"
                            checked={names(row.allowed_tools).includes(name)}
                            onChange={(e) => patch(i, { allowed_tools: toggleName(row.allowed_tools, name, e.target.checked) })}
                          />
                          <span>
                            <span className="tool-name">{name}</span>
                            {t.description && <div className="tool-desc">{t.description}</div>}
                          </span>
                        </label>
                      );
                    })}
                  </div>
                )}
              </div>
              <label>
                Tool grants<span className="hint">write tiers on this server, unlocked per user</span>
              </label>
              <GrantRows rows={row.tool_grants} onChange={(g) => patch(i, { tool_grants: g })} readOnly={readOnly} />
            </div>
            {!readOnly && (
              <button
                className="btn-sm ghost"
                type="button"
                onClick={() => {
                  setSurveys({});
                  onChange(rows.filter((_, j) => j !== i));
                }}
              >
                Remove server
              </button>
            )}
          </div>
        );
      })}
      {!readOnly && (
        <button className="btn-sm ghost" type="button" onClick={() => onChange([...rows, emptyServerRow()])}>
          + Add server
        </button>
      )}
    </div>
  );
}

function AgentEditor({
  mode,
  agent,
  onDone,
  onCancel,
}: {
  mode: "create" | "edit";
  agent?: api.Agent;
  onDone: () => void;
  onCancel: () => void;
}) {
  const isNew = mode === "create";
  const [tab, setTab] = useState<Tab>("persona");
  const [file, setFile] = useState<keyof api.IdentityFiles>("identity");

  const [id, setId] = useState(agent?.id ?? "");
  const [displayName, setDisplayName] = useState(agent?.display_name ?? "");
  const [identity, setIdentity] = useState<api.IdentityFiles>(
    agent?.identity ?? { soul: "", identity: "", agents: "", tools: "" },
  );
  const [welcome, setWelcome] = useState(agent?.welcome ?? "");
  const [model, setModel] = useState(agent?.model ?? "");
  const [issuer, setIssuer] = useState(agent?.keycloak_issuer ?? "");
  const [audience, setAudience] = useState(agent?.keycloak_audience ?? "");
  const [origins, setOrigins] = useState((agent?.allowed_origins ?? []).join("\n"));
  const [emails, setEmails] = useState((agent?.allowed_emails ?? []).join("\n"));
  const [tools, setTools] = useState((agent?.allowed_tools ?? []).join("\n"));
  const [dataTables, setDataTables] = useState(agent?.data_tables ?? false);
  const [charts, setCharts] = useState(agent?.charts ?? false);
  const [forms, setForms] = useState(agent?.forms ?? false);
  const [maps, setMaps] = useState(agent?.maps ?? false);
  const [diagrams, setDiagrams] = useState(agent?.diagrams ?? false);
  const [excalidrawExport, setExcalidrawExport] = useState(agent?.excalidraw_export ?? false);
  const [inlineImages, setInlineImages] = useState(agent?.inline_images ?? false);
  const [images, setImages] = useState(agent?.images ?? false);
  const [uploads, setUploads] = useState<api.UploadKind[]>(agent?.uploads ?? []);
  const [uploadMaxMb, setUploadMaxMb] = useState(String(agent?.upload_max_mb ?? 10));
  const [drawing, setDrawing] = useState(agent?.drawing ?? false);
  const toggleUpload = (kind: api.UploadKind) =>
    setUploads((cur) => (cur.includes(kind) ? cur.filter((k) => k !== kind) : [...cur, kind]));
  const [integ, setInteg] = useState<IntegrationForm>(() => integrationForm(agent));
  const setInt = <K extends keyof IntegrationForm>(k: K, v: IntegrationForm[K]) =>
    setInteg((p) => ({ ...p, [k]: v }));
  const pickable = concreteTools(lines(tools), integ.tool_grants, integ.mcp_servers);
  const [b, setB] = useState<api.Branding>({ ...(agent?.branding ?? {}) });
  // Git-managed installs: everything but Appearance is read-only (edit in git).
  const readOnly = api.isGitManaged() && !isNew;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const setBrand = (k: keyof api.Branding, v: string) => setB((p) => ({ ...p, [k]: v || null }));
  /** A px size: an integer, or null to fall back to the default. */
  const setSize = (k: keyof api.Branding, v: string) =>
    setB((p) => {
      const n = Number.parseInt(v, 10);
      return { ...p, [k]: v.trim() === "" || Number.isNaN(n) ? null : n };
    });
  // Gradient and dark palette are nested objects, so they patch a sub-object
  // rather than a top-level key. Clearing the last field of either drops the whole
  // block to null, which is what the server reads as "no gradient / no overrides"
  // — leaving an empty object behind would be a silent no-op the operator can't see.
  const setGrad = (k: keyof api.Gradient, v: string) =>
    setB((p) => {
      const next: api.Gradient = { ...(p.gradient ?? {}), [k]: v || null };
      const empty = !next.from && !next.via && !next.to;
      return { ...p, gradient: empty ? null : next };
    });
  const setDark = (k: keyof api.DarkPalette, v: string) =>
    setB((p) => {
      const next: api.DarkPalette = { ...(p.dark ?? {}), [k]: v || null };
      const empty = Object.values(next).every((x) => !x);
      return { ...p, dark: empty ? null : next };
    });
  // The stop editor replaces the whole list. Writing `stops` is what makes the drag
  // authoritative: the server prefers it over from/via/to as soon as two are usable.
  const setGradStops = (stops: api.GradientStop[]) =>
    setB((p) => ({ ...p, gradient: { ...(p.gradient ?? {}), stops } }));
  const setDarkGradStops = (stops: api.GradientStop[]) =>
    setB((p) => ({
      ...p,
      dark: { ...(p.dark ?? {}), gradient: { ...(p.dark?.gradient ?? {}), stops } },
    }));
  const setDarkGrad = (k: keyof api.Gradient, v: string) =>
    setB((p) => {
      const cur = p.dark ?? {};
      const g: api.Gradient = { ...(cur.gradient ?? {}), [k]: v || null };
      const gEmpty = !g.from && !g.via && !g.to;
      const next: api.DarkPalette = { ...cur, gradient: gEmpty ? null : g };
      const empty = Object.values(next).every((x) => !x);
      return { ...p, dark: empty ? null : next };
    });

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    setError("");
    if (lines(emails).length === 0 &&
      !window.confirm("No allowed emails set — ANY verified account from the identity provider can chat with this agent. Continue?")) {
      return;
    }
    setBusy(true);
    try {
      if (isNew) {
        await api.createAgent({
          id: id.trim(),
          display_name: displayName.trim(),
          identity,
          welcome,
          model: model.trim() || null,
          allowed_origins: lines(origins),
          allowed_emails: lines(emails),
          keycloak_issuer: issuer.trim(),
          keycloak_audience: audience.trim() || null,
          allowed_tools: lines(tools),
          branding: b,
          data_tables: dataTables,
          charts,
          forms,
          maps,
          diagrams,
          excalidraw_export: excalidrawExport,
          inline_images: inlineImages,
          images,
          uploads,
          upload_max_mb: Number(uploadMaxMb) || null,
          drawing,
          ...integrationEdit(integ),
        });
      } else {
        await api.updateAgent(agent!.id, {
          display_name: displayName,
          identity,
          welcome,
          model: model.trim() || null,
          allowed_origins: lines(origins),
          allowed_emails: lines(emails),
          allowed_tools: lines(tools),
          keycloak_issuer: issuer.trim(),
          keycloak_audience: audience.trim() || null,
          branding: b,
          data_tables: dataTables,
          charts,
          forms,
          maps,
          diagrams,
          excalidraw_export: excalidrawExport,
          inline_images: inlineImages,
          images,
          uploads,
          upload_max_mb: Number(uploadMaxMb) || null,
          drawing,
          ...integrationEdit(integ),
        });
      }
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };

  return (
    <form className="card editing" onSubmit={save}>
      <div className="editor-head">
        <h3>{isNew ? "New agent" : `Editing ${agent!.display_name}`}</h3>
        <div className="actions" style={{ marginTop: 0 }}>
          <button className="btn-sm ghost" type="button" onClick={onCancel} disabled={busy}>
            {readOnly ? "Close" : "Cancel"}
          </button>
          {!readOnly && (
            <button className="btn-sm" type="submit" disabled={busy}>
              {busy ? "Applying…" : isNew ? "Create agent" : "Save changes"}
            </button>
          )}
        </div>
      </div>
      {error && <p className="err">{error}</p>}
      {readOnly && (
        <p className="banner-info">
          This agent is managed in git — view only. Edit its config, appearance, and training in the
          config repository, then it applies automatically (or hit “Sync from git”).
        </p>
      )}

      <div className="tabs">
        {TABS.filter(([t]) => t !== "training" || !isNew).map(([t, label]) => (
          <button
            key={t}
            type="button"
            className={tab === t ? "tab active" : "tab"}
            onClick={() => setTab(t)}
          >
            {label}
          </button>
        ))}
      </div>
      {tab === "training" && !isNew && <AgentTraining agentId={agent!.id} />}

      {/* View-only in git mode: inputs are non-editable but navigation (the
          identity-file tabs) still works — a `disabled` fieldset would disable
          those buttons too. */}
      <fieldset
        className={readOnly ? "view-only" : undefined}
        style={{ border: 0, padding: 0, margin: 0, minInlineSize: "auto" }}
      >
      {tab === "persona" && (
        <div>
          {isNew && (
            <div className="form-grid" style={{ marginBottom: 16 }}>
              <label>
                Agent id<span className="hint">lowercase, dashes</span>
              </label>
              <input value={id} onChange={(e) => setId(e.target.value.toLowerCase())} placeholder="support" />
              <label>Display name</label>
              <input value={displayName} onChange={(e) => setDisplayName(e.target.value)} placeholder="Support Assistant" />
            </div>
          )}
          {!isNew && (
            <div className="form-grid" style={{ marginBottom: 16 }}>
              <label>Display name</label>
              <input value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
            </div>
          )}
          <label style={{ display: "block", marginBottom: 8 }}>
            Identity files
            <span className="hint">the agent's persona, assembled into its system prompt</span>
          </label>
          <div className="filetabs">
            {FILES.map((f) => (
              <button key={f.key} type="button" className={file === f.key ? "filetab active" : "filetab"} onClick={() => setFile(f.key)}>
                {f.name}
                {identity[f.key].trim() && <span className="ok-dot" />}
              </button>
            ))}
          </div>
          {FILES.filter((f) => f.key === file).map((f) => (
            <div key={f.key}>
              <p className="filehint">{f.blurb}</p>
              <textarea
                className="mono-area"
                rows={12}
                value={identity[f.key]}
                placeholder={f.ph}
                onChange={(e) => setIdentity((p) => ({ ...p, [f.key]: e.target.value }))}
              />
            </div>
          ))}
          <div className="form-grid" style={{ marginTop: 16 }}>
            <label>
              Welcome message<span className="hint">WELCOME.md — greets the customer</span>
            </label>
            <textarea rows={2} value={welcome} onChange={(e) => setWelcome(e.target.value)} placeholder="Hi! How can I help today?" />
            <label>
              Model<span className="hint">blank = install default</span>
            </label>
            <input value={model} onChange={(e) => setModel(e.target.value)} placeholder="standard / powerful / a model id" />
          </div>
        </div>
      )}

      {tab === "access" && (
        <div className="form-grid">
          <label>
            Identity provider<span className="hint">OIDC issuer that signs customer tokens</span>
          </label>
          <input value={issuer} onChange={(e) => setIssuer(e.target.value)} placeholder="https://accounts.google.com" />
          <label>
            Audience<span className="hint">optional; pins tokens to your app</span>
          </label>
          <input value={audience} onChange={(e) => setAudience(e.target.value)} placeholder="your-oauth-client-id" />
          <label>
            Allowed origins<span className="hint">one per line; sites that may embed the widget</span>
          </label>
          <textarea rows={3} value={origins} onChange={(e) => setOrigins(e.target.value)} placeholder="https://app.company.com" />
          <label>
            Allowed emails<span className="hint">one per line; empty = any verified account</span>
          </label>
          <textarea rows={3} value={emails} onChange={(e) => setEmails(e.target.value)} />
          <label>
            Allowed tools<span className="hint">one per line; exact or prefix.* — empty = no tools</span>
          </label>
          <textarea className="mono-area" rows={3} value={tools} onChange={(e) => setTools(e.target.value)} placeholder="acme.list_orders&#10;acme.*" />
          <label>
            Rich output<span className="hint">interactive blocks the widget may render</span>
          </label>
          <div className="check-row">
            <label className="check">
              <input
                type="checkbox"
                checked={dataTables}
                disabled={readOnly}
                onChange={(e) => setDataTables(e.target.checked)}
              />
              Data tables (wf-table)
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={charts}
                disabled={readOnly}
                onChange={(e) => setCharts(e.target.checked)}
              />
              Charts (wf-chart)
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={forms}
                disabled={readOnly}
                onChange={(e) => setForms(e.target.checked)}
              />
              Forms (wf-form)
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={maps}
                disabled={readOnly}
                onChange={(e) => setMaps(e.target.checked)}
              />
              Maps (wf-map)
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={diagrams}
                disabled={readOnly}
                onChange={(e) => setDiagrams(e.target.checked)}
              />
              Diagrams (wf-diagram)
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={excalidrawExport}
                disabled={readOnly || !diagrams}
                onChange={(e) => setExcalidrawExport(e.target.checked)}
              />
              Excalidraw download
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={inlineImages}
                disabled={readOnly}
                onChange={(e) => setInlineImages(e.target.checked)}
              />
              Show images inline
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={images}
                disabled={readOnly}
                onChange={(e) => setImages(e.target.checked)}
              />
              Agent can show pictures
            </label>
          </div>
          <label>
            Customer uploads<span className="hint">what a customer may attach; the server enforces it</span>
          </label>
          <div className="checks">
            {(["image", "spreadsheet", "document"] as api.UploadKind[]).map((kind) => (
              <label className="check" key={kind}>
                <input
                  type="checkbox"
                  checked={uploads.includes(kind)}
                  disabled={readOnly}
                  onChange={() => toggleUpload(kind)}
                />
                {kind === "image" ? "Images" : kind === "spreadsheet" ? "Spreadsheets (xlsx, csv)" : "Documents (pdf, docx, txt)"}
              </label>
            ))}
            <label className="check">
              <input
                type="checkbox"
                checked={drawing}
                disabled={readOnly || !uploads.includes("image")}
                onChange={(e) => setDrawing(e.target.checked)}
              />
              Drawing canvas{!uploads.includes("image") && " (needs Images)"}
            </label>
          </div>
          <label>
            Max upload size (MB)<span className="hint">1–25; the server clamps it</span>
          </label>
          <input
            type="number"
            min={1}
            max={25}
            value={uploadMaxMb}
            disabled={readOnly || uploads.length === 0}
            onChange={(e) => setUploadMaxMb(e.target.value)}
          />
        </div>
      )}

      {tab === "integration" && (
        <div className="form-grid">
          <McpConnectionFields
            value={integ.mcp}
            onChange={(mcp) => setInt("mcp", mcp)}
            readOnly={readOnly}
            urlLabel="Company MCP URL"
            urlHint="the primary server the agent's tools live on; blank = pure chat"
          />
          <label>
            Additional MCP servers
            <span className="hint">more servers alongside the primary, each with its own allowlist</span>
          </label>
          <McpServerList
            rows={integ.mcp_servers}
            onChange={(rows) => setInt("mcp_servers", rows)}
            readOnly={readOnly || !integ.mcp.url.trim()}
          />
          <label>
            Permissions URL
            <span className="hint">resolves backend permission grants; {"{user}"} becomes the signed-in user</span>
          </label>
          <input
            value={integ.permissions_url}
            onChange={(e) => setInt("permissions_url", e.target.value)}
            placeholder="https://api.company.com/users/{user}/permissions"
          />
          <label>
            Permissions cache<span className="hint">seconds per session; blank = 300</span>
          </label>
          <input
            type="number"
            min={1}
            value={integ.permissions_ttl}
            disabled={readOnly || !integ.permissions_url.trim()}
            onChange={(e) => setInt("permissions_ttl", e.target.value)}
          />
          <label>
            Tool grants
            <span className="hint">write tiers unlocked per user, on top of the allowed tools</span>
          </label>
          <GrantRows
            rows={integ.tool_grants}
            onChange={(rows) => setInt("tool_grants", rows)}
            readOnly={readOnly}
          />
          <label>Write policy</label>
          <label className="check">
            <input
              type="checkbox"
              checked={integ.confirm_writes}
              disabled={readOnly}
              onChange={(e) => setInt("confirm_writes", e.target.checked)}
            />
            Confirm in chat before any granted write
          </label>
          <label>
            Image hosts
            <span className="hint">one per line; where image.fetch may pull from — .example.com allows subdomains</span>
          </label>
          <textarea
            className="mono-area"
            rows={3}
            value={integ.image_hosts}
            onChange={(e) => setInt("image_hosts", e.target.value)}
            placeholder="cdn.company.com&#10;.files.company.com"
          />
          <label>
            Read verbatim
            <span className="hint">tools whose results are never spilled to a handle (signed URLs, tokens)</span>
          </label>
          <div className="check-row" style={{ paddingTop: 6 }}>
            {pickable.length === 0 && (
              <span className="muted">Add exact tool names under Access &amp; tools to pick them here.</span>
            )}
            {pickable.map((t) => (
              <label className="check" key={t}>
                <input
                  type="checkbox"
                  checked={integ.spill_skip_tools.includes(t)}
                  disabled={readOnly}
                  onChange={(e) =>
                    setInt(
                      "spill_skip_tools",
                      e.target.checked
                        ? [...integ.spill_skip_tools, t]
                        : integ.spill_skip_tools.filter((x) => x !== t),
                    )
                  }
                />
                <span className="mono-area">{t}</span>
              </label>
            ))}
          </div>
          <label>Passive memory</label>
          <label className="check">
            <input
              type="checkbox"
              checked={integ.passive_memory}
              disabled={readOnly}
              onChange={(e) => setInt("passive_memory", e.target.checked)}
            />
            Learn durable facts about each customer across conversations
          </label>
        </div>
      )}

      {tab === "appearance" && (
        <div className="appearance">
          <div className="appearance-fields">
            <div className="field-group">
              <div className="group-title">Header</div>
              <div className="form-grid">
                <label>Title</label>
                <input value={b.title ?? ""} onChange={(e) => setBrand("title", e.target.value)} placeholder={displayName} />
                <label>Subtitle</label>
                <input value={b.subtitle ?? ""} onChange={(e) => setBrand("subtitle", e.target.value)} />
                <label>Logo URL</label>
                <input value={b.logo_url ?? ""} onChange={(e) => setBrand("logo_url", e.target.value)} placeholder="https://… or data:image/…" />
                <label>Launcher</label>
                <select className="select" value={b.launcher ?? "bubble"} onChange={(e) => setBrand("launcher", e.target.value)}>
                  <option value="bubble">Bubble (icon)</option>
                  <option value="pill">Pill (labelled)</option>
                  <option value="expand">Expanding (label on hover)</option>
                </select>
                <label>
                  Agent avatar<span className="hint">shows the logo beside each reply</span>
                </label>
                <select className="select" value={b.avatar === true ? "on" : "off"} onChange={(e) => setB((p) => ({ ...p, avatar: e.target.value === "on" }))}>
                  <option value="off">Off</option>
                  <option value="on">On</option>
                </select>
                <label>
                  Logo fit<span className="hint">contain suits a bare mark on transparency</span>
                </label>
                <select className="select" value={b.logo_fit === "contain" ? "contain" : "cover"} onChange={(e) => setBrand("logo_fit", e.target.value)}>
                  <option value="cover">Cover (rounded tile)</option>
                  <option value="contain">Contain (no crop)</option>
                </select>
                <label>Dock side</label>
                <select className="select" value={b.position ?? "right"} onChange={(e) => setBrand("position", e.target.value)}>
                  <option value="right">Right</option>
                  <option value="left">Left</option>
                </select>
                <label>
                  Reposition<span className="hint">lets customers drag the widget to another corner</span>
                </label>
                <select className="select" value={b.draggable === true ? "on" : "off"} onChange={(e) => setB((p) => ({ ...p, draggable: e.target.value === "on" }))}>
                  <option value="off">Fixed</option>
                  <option value="on">Draggable</option>
                </select>
                <label>
                  Dock inset<span className="hint">px in from the corner, across × up — clears a sticky bar</span>
                </label>
                <span className="size-row">
                  <Px v={b.offset_x} d={0} onChange={(v) => setSize("offset_x", v)} />
                  <span className="size-x">×</span>
                  <Px v={b.offset_y} d={0} onChange={(v) => setSize("offset_y", v)} />
                </span>
              </div>
            </div>
            <div className="field-group">
              <div className="group-title">Colours</div>
              <div className="form-grid">
                <label>Primary</label>
                <Color v={b.primary_color} d="#4f46e5" onChange={(v) => setBrand("primary_color", v)} />
                <label>Primary text</label>
                <Color v={b.primary_text} d="#ffffff" onChange={(v) => setBrand("primary_text", v)} />
                <label>Customer bubble</label>
                <Color v={b.user_bubble} d="#4f46e5" onChange={(v) => setBrand("user_bubble", v)} />
                <label>Customer text</label>
                <Color v={b.user_bubble_text} d="#ffffff" onChange={(v) => setBrand("user_bubble_text", v)} />
                <label>Agent bubble</label>
                <Color v={b.agent_bubble} d="#f1f2f6" onChange={(v) => setBrand("agent_bubble", v)} />
                <label>Agent text</label>
                <Color v={b.agent_bubble_text} d="#10131a" onChange={(v) => setBrand("agent_bubble_text", v)} />
                <label>
                  Message box<span className="hint">the input; defaults to the panel</span>
                </label>
                <Color v={b.input_bg} d={b.panel || "#ffffff"} onChange={(v) => setBrand("input_bg", v)} />
                <label>
                  Message box border<span className="hint">defaults to the hairline</span>
                </label>
                <Color v={b.input_border} d={b.border || "#e6eaf0"} onChange={(v) => setBrand("input_border", v)} />
                <label>
                  Agent bubble border<span className="hint">defaults to the hairline</span>
                </label>
                <Color
                  v={b.agent_bubble_border}
                  d={b.border || "#e6eaf0"}
                  onChange={(v) => setBrand("agent_bubble_border", v)}
                />
              </div>
            </div>
            <GradientFields
              g={b.gradient}
              onChange={setGrad}
              onStops={setGradStops}
              headerText={b.primary_text ?? "#ffffff"}
              title="Brand gradient"
              hint="Painted on the launcher, header, send button, avatar and panel ring. Leave the stops empty for a flat Primary."
            />
            <SurfacePaintFields
              title="Agent icon"
              hint="the mark beside each reply"
              paint={b.avatar_paint}
              brand={b.gradient}
              onChange={(p) => setB((prev) => ({ ...prev, avatar_paint: p }))}
            />
            <SurfacePaintFields
              title="Send button"
              hint="independent of the icon"
              paint={b.send_paint}
              brand={b.gradient}
              onChange={(p) => setB((prev) => ({ ...prev, send_paint: p }))}
            />
            <div className="field-group">
              <div className="group-title">Colour scheme</div>
              <div className="form-grid">
                <label>
                  Mode
                  <span className="hint">auto follows the host page's own light/dark</span>
                </label>
                <select className="select" value={themeMode(b)} onChange={(e) => setBrand("theme_mode", e.target.value)}>
                  <option value="light">Light only</option>
                  <option value="dark">Dark only</option>
                  <option value="auto">Auto (follow host page)</option>
                </select>
              </div>
              {themeMode(b) === "light" ? (
                <p className="hint" style={{ margin: "8px 2px 0" }}>
                  The dark palette below only paints in <strong>Dark</strong> or <strong>Auto</strong>.
                </p>
              ) : (
                <p className="hint" style={{ margin: "8px 2px 0" }}>
                  Anything left empty falls back on its own: brand colours to their light
                  value, neutrals to a curated dark set. A partial palette is fine.
                </p>
              )}
            </div>
            <div className="field-group">
              <div className="group-title">Dark palette</div>
              <div className="form-grid">
                <label>Primary</label>
                <Color v={b.dark?.primary_color} d="#818cf8" onChange={(v) => setDark("primary_color", v)} />
                <label>Primary text</label>
                <Color v={b.dark?.primary_text} d="#ffffff" onChange={(v) => setDark("primary_text", v)} />
                <label>Customer bubble</label>
                <Color v={b.dark?.user_bubble} d="#818cf8" onChange={(v) => setDark("user_bubble", v)} />
                <label>Customer text</label>
                <Color v={b.dark?.user_bubble_text} d="#ffffff" onChange={(v) => setDark("user_bubble_text", v)} />
                <label>Agent bubble</label>
                <Color v={b.dark?.agent_bubble} d="#2a323d" onChange={(v) => setDark("agent_bubble", v)} />
                <label>Agent text</label>
                <Color v={b.dark?.agent_bubble_text} d="#e6eaf0" onChange={(v) => setDark("agent_bubble_text", v)} />
                <label>Thread background</label>
                <Color v={b.dark?.surface} d="#1b2027" onChange={(v) => setDark("surface", v)} />
                <label>Panel</label>
                <Color v={b.dark?.panel} d="#242b34" onChange={(v) => setDark("panel", v)} />
                <label>Message box</label>
                <Color v={b.dark?.input_bg} d={b.dark?.panel || "#242b34"} onChange={(v) => setDark("input_bg", v)} />
                <label>Border</label>
                <Color v={b.dark?.border} d="#333b46" onChange={(v) => setDark("border", v)} />
                <label>Message box border</label>
                <Color v={b.dark?.input_border} d={b.dark?.border || "#333b46"} onChange={(v) => setDark("input_border", v)} />
                <label>Agent bubble border</label>
                <Color v={b.dark?.agent_bubble_border} d={b.dark?.border || "#333b46"} onChange={(v) => setDark("agent_bubble_border", v)} />
                <label>Muted text</label>
                <Color v={b.dark?.muted} d="#9aa5b4" onChange={(v) => setDark("muted", v)} />
                <label>Body text</label>
                <Color v={b.dark?.text} d="#e6eaf0" onChange={(v) => setDark("text", v)} />
              </div>
            </div>
            <GradientFields
              g={b.dark?.gradient}
              onChange={setDarkGrad}
              onStops={setDarkGradStops}
              headerText={String(b.dark?.primary_text ?? b.primary_text ?? "#ffffff")}
              title="Dark gradient"
              hint="Optional. Empty reuses the brand gradient above, like every other brand colour."
              inherit={b.gradient}
            />
            <div className="field-group">
              <div className="group-title">Type & shape</div>
              <div className="form-grid">
                <label>Font family</label>
                <input value={b.font_family ?? ""} onChange={(e) => setBrand("font_family", e.target.value)} placeholder="Poppins, system-ui, sans-serif" />
                <label>
                  Font stylesheet URL<span className="hint">loads the web font, e.g. a Google Fonts link</span>
                </label>
                <input value={b.font_url ?? ""} onChange={(e) => setBrand("font_url", e.target.value)} placeholder="https://fonts.googleapis.com/css2?family=Poppins:wght@400;600&display=swap" />
                <label>Corner radius</label>
                <input value={b.radius ?? ""} onChange={(e) => setBrand("radius", e.target.value)} placeholder="14px" />
                <label>Panel shadow</label>
                <select className="select" value={b.shadow === false ? "off" : "on"} onChange={(e) => setB((p) => ({ ...p, shadow: e.target.value === "on" }))}>
                  <option value="on">On</option>
                  <option value="off">Off</option>
                </select>
                <label>
                  Full-screen width<span className="hint">wide fits large tables</span>
                </label>
                <select className="select" value={b.fullscreen ?? "centered"} onChange={(e) => setBrand("fullscreen", e.target.value)}>
                  <option value="centered">Centered column</option>
                  <option value="wide">Wide</option>
                </select>
                <label>
                  Panel style
                  <span className="hint">framed insets the conversation inside the brand colour</span>
                </label>
                <select className="select" value={b.frame === true ? "framed" : "flat"} onChange={(e) => setB((p) => ({ ...p, frame: e.target.value === "framed" }))}>
                  <option value="flat">Flat (bordered)</option>
                  <option value="framed">Framed</option>
                </select>
                <label>
                  Frame thickness
                  {typeof b.frame_width === "number" && b.frame_width > FRAME_W_MAX ? (
                    <span className="hint warn-hint">capped at {FRAME_W_MAX}px</span>
                  ) : (
                    <span className="hint">px of brand paint, 0–{FRAME_W_MAX}</span>
                  )}
                </label>
                <span className="size-row">
                  <Px v={b.frame_width} d={2} onChange={(v) => setSize("frame_width", v)} />
                  {b.frame !== true && <span className="hint">framed panel only</span>}
                </span>
                <label>Send button</label>
                <select className="select" value={b.send_shape === "circle" ? "circle" : "rounded"} onChange={(e) => setBrand("send_shape", e.target.value)}>
                  <option value="rounded">Rounded square</option>
                  <option value="circle">Circle</option>
                </select>
                <label>Send icon</label>
                <select className="select" value={b.send_icon === "plane" ? "plane" : "arrow"} onChange={(e) => setBrand("send_icon", e.target.value)}>
                  <option value="arrow">Arrow</option>
                  <option value="plane">Paper plane</option>
                </select>
              </div>
            </div>
            <div className="field-group">
              <div className="group-title">Panel size</div>
              <div className="form-grid">
                <label>
                  Opens at<span className="hint">width × height in px</span>
                </label>
                <span className="size-row">
                  <Px v={b.width} d={400} step={10} onChange={(v) => setSize("width", v)} />
                  <span className="size-x">×</span>
                  <Px v={b.height} d={660} step={10} onChange={(v) => setSize("height", v)} />
                </span>
                <label>
                  Smallest<span className="hint">how far a customer may shrink it</span>
                </label>
                <span className="size-row">
                  <Px v={b.min_width} d={320} step={10} onChange={(v) => setSize("min_width", v)} />
                  <span className="size-x">×</span>
                  <Px v={b.min_height} d={420} step={10} onChange={(v) => setSize("min_height", v)} />
                </span>
              </div>
              <p className="hint" style={{ margin: "8px 2px 0" }}>
                Held to a usable range ({PANEL_FLOOR_W}×{PANEL_FLOOR_H} minimum), and the
                smallest size is pulled down to the opening size if you set it larger — so
                these can't make the widget unusable. The panel is also capped to the host
                page's viewport, so a large size just means "as big as it fits".
              </p>
              {sizeNote(b) && (
                <p className="hint" style={{ margin: "6px 2px 0", color: "var(--warn, #b45309)" }}>
                  {sizeNote(b)}
                </p>
              )}
            </div>
          </div>
          <WidgetPreview b={b} title={b.title || displayName || "Assistant"} welcome={welcome} />
        </div>
      )}
      </fieldset>
    </form>
  );
}

const ANGLES = [
  ["to right", "Left → right"],
  ["to left", "Right → left"],
  ["to bottom", "Top → bottom"],
  ["to top", "Bottom → top"],
  ["to bottom right", "Diagonal ↘"],
  ["to bottom left", "Diagonal ↙"],
  ["to top right", "Diagonal ↗"],
  ["to top left", "Diagonal ↖"],
] as const;

/** Gradient stop editor — three colours plus a direction. Deliberately NOT a CSS
 *  field: the server composes the gradient from validated stops so a typo can only
 *  restyle, never smuggle in a `url()`. */
function GradientFields({
  g,
  onChange,
  onStops,
  headerText,
  title,
  hint,
  inherit,
}: {
  g?: api.Gradient | null;
  onChange: (k: keyof api.Gradient, v: string) => void;
  /** Replace the whole stop list — what the drag editor emits. */
  onStops: (stops: api.GradientStop[]) => void;
  headerText: string;
  title: string;
  hint: string;
  /** What an empty field actually falls back to, shown as the placeholder. The
   *  dark gradient inherits the CONFIGURED light stops, so hardcoding brand
   *  colours here would tell the operator something untrue. */
  inherit?: api.Gradient | null;
}) {
  const css = resolveGradient(g);
  // The header's controls sit at the END of the gradient, so that stop is the one
  // that has to carry the header text. Warn rather than block — matching a brand
  // deck exactly is sometimes the right call, but it should be a decision.
  const endStop = g?.to ?? "";
  const ratio = contrast(endStop, headerText);
  const poor = ratio !== null && ratio < 3;
  return (
    <div className="field-group">
      <div className="group-title">{title}</div>
      <div className="form-grid">
        <label>Direction</label>
        <select className="select" value={g?.angle ?? inherit?.angle ?? "to right"} onChange={(e) => onChange("angle", e.target.value)}>
          {ANGLES.map(([v, lbl]) => (
            <option key={v} value={v}>
              {lbl}
            </option>
          ))}
        </select>
      </div>
      {css || stopsOf(g).length >= 2 ? (
        <GradientBar g={g} onChange={onStops} />
      ) : (
        <p className="hint" style={{ margin: "8px 2px 0" }}>
          {hint}
        </p>
      )}
      {poor && (
        <p className="hint" style={{ margin: "8px 2px 0", color: "var(--warn, #b45309)" }}>
          Header text on the end stop is {ratio!.toFixed(2)}:1 — under the 3:1 needed to
          read. The header's buttons sit at that end. Try a darker end stop, a different
          direction, or a darker Primary text.
        </p>
      )}
    </div>
  );
}

/** Mirrors the server's usable floor (theme.rs `mod panel`). Shown, not enforced —
 *  the server clamps regardless, so the field never has to fight the operator. */
const PANEL_FLOOR_W = 280;
const PANEL_FLOOR_H = 360;
/** theme.rs `panel::FRAME_W_MAX`. */
const FRAME_W_MAX = 24;

/** Tell the operator what the server is about to do to what they typed, rather than
 *  silently correcting it after save. */
function sizeNote(b: api.Branding): string | null {
  const n = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : null);
  const [w, h, mw, mh] = [n(b.width), n(b.height), n(b.min_width), n(b.min_height)];
  if ((w !== null && w < PANEL_FLOOR_W) || (h !== null && h < PANEL_FLOOR_H)) {
    return `Below the usable floor — this will open at ${PANEL_FLOOR_W}×${PANEL_FLOOR_H} instead.`;
  }
  if ((mw !== null && w !== null && mw > w) || (mh !== null && h !== null && mh > h)) {
    return "The smallest size is larger than the opening size — it will be pulled down to match.";
  }
  return null;
}

/** A px field. Deliberately carries NO `min`/`max` beyond "not negative": these live
 *  inside a <form>, so a native range violation blocks the whole save — and the one
 *  hardcoded floor this used to have was the panel HEIGHT floor, which rejected both
 *  a valid 300px width and every frame thickness. The server clamps regardless, and
 *  `sizeNote` says so before you save, which is the behaviour that was wanted. */
function Px({
  v,
  d,
  step = 1,
  onChange,
}: {
  v?: number | null;
  d: number;
  step?: number;
  onChange: (v: string) => void;
}) {
  return (
    <input
      type="number"
      className="size-input"
      value={v ?? ""}
      placeholder={String(d)}
      min={0}
      step={step}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}

function Color({ v, d, onChange }: { v?: string | null; d: string; onChange: (v: string) => void }) {
  const val = v ?? "";
  return (
    <span className="color-row">
      <input type="color" value={HEX.test(val) ? val : d} onChange={(e) => onChange(e.target.value)} />
      <input type="text" value={val} placeholder={d} onChange={(e) => onChange(e.target.value)} />
    </span>
  );
}

function WidgetPreview({ b, title, welcome }: { b: api.Branding; title: string; welcome: string }) {
  const mode = themeMode(b);
  // Which palette the preview paints. Follows the configured mode, but stays
  // flippable so a dark palette can be checked without switching the agent to it.
  const [dark, setDark] = useState(mode === "dark");
  useEffect(() => setDark(mode === "dark"), [mode]);
  const t = resolveBrand(b, dark);
  // Every brand surface paints the gradient when there is one, and the flat
  // primary when there isn't — exactly the fallback the widget CSS uses.
  const brand = t.gradient || t.primary;
  // Small round controls zoom to the gradient's mid stop, like the widget does, so
  // a white glyph doesn't land on the palest end.
  // Own paint shows in full; an inherited brand gradient is zoomed to its mid stop.
  const surface = (own: string) =>
    own
      ? { background: own, backgroundSize: "100% 100%", backgroundPosition: "center" }
      : t.gradient
        ? { background: brand, backgroundSize: "260% 100%", backgroundPosition: "center" }
        : { background: brand };
  const avatarPaint = surface(t.avatarBg);
  const sendPaint = surface(t.sendBg);
  const logo = b.logo_url ? String(b.logo_url) : "";
  return (
    <div className="preview-col">
      <div className="preview-head">
        <div className="preview-label">Live preview</div>
        {mode !== "light" && (
          <div className="preview-modes">
            {(["light", "dark"] as const).map((m) => (
              <button
                key={m}
                type="button"
                className={"btn-sm ghost" + (dark === (m === "dark") ? " on" : "")}
                onClick={() => setDark(m === "dark")}
              >
                {m === "dark" ? "Dark" : "Light"}
              </button>
            ))}
          </div>
        )}
      </div>
      <div
        className="widget-preview"
        style={{
          borderRadius: t.radius,
          fontFamily: t.font,
          boxShadow: t.shadow ? undefined : "none",
          // Three distinct treatments, matching the widget CSS exactly:
          //  framed      → brand paint fills the panel, body inset inside it
          //  gradient    → 1px gradient ring (padding-box / border-box layers)
          //  neither     → the plain neutral hairline
          ...(t.frame
            ? { background: brand, border: 0 }
            : t.gradient
              ? {
                  background: t.panel,
                  border: "1px solid transparent",
                  backgroundImage: `linear-gradient(${t.panel}, ${t.panel}), ${t.gradient}`,
                  backgroundOrigin: "border-box" as const,
                  backgroundClip: "padding-box, border-box" as const,
                }
              : { background: t.panel, border: `1px solid ${t.border}` }),
        }}
      >
        {/* Framed: transparent, so the header shows the top slice of the panel-wide
            gradient — exactly what `:root.wf-frame .wf-head` does. Painting it here
            instead completes the gradient inside the header box, so it reaches its end
            stop across the header rather than partway down the panel. */}
        <div
          className="wp-head"
          style={{ background: t.frame ? "transparent" : brand, color: t.primaryText }}
        >
          {logo && (
            <img src={logo} alt="" className={"wp-logo" + (t.logoFit === "contain" ? " wp-logo-contain" : "")} onError={(e) => (e.currentTarget.style.display = "none")} />
          )}
          <div>
            <div className="wp-title">{title}</div>
            {b.subtitle && <div className="wp-sub">{String(b.subtitle)}</div>}
          </div>
        </div>
        <div
          className="wp-body"
          style={{
            background: t.surface,
            // Framed: inset from the brand paint, with the body's own top rounding.
            ...(t.frame
              ? {
                  margin: `0 ${t.frameWidth}px 0`,
                  borderRadius: `calc(${t.radius} + 2px) calc(${t.radius} + 2px) 0 0`,
                }
              : {}),
          }}
        >
          <div className="wp-row">
            {t.avatar && (
              <span className="wp-avatar" style={{ color: t.primaryText, ...avatarPaint }}>
                {logo ? <img src={logo} alt="" className={t.logoFit === "contain" ? "wp-logo-contain" : undefined} /> : <PreviewChatIcon />}
              </span>
            )}
            <div className="wp-bubble wp-agent" style={{ background: t.agentBubble, color: t.agentText, border: `1px solid ${t.agentBorder}` }}>
              {welcome || "Hi! How can I help today?"}
            </div>
          </div>
          <div className="wp-bubble wp-user" style={{ background: t.userBubble, color: t.userText }}>
            Can you help me with my booking?
          </div>
          <div className="wp-row">
            {t.avatar && (
              <span className="wp-avatar" style={{ color: t.primaryText, ...avatarPaint }}>
                {logo ? <img src={logo} alt="" className={t.logoFit === "contain" ? "wp-logo-contain" : undefined} /> : <PreviewChatIcon />}
              </span>
            )}
            <div className="wp-bubble wp-agent" style={{ background: t.agentBubble, color: t.agentText, border: `1px solid ${t.agentBorder}` }}>
              Of course — let me pull that up for you.
            </div>
          </div>
        </div>
        <div
          className="wp-input"
          style={{
            background: t.panel,
            borderTopColor: t.border,
            color: t.muted,
            ...(t.frame
              ? {
                  margin: `0 ${t.frameWidth}px ${t.frameWidth}px`,
                  borderRadius: `0 0 max(0px, calc(${t.radius} - ${t.frameWidth}px)) max(0px, calc(${t.radius} - ${t.frameWidth}px))`,
                }
              : {}),
          }}
        >
          <span className="wp-inputbox" style={{ background: t.inputBg, borderColor: t.border }}>
            Message…
          </span>
          <span
            className="wp-send"
            style={{
              color: t.primaryText,
              borderRadius: t.sendShape === "circle" ? "50%" : "8px",
              ...sendPaint,
            }}
          >
            <PreviewSendIcon kind={t.sendIcon} />
          </span>
        </div>
      </div>
      <PreviewLauncher t={t} brand={brand} title={title} logo={logo} left={b.position === "left"} />
    </div>
  );
}

/** The launcher in its resting state, plus the pill an `expand` launcher grows to
 *  — shown side by side, since the hover state is the whole point of that mode. */
function PreviewLauncher({
  t,
  brand,
  title,
  logo,
  left,
}: {
  t: Resolved;
  brand: string;
  title: string;
  logo: string;
  left: boolean;
}) {
  const mark = logo ? (
    <img src={logo} alt="" className={"wp-launch-logo" + (t.logoFit === "contain" ? " wp-logo-contain" : "")} />
  ) : (
    <PreviewChatIcon />
  );
  const skin = { background: brand, color: t.primaryText };
  return (
    <div className="preview-launcher" style={{ justifyContent: left ? "flex-start" : "flex-end" }}>
      {t.launcher === "expand" && (
        <span className="wp-launcher wp-launcher-pill" style={{ ...skin, borderRadius: "999px" }}>
          {mark}
          <span style={{ marginLeft: 8 }}>{title}</span>
        </span>
      )}
      <span
        className={"wp-launcher " + (t.launcher === "pill" ? "wp-launcher-pill" : "wp-launcher-bubble")}
        style={{ ...skin, borderRadius: t.launcher === "pill" ? "999px" : "50%" }}
      >
        {t.launcher === "pill" ? (
          <>
            {mark}
            <span style={{ marginLeft: 8 }}>{title}</span>
          </>
        ) : (
          mark
        )}
      </span>
    </div>
  );
}

/** Kept byte-identical to App.tsx SendIcon so the preview cannot drift from it. */
const PLANE_D =
  "M21.61 12C16.65 8.75 11.07 6.11 6.11 4.71C9.05 8.12 10.14 9.83 10.45 10.92L15.41 10.92Q16.5 12 15.41 13.08L10.45 13.08C10.14 14.17 9.05 15.88 6.11 19.29C11.07 17.89 16.65 15.25 21.61 12Z";

/** The same two send glyphs the widget offers (App.tsx SendIcon). */
function PreviewSendIcon({ kind }: { kind: string }) {
  return (
    <svg viewBox="0 0 24 24" width="13" height="13" fill="none" aria-hidden="true">
      <path
        d={kind === "plane" ? PLANE_D : "M20 12L4 4l6 8-6 8 16-8Z"}
        {...(kind === "plane" ? { stroke: "currentColor", strokeWidth: 0.7, strokeLinejoin: "round" as const } : {})}
        fill="currentColor"
      />
    </svg>
  );
}

/** The same speech-bubble glyph the real widget launcher renders (App.tsx ChatIcon). */
function PreviewChatIcon() {
  return (
    <svg viewBox="0 0 24 24" width="26" height="26" fill="none" aria-hidden="true">
      <path d="M4 4h16a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H8l-4 4V5a1 1 0 0 1 1-1Z" fill="currentColor" />
    </svg>
  );
}
