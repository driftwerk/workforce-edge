import { useEffect, useState } from "react";

import * as api from "./api";
import { describeKeyStatus, providerChoices, toForm, toUpdate } from "./settingsForm";
import {
  STEPS,
  agentCreateBody,
  agentEditBody,
  discoverBody,
  embedSnippet,
  nextStep,
  normaliseOrigin,
  oidcCheckBody,
  prefill,
  previousStep,
  resumeStep,
  slug,
  stepDone,
  toggleTool,
  validateStep,
  type SetupState,
  type Step,
} from "./setupForm";

/** The first-run wizard: from an empty workspace to a working embedded
 *  widget, each step verified before the next. Resumable — every flag comes
 *  from the setup report, and the agent it resumes on pre-fills the form.
 *  It writes only through the settings and agents routes; the two probes
 *  (MCP discovery, the issuer check) store nothing. */
export function Setup({
  onLlmStatus,
  onDone,
}: {
  onLlmStatus: (s: api.LlmStatus) => void;
  onDone: () => void;
}) {
  const [report, setReport] = useState<api.SetupReport | null>(null);
  const [view, setView] = useState<api.SettingsView | null>(null);
  /** The agent the wizard creates or, resuming, edits. */
  const [agentId, setAgentId] = useState<string | null>(null);
  const [form, setForm] = useState<SetupState | null>(null);
  const [step, setStep] = useState<Step>("llm");
  const [loadError, setLoadError] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  // Probe results, kept across steps so going back does not lose them.
  const [keyStatus, setKeyStatus] = useState<api.LlmStatus | null>(null);
  const [survey, setSurvey] = useState<api.McpSurvey | null>(null);
  const [issuerCheck, setIssuerCheck] = useState<api.OidcCheck | null>(null);
  const [verdict, setVerdict] = useState<api.TokenVerdict | null>(null);

  useEffect(() => {
    (async () => {
      const [r, v] = await Promise.all([api.setup(), api.getSettings()]);
      const agent = r.agent ? ((await api.listAgents()).find((a) => a.id === r.agent) ?? null) : null;
      setReport(r);
      setView(v);
      setAgentId(agent?.id ?? null);
      setForm(prefill(v.llm.provider, agent));
      setKeyStatus(r.llm_status);
      setStep(resumeStep(r));
      onLlmStatus(r.llm_status);
    })().catch((e) => setLoadError(e instanceof Error ? e.message : String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (loadError) return <p className="err">{loadError}</p>;
  if (!report || !view || !form) return <p className="spinner">Loading…</p>;

  const set = <K extends keyof SetupState>(k: K, v: SetupState[K]) =>
    setForm((f) => (f ? { ...f, [k]: v } : f));
  const keyStored = report.llm_key;

  /** Whether every step before `target` is done or passes, so a later step
   *  is reachable only through the ones before it. */
  const canReach = (target: Step) => {
    for (const s of STEPS) {
      if (s.id === target) return true;
      if (!stepDone(s.id, report) && validateStep(s.id, form, keyStored)) return false;
    }
    return true;
  };
  const go = (target: Step) => {
    setError("");
    setStep(target);
  };
  const advance = () => {
    const problem = validateStep(step, form, keyStored);
    if (problem) {
      setError(problem);
      return;
    }
    if (step === "origin") set("origin", normaliseOrigin(form.origin) ?? form.origin);
    const next = nextStep(step);
    if (next) go(next);
  };
  const refreshReport = async () => {
    const r = await api.setup();
    setReport(r);
    return r;
  };
  /** Run a step's write or probe, surfacing its error under the form. */
  const run = async (op: () => Promise<void>) => {
    setBusy(true);
    setError("");
    try {
      await op();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const submitKey = () =>
    run(async () => {
      if (!view.editable) {
        throw new Error("This install has no master key, so its key is read from edge.toml and cannot be set here.");
      }
      const provider = form.provider.trim();
      if (provider !== view.llm.provider) {
        const update = toUpdate({ ...toForm(view), provider }, view);
        if (!update.ok) throw new Error(update.error);
        await api.saveSettings(update.body);
        setView(await api.getSettings());
      }
      const outcome = await api.setLlmKey(form.key);
      if (outcome.llm_status) {
        setKeyStatus(outcome.llm_status);
        onLlmStatus(outcome.llm_status);
      }
      if (!outcome.ok) throw new Error(outcome.error);
      set("key", "");
      await refreshReport();
      go("mcp");
    });

  const discover = () =>
    run(async () => {
      if (validateStep("mcp", { ...form, discovered: ["probe"], tools: ["probe"] })) {
        throw new Error("The MCP server URL must be an http(s) URL.");
      }
      const found = await api.discoverMcp(discoverBody(form));
      setSurvey(found);
      const names = found.tools.map((t) => t.name);
      // Keep an earlier choice where it still exists; a first survey offers
      // everything, and the operator unticks what the agent must not call.
      const kept = form.tools.filter((t) => names.includes(t));
      setForm((f) =>
        f ? { ...f, discovered: names, tools: kept.length ? kept : names, skip_mcp: false } : f,
      );
    });

  const checkIssuer = (token = "") =>
    run(async () => {
      const issuer = form.issuer.trim();
      if (validateStep("oidc", { ...form, issuer_checked: issuer })) {
        throw new Error("The issuer must be an http(s) URL.");
      }
      const check = await api.checkOidc(oidcCheckBody(form, token));
      setIssuerCheck(check);
      if (token) setVerdict(check.token);
      const claimed = check.report.issuer;
      if (claimed && !check.issuer_matches) {
        throw new Error(
          `The discovery document says its issuer is ${claimed}, not ${issuer}. Tokens carry the document's value, so enter that.`,
        );
      }
      // Bind the document's spelling: the socket pins `iss` to the string
      // as configured, and a trailing slash would fail every token.
      const verified = claimed ?? issuer;
      setForm((f) => (f ? { ...f, issuer: verified, issuer_checked: verified } : f));
    });

  const saveAgent = () =>
    run(async () => {
      const problem = validateStep("agent", form);
      if (problem) throw new Error(problem);
      let id = agentId;
      if (id) await api.updateAgent(id, agentEditBody(form));
      else {
        const body = agentCreateBody(form);
        await api.createAgent(body);
        id = body.id;
        setAgentId(id);
      }
      set("credential", "");
      await refreshReport();
      go("snippet");
    });

  const current = STEPS.find((s) => s.id === step)!;
  const back = previousStep(step);

  return (
    <>
      <h2>Setup</h2>
      <p className="sub">
        From an empty workspace to a widget on your site, one verified step at a time. Come back any time: finished
        steps are ticked, and the agent you created is picked up where you left it.
      </p>
      <div className="wizard">
        <ol className="wizard-steps">
          {STEPS.map((s, i) => {
            const done = stepDone(s.id, report);
            return (
              <li key={s.id} className={[s.id === step ? "active" : "", done ? "done" : ""].join(" ").trim()}>
                <button type="button" onClick={() => go(s.id)} disabled={!canReach(s.id)}>
                  <span className="wstep-num" aria-hidden="true">
                    {done ? "✓" : i + 1}
                  </span>
                  <span className="wstep-text">
                    {s.title}
                    <small>{s.blurb}</small>
                  </span>
                </button>
              </li>
            );
          })}
        </ol>

        <div className="card wizard-pane">
          <h3>{current.title}</h3>
          <p className="muted settings-blurb">{current.blurb}</p>

          {step === "llm" && (
            <div className="form-grid">
              <label>Provider</label>
              <ProviderField view={view} value={form.provider} onChange={(v) => set("provider", v)} />
              <label>
                API key<span className="hint">validated with one probe before it is stored</span>
              </label>
              <div>
                <input
                  type="password"
                  autoComplete="off"
                  value={form.key}
                  placeholder={keyStored ? "Replace the stored key" : "Paste the provider key"}
                  onChange={(e) => set("key", e.target.value)}
                  disabled={!view.editable}
                />
                {keyStatus && (
                  <p className={keyStatus.state === "failed" ? "err" : "muted"} style={{ marginTop: 8 }}>
                    {describeKeyStatus(keyStatus)}
                  </p>
                )}
                {!view.editable && (
                  <p className="banner-info" style={{ marginTop: 8 }}>
                    This install has no master key: the key is read from <code>edge.toml</code>
                    {keyStored ? " and is set." : " and cannot be set here."}
                  </p>
                )}
              </div>
            </div>
          )}

          {step === "mcp" && (
            <>
              <div className="form-grid">
                <label>Server URL</label>
                <input
                  value={form.mcp_url}
                  placeholder="https://mcp.your-company.example/mcp"
                  onChange={(e) => set("mcp_url", e.target.value)}
                />
                <label>
                  Discovery credential<span className="hint">optional bearer; customers' own tokens are forwarded per call</span>
                </label>
                <input
                  type="password"
                  autoComplete="off"
                  value={form.credential}
                  placeholder={agentId ? "Leave blank to keep the stored one" : "A service token, if discovery needs one"}
                  onChange={(e) => set("credential", e.target.value)}
                />
              </div>
              <div className="actions">
                <button className="btn-sm" type="button" disabled={busy} onClick={discover}>
                  {busy ? "Connecting…" : "Discover tools"}
                </button>
                <button
                  className="btn-sm ghost"
                  type="button"
                  disabled={busy}
                  onClick={() => {
                    set("skip_mcp", true);
                    go("origin");
                  }}
                >
                  Continue without an MCP server
                </button>
              </div>
              {survey && (
                <p className="muted" style={{ marginTop: 12 }}>
                  Connected to <strong>{survey.server ?? "the server"}</strong>
                  {survey.version ? ` ${survey.version}` : ""} (protocol {survey.protocol_version}).
                  {survey.truncated && " The list stopped at the cap, so it is a floor, not the whole surface."}
                </p>
              )}
              {form.discovered.length > 0 && (
                <ToolPicker
                  form={form}
                  tools={survey?.tools ?? form.discovered.map((name) => ({ name, description: "" }))}
                  onChange={(tools) => set("tools", tools)}
                />
              )}
            </>
          )}

          {step === "origin" && (
            <div className="form-grid">
              <label>Origin</label>
              <div>
                <input
                  value={form.origin}
                  placeholder="https://app.your-company.example"
                  onChange={(e) => set("origin", e.target.value)}
                />
                <p className="muted" style={{ marginTop: 8 }}>
                  Scheme, host and port of the page that will include the snippet — matched exactly, no wildcards.
                  More origins can be added later in the agent's editor.
                </p>
              </div>
            </div>
          )}

          {step === "oidc" && (
            <>
              <div className="form-grid">
                <label>Issuer</label>
                <input
                  value={form.issuer}
                  placeholder="https://auth.your-company.example/realms/main"
                  onChange={(e) => set("issuer", e.target.value)}
                />
                <label>
                  Audience<span className="hint">optional; your web app's client id</span>
                </label>
                <input value={form.audience} onChange={(e) => set("audience", e.target.value)} />
              </div>
              <div className="actions">
                <button className="btn-sm" type="button" disabled={busy} onClick={() => checkIssuer()}>
                  {busy ? "Checking…" : "Check issuer"}
                </button>
              </div>
              {issuerCheck && <IssuerVerdict check={issuerCheck} />}
              {form.issuer_checked && (
                <TokenTester busy={busy} verdict={verdict} onTest={(token) => checkIssuer(token)} />
              )}
            </>
          )}

          {step === "agent" && (
            <div className="form-grid">
              <label>Name</label>
              <div>
                <input value={form.name} placeholder="Acme Assistant" onChange={(e) => set("name", e.target.value)} />
                <p className="id-preview" style={{ marginTop: 6 }}>
                  id: {agentId ?? (slug(form.name) || "—")}
                </p>
              </div>
              <label>
                Persona<span className="hint">who the agent is and how it should help</span>
              </label>
              <textarea
                className="mono-area"
                rows={7}
                value={form.persona}
                placeholder="You are the Acme staff assistant. Answer from the tools; never invent a figure."
                onChange={(e) => set("persona", e.target.value)}
              />
              <label>
                Welcome<span className="hint">the first message a customer sees</span>
              </label>
              <textarea rows={2} value={form.welcome} placeholder="Hi! How can I help today?" onChange={(e) => set("welcome", e.target.value)} />
            </div>
          )}

          {step === "snippet" && <SnippetStep agentId={agentId} report={report} />}

          {error && (
            <p className="err" style={{ marginTop: 14 }}>
              {error}
            </p>
          )}

          <div className="wizard-foot">
            {back ? (
              <button className="btn-sm ghost" type="button" disabled={busy} onClick={() => go(back)}>
                Back
              </button>
            ) : (
              <span />
            )}
            <span className="spacer" />
            {step === "llm" && (
              <>
                {keyStored && !form.key.trim() && (
                  <button className="btn-sm ghost" type="button" disabled={busy} onClick={advance}>
                    Keep the stored key
                  </button>
                )}
                <button className="btn-sm" type="button" disabled={busy || !form.key.trim() || !view.editable} onClick={submitKey}>
                  {busy ? "Checking…" : "Validate and save"}
                </button>
              </>
            )}
            {(step === "mcp" || step === "origin" || step === "oidc") && (
              <button className="btn-sm" type="button" disabled={busy} onClick={advance}>
                Continue
              </button>
            )}
            {step === "agent" && (
              <button className="btn-sm" type="button" disabled={busy} onClick={saveAgent}>
                {busy ? "Building…" : agentId ? "Save agent" : "Create agent"}
              </button>
            )}
            {step === "snippet" && (
              <button className="btn-sm" type="button" onClick={onDone}>
                Finish
              </button>
            )}
          </div>
        </div>
      </div>
    </>
  );
}

/** The provider: the install's allowlist as a select, or free text when
 *  the install leaves it unrestricted — as the settings page offers it. */
function ProviderField({
  view,
  value,
  onChange,
}: {
  view: api.SettingsView;
  value: string;
  onChange: (v: string) => void;
}) {
  const choices = providerChoices({ llm: { ...view.llm, provider: value || view.llm.provider } });
  if (choices.length === 0) {
    return <input value={value} placeholder="anthropic" onChange={(e) => onChange(e.target.value)} disabled={!view.editable} />;
  }
  return (
    <select className="select" value={value} onChange={(e) => onChange(e.target.value)} disabled={!view.editable}>
      {choices.map((p) => (
        <option key={p} value={p}>
          {p}
        </option>
      ))}
    </select>
  );
}

/** The discovered tools with a tick per tool the agent may use. */
function ToolPicker({
  form,
  tools,
  onChange,
}: {
  form: SetupState;
  tools: api.McpToolInfo[];
  onChange: (tools: string[]) => void;
}) {
  return (
    <>
      <div className="tool-head">
        <span>
          {form.tools.length} of {form.discovered.length} tools allowed
        </span>
        <div className="actions">
          <button className="btn-sm ghost" type="button" onClick={() => onChange([...form.discovered])}>
            All
          </button>
          <button className="btn-sm ghost" type="button" onClick={() => onChange([])}>
            None
          </button>
        </div>
      </div>
      <div className="tool-list">
        {tools.map((t) => (
          <label key={t.name} className="tool-row">
            <input
              type="checkbox"
              checked={form.tools.includes(t.name)}
              onChange={(e) => onChange(toggleTool(form, t.name, e.target.checked))}
            />
            <span>
              <span className="tool-name">{t.name}</span>
              {t.description && <div className="tool-desc">{t.description}</div>}
            </span>
          </label>
        ))}
      </div>
    </>
  );
}

/** What the issuer's discovery document and JWKS said. */
function IssuerVerdict({ check }: { check: api.OidcCheck }) {
  const ok = check.issuer_matches && check.report.keys > 0;
  return (
    <div className={`verdict ${ok ? "ok" : "bad"}`}>
      <strong>{ok ? "Issuer reachable." : "Issuer reachable, but check the details."}</strong>
      <dl className="kv">
        <dt>Issuer</dt>
        <dd>{check.report.issuer ?? "(not stated)"}</dd>
        <dt>JWKS</dt>
        <dd>{check.report.jwks_uri}</dd>
        <dt>Algorithms</dt>
        <dd>{check.report.algs.length ? check.report.algs.join(", ") : "(not stated)"}</dd>
        <dt>Signing keys</dt>
        <dd>{check.report.keys}</dd>
      </dl>
      {check.report.keys === 0 && <p className="err" style={{ marginTop: 8 }}>The JWKS has no key this edge can verify with.</p>}
    </div>
  );
}

/** Paste a token: verified through the same validator the widget's socket
 *  uses for this issuer and audience. */
function TokenTester({
  busy,
  verdict,
  onTest,
}: {
  busy: boolean;
  verdict: api.TokenVerdict | null;
  onTest: (token: string) => void;
}) {
  const [token, setToken] = useState("");
  return (
    <div style={{ marginTop: 16 }}>
      <label>
        Test with a token<span className="hint">an id_token your app would hand the widget; verified, never stored</span>
      </label>
      <textarea
        className="mono-area"
        rows={3}
        value={token}
        placeholder="eyJhbGciOi…"
        onChange={(e) => setToken(e.target.value)}
        style={{ marginTop: 6 }}
      />
      <div className="actions" style={{ marginTop: 8 }}>
        <button className="btn-sm ghost" type="button" disabled={busy || !token.trim()} onClick={() => onTest(token)}>
          Test token
        </button>
      </div>
      {verdict && (
        <div className={`verdict ${verdict.ok ? "ok" : "bad"}`}>
          {verdict.ok ? (
            <>
              <strong>Token accepted.</strong>
              <dl className="kv">
                <dt>Subject</dt>
                <dd>{verdict.sub}</dd>
                <dt>Email</dt>
                <dd>
                  {verdict.email ?? "(none)"}
                  {verdict.email_verified === false ? " — unverified" : ""}
                </dd>
                {verdict.name && (
                  <>
                    <dt>Name</dt>
                    <dd>{verdict.name}</dd>
                  </>
                )}
              </dl>
            </>
          ) : (
            <>
              <strong>Token refused.</strong> {verdict.error}
            </>
          )}
        </div>
      )}
    </div>
  );
}

/** The embed snippet with a copy button, and the demo page as a live
 *  preview where the install serves one. */
function SnippetStep({ agentId, report }: { agentId: string | null; report: api.SetupReport }) {
  const [copied, setCopied] = useState(false);
  if (!agentId) {
    return <p className="muted">Create the agent first; its snippet appears here.</p>;
  }
  const snippet = embedSnippet(window.location.origin, agentId);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(snippet);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    } catch {
      setCopied(false);
    }
  };
  return (
    <>
      <p className="muted">
        Paste this before <code>&lt;/body&gt;</code> on <strong>{report.origins ? "the origin you allowed" : "your page"}</strong>.
        Your page answers the widget's token request with the signed-in user's id_token; the widget then talks to the
        agent as that user.
      </p>
      <textarea className="snippet" readOnly value={snippet} rows={9} onFocus={(e) => e.currentTarget.select()} />
      <div className="actions">
        <button className="btn-sm" type="button" onClick={copy}>
          {copied ? "Copied" : "Copy snippet"}
        </button>
      </div>
      {report.demo ? (
        <>
          <p className="muted" style={{ marginTop: 16 }}>
            Live preview — the install's test page, signing in with Google and handing the widget its token exactly as
            your page will.
          </p>
          <iframe className="wizard-preview" src={`/demo/${encodeURIComponent(agentId)}`} title="Widget preview" />
        </>
      ) : (
        <p className="muted" style={{ marginTop: 16 }}>
          No live preview on this install (the <code>/demo</code> page is off). Paste the snippet into your page to see
          the widget.
        </p>
      )}
    </>
  );
}
