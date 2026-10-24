import { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";

import * as api from "./api";
import { Agents } from "./Agents";
import { Playbooks } from "./Playbooks";
import { Conversations } from "./Conversations";
import { Analytics } from "./Analytics";
import { Feedback } from "./Feedback";
import { Gaps } from "./Gaps";
import { Lessons } from "./Lessons";
import { Operators } from "./Operators";
import { Audit } from "./Audit";
import { Settings } from "./Settings";
import { Setup } from "./Setup";
import { Import } from "./Import";
import { planBanner } from "./usage";
import "./styles.css";

// Minimal ambient typing for the Google Identity Services client.
declare global {
  interface Window {
    google?: {
      accounts: {
        id: {
          initialize: (o: {
            client_id: string;
            callback: (r: { credential: string }) => void;
          }) => void;
          renderButton: (el: HTMLElement, o: Record<string, unknown>) => void;
        };
      };
    };
  }
}

function GoogleButton({
  clientId,
  onToken,
}: {
  clientId: string;
  onToken: (token: string) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const cb = useRef(onToken);
  cb.current = onToken;

  useEffect(() => {
    const render = () => {
      const g = window.google;
      if (!g || !ref.current) return false;
      g.accounts.id.initialize({
        client_id: clientId,
        callback: (r) => cb.current(r.credential),
      });
      g.accounts.id.renderButton(ref.current, {
        theme: "outline",
        size: "large",
        width: 296,
      });
      return true;
    };
    if (render()) return;
    const s = document.createElement("script");
    s.src = "https://accounts.google.com/gsi/client";
    s.async = true;
    s.onload = render;
    document.head.appendChild(s);
  }, [clientId]);

  return <div ref={ref} />;
}

function Login({ onIn }: { onIn: () => void }) {
  const [cfg, setCfg] = useState<api.LoginConfig | null>(null);
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    // Surface an error bounced back from a failed OIDC redirect callback.
    const params = new URLSearchParams(location.search);
    const err = params.get("login_error");
    if (err) {
      setError(err);
      history.replaceState(null, "", location.pathname);
    }
    api
      .loginConfig()
      .then(setCfg)
      .catch(() =>
        setCfg({ password: true, oidc_mode: null, google_client_id: null }),
      );
  }, []);

  const submitPassword = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError("");
    const ok = await api.login(password);
    setBusy(false);
    if (ok) onIn();
    else setError("Incorrect password.");
  };

  const onGoogle = async (token: string) => {
    setError("");
    const r = await api.loginOidc(token);
    if (r.ok) onIn();
    else setError(r.error ?? "Sign-in failed.");
  };

  return (
    <div className="login-wrap">
      <div className="login-card">
        <h1>Edge Admin</h1>
        <p>Sign in to manage this install.</p>
        {error && <p className="err">{error}</p>}
        {cfg?.oidc_mode === "redirect" && (
          <button
            className="btn sso"
            type="button"
            onClick={() => (location.href = "/admin/api/login/oidc/start")}
          >
            Sign in with SSO
          </button>
        )}
        {cfg?.oidc_mode === "google" && cfg.google_client_id && (
          <div className="google-row">
            <GoogleButton clientId={cfg.google_client_id} onToken={onGoogle} />
          </div>
        )}
        {cfg?.oidc_mode && cfg.password && (
          <div className="or">or break-glass password</div>
        )}
        {cfg?.password && (
          <form onSubmit={submitPassword}>
            <div className="field">
              <input
                type="password"
                placeholder="Password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </div>
            <button className="btn" type="submit" disabled={!password || busy}>
              {busy ? "Signing in…" : "Sign in"}
            </button>
          </form>
        )}
      </div>
    </div>
  );
}

type Screen =
  | "setup"
  | "agents"
  | "playbooks"
  | "knowledge"
  | "conversations"
  | "analytics"
  | "feedback"
  | "gaps"
  | "lessons"
  | "operators"
  | "audit"
  | "settings"
  | "import";

// Inline icons (no external assets — CSP-safe). currentColor so nav states apply.
const I = {
  setup: "M2.5 4.5l1.5 1.5 3-3M2.5 9.5l1.5 1.5 3-3M9 4.5h5M9 9.5h5",
  agents: "M8 8a3 3 0 100-6 3 3 0 000 6zM2 14a6 6 0 0112 0",
  playbooks:
    "M3 2h7l3 3v9a1 1 0 01-1 1H3a1 1 0 01-1-1V3a1 1 0 011-1zM5 7h6M5 10h6",
  knowledge:
    "M2 3.5A1.5 1.5 0 013.5 2H8v12H3.5A1.5 1.5 0 012 12.5v-9zM8 2h4.5A1.5 1.5 0 0114 3.5v9a1.5 1.5 0 01-1.5 1.5H8",
  conversations: "M2 4a2 2 0 012-2h8a2 2 0 012 2v5a2 2 0 01-2 2H6l-3 3V4z",
  analytics: "M2 2v11a1 1 0 001 1h11M5 11V8M8.5 11V5.5M12 11V9",
  feedback:
    "M5 13V6.5l3.2-4a.9.9 0 011.6.7L9.3 6H13a1.1 1.1 0 011.1 1.3l-.9 4.6A1.4 1.4 0 0111.8 13H5zm0 0H2.6a.6.6 0 01-.6-.6V7a.6.6 0 01.6-.6H5",
  gaps: "M8 14A6 6 0 108 2a6 6 0 000 12zM8 10.5A2.5 2.5 0 108 5.5a2.5 2.5 0 000 5zM8 8h.01",
  lessons:
    "M8 1.5l1.9 3.9 4.3.6-3.1 3 .7 4.3L8 11.3 4.2 13.3l.7-4.3-3.1-3 4.3-.6z",
  operators:
    "M6 8a2.5 2.5 0 100-5 2.5 2.5 0 000 5zM1.5 14a4.5 4.5 0 019 0M11 6.5a2 2 0 100-4M14.5 13.5a3.5 3.5 0 00-3-3.4",
  audit:
    "M3 1.5h6l4 4v9a1 1 0 01-1 1H3a1 1 0 01-1-1v-12a1 1 0 011-1zM9 1.5V5h4M5 9l2 2 4-4",
  settings:
    "M8 10.5a2.5 2.5 0 100-5 2.5 2.5 0 000 5zM6.7 1.8h2.6l.4 1.7 1.5.9 1.7-.5 1.3 2.2-1.3 1.2v1.7l1.3 1.2-1.3 2.2-1.7-.5-1.5.9-.4 1.7H6.7l-.4-1.7-1.5-.9-1.7.5-1.3-2.2 1.3-1.2V7.3L1.8 6.1l1.3-2.2 1.7.5 1.5-.9z",
  import: "M8 2v8M4.8 6.8L8 10l3.2-3.2M2.5 13.5h11",
};
const Icon = ({ d }: { d: string }) => (
  <svg
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth={1.4}
    strokeLinecap="round"
    strokeLinejoin="round"
  >
    <path d={d} />
  </svg>
);

const NAV: { group: string; items: [Screen, string][] }[] = [
  {
    group: "Configure",
    items: [
      ["setup", "Setup"],
      ["agents", "Agents"],
      ["playbooks", "Playbooks"],
    ],
  },
  {
    group: "Monitor",
    items: [
      ["conversations", "Conversations"],
      ["analytics", "Analytics"],
      ["feedback", "Feedback"],
      ["gaps", "Gaps"],
      ["lessons", "Lessons"],
      ["audit", "Audit log"],
    ],
  },
  { group: "Access", items: [["operators", "Operators"]] },
  {
    group: "Install",
    items: [
      ["settings", "Settings"],
      ["import", "Import from git"],
    ],
  },
];

const SCREENS = NAV.flatMap((g) => g.items.map(([id]) => id));
// The full location lives in the URL (/admin/<screen>/<item…>) so a refresh — or
// a shared/bookmarked link — lands on exactly the same place, item and all.
interface Route {
  screen: Screen;
  params: string[];
}
function currentRoute(): Route {
  const segs = window.location.pathname
    .replace(/^\/admin\/?/, "")
    .split("/")
    .filter(Boolean)
    .map(decodeURIComponent);
  const screen = (SCREENS as string[]).includes(segs[0])
    ? (segs[0] as Screen)
    : "agents";
  return { screen, params: segs.slice(1) };
}

/** Deep-link handle a screen uses to read its item params and navigate within itself. */
export interface Nav {
  params: string[];
  go: (...params: string[]) => void;
}

/** Whether the page was opened at a bare `/admin` — no screen chosen — which
 *  is where a first run lands on the wizard instead of the agent list. */
function openedAtBareAdmin(): boolean {
  return window.location.pathname.replace(/\/$/, "") === "/admin";
}

function Shell({ operator, onOut }: { operator: string; onOut: () => void }) {
  const [route, setRoute] = useState<Route>(currentRoute);
  // Decided before the mount effect normalises the URL.
  const landedBare = useRef(openedAtBareAdmin());
  // The key banner: seeded by `me()`, moved by the settings page as the key
  // is set or removed, so it never lags a change made on this screen.
  const [llmStatus, setLlmStatus] = useState<api.LlmStatus | null>(api.llmStatus);
  const banner = api.llmBanner(llmStatus);
  // The plan banner: which caps the workspace has reached, as `me()` found
  // them when this session opened.
  const limits = planBanner(api.planUsage()?.over ?? []);
  const screen = route.screen;
  const navigate = (s: Screen, params: string[] = []) => {
    const path =
      "/admin/" + [s as string, ...params].map(encodeURIComponent).join("/");
    if (window.location.pathname !== path)
      window.history.pushState({}, "", path);
    setRoute({ screen: s, params });
  };
  const navFor = (s: Screen): Nav => ({
    params: route.screen === s ? route.params : [],
    go: (...params: string[]) => navigate(s, params),
  });
  useEffect(() => {
    // Normalise a bare /admin → /admin/<screen> without adding history.
    if (window.location.pathname.replace(/\/$/, "") === "/admin") {
      window.history.replaceState({}, "", `/admin/${route.screen}`);
    }
    const onPop = () => setRoute(currentRoute());
    window.addEventListener("popstate", onPop);
    // First run: a workspace with setup still to do opens on the wizard
    // when no screen was asked for. A deep link is left alone.
    if (landedBare.current) {
      api
        .setup()
        .then((r) => {
          if (!r.complete) navigate("setup");
        })
        .catch(() => {
          /* the report is advisory; the agent list is a fine landing */
        });
    }
    return () => window.removeEventListener("popstate", onPop);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const logout = async () => {
    await api.logout();
    onOut();
  };
  return (
    <div className="shell">
      <aside className="side">
        <div className="brand">
          <span className="dot" />
          workforce-edge
          <span className="env">admin</span>
        </div>
        {NAV.map((g) => (
          <div className="nav-group" key={g.group}>
            <div className="nav-label">{g.group}</div>
            <nav>
              {g.items.map(([id, label]) => (
                <button
                  key={id}
                  className={screen === id ? "active" : ""}
                  onClick={() => navigate(id)}
                >
                  <Icon d={I[id]} />
                  {label}
                </button>
              ))}
            </nav>
          </div>
        ))}
        <div className="side-foot">
          <div className="whoami" title={operator}>
            <span className="avatar">{operator.slice(0, 1)}</span>
            <span className="who">
              {operator}
              {api.tenant() && (
                <span className="tenant-name">{api.tenant()!.name}</span>
              )}
            </span>
          </div>
          <button className="logout" onClick={logout}>
            Sign out
          </button>
        </div>
      </aside>
      <main className="main">
        <div className="main-inner">
          {banner && (
            <p className="llm-banner" role="alert">
              {banner.text}{" "}
              <a
                href="/admin/settings"
                onClick={(e) => {
                  e.preventDefault();
                  navigate("settings");
                }}
              >
                {banner.link}
              </a>
            </p>
          )}
          {limits && (
            <p className="llm-banner" role="alert">
              {limits}{" "}
              <a
                href="/admin/settings"
                onClick={(e) => {
                  e.preventDefault();
                  navigate("settings");
                }}
              >
                See usage in Settings.
              </a>
            </p>
          )}
          {screen === "setup" && (
            <Setup onLlmStatus={setLlmStatus} onDone={() => navigate("agents")} />
          )}
          {screen === "agents" && <Agents nav={navFor("agents")} />}
          {screen === "playbooks" && <Playbooks nav={navFor("playbooks")} />}
          {screen === "conversations" && (
            <Conversations nav={navFor("conversations")} />
          )}
          {screen === "analytics" && (
            <Analytics
              onOpen={(a, owner, conv) =>
                navigate("conversations", [a, owner, conv])
              }
            />
          )}
          {screen === "feedback" && (
            <Feedback
              onOpen={(a, owner, conv) =>
                navigate("conversations", [a, owner, conv])
              }
            />
          )}
          {screen === "gaps" && (
            <Gaps
              onOpen={(a, owner, conv) =>
                navigate("conversations", [a, owner, conv])
              }
            />
          )}
          {screen === "lessons" && <Lessons />}
          {screen === "operators" && <Operators />}
          {screen === "audit" && <Audit />}
          {screen === "settings" && <Settings onLlmStatus={setLlmStatus} />}
          {screen === "import" && <Import />}
        </div>
      </main>
    </div>
  );
}

function App() {
  const [operator, setOperator] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const refresh = () => api.me().then(setOperator);
  useEffect(() => {
    refresh().finally(() => setLoading(false));
  }, []);
  if (loading) return null;
  if (!operator) return <Login onIn={() => void refresh()} />;
  return <Shell operator={operator} onOut={() => setOperator(null)} />;
}

createRoot(document.getElementById("root")!).render(<App />);
