import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";

import * as api from "./api";
import { Audit } from "./Audit";
import { parseRoute, routePath, type Route } from "./route";
import { TenantDetail } from "./TenantDetail";
import { Tenants } from "./Tenants";
import "./styles.css";

function SignIn() {
  const [error, setError] = useState("");
  useEffect(() => {
    // A failed OIDC callback bounces back here with its sentence.
    const params = new URLSearchParams(location.search);
    const err = params.get("login_error");
    if (err) {
      setError(err);
      history.replaceState(null, "", location.pathname);
    }
  }, []);
  return (
    <div className="login-wrap">
      <div className="login-card">
        <h1>Platform</h1>
        <p>The deployment's tenants, for its staff. Sign in with your platform account.</p>
        {error && <p className="err">{error}</p>}
        <button className="btn sso" type="button" onClick={() => (location.href = "/platform/api/login/oidc/start")}>
          Sign in with SSO
        </button>
      </div>
    </div>
  );
}

// Inline icons (no external assets — CSP-safe). currentColor so nav states apply.
const ICON = {
  tenants: "M2 13V6l6-3.5L14 6v7a1 1 0 01-1 1H3a1 1 0 01-1-1zM6 14V9h4v5",
  audit: "M3 1.5h6l4 4v9a1 1 0 01-1 1H3a1 1 0 01-1-1v-12a1 1 0 011-1zM9 1.5V5h4M5 9l2 2 4-4",
};
const Icon = ({ d }: { d: string }) => (
  <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={1.4} strokeLinecap="round" strokeLinejoin="round">
    <path d={d} />
  </svg>
);

function Shell({ me, onOut }: { me: api.Me; onOut: () => void }) {
  const [route, setRoute] = useState<Route>(() => parseRoute(location.pathname));
  const navigate = (next: Route) => {
    const path = routePath(next);
    if (location.pathname !== path) history.pushState({}, "", path);
    setRoute(next);
  };
  useEffect(() => {
    // A bare /platform becomes /platform/tenants without a history entry.
    const path = routePath(parseRoute(location.pathname));
    if (location.pathname !== path) history.replaceState({}, "", path);
    const onPop = () => setRoute(parseRoute(location.pathname));
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  const signOut = async () => {
    await api.logout();
    onOut();
  };
  return (
    <div className="shell">
      <aside className="side">
        <div className="brand">
          <span className="dot" />
          workforce-edge
          <span className="env">platform</span>
        </div>
        <div className="nav-group">
          <div className="nav-label">Operate</div>
          <nav>
            <button className={route.screen === "tenants" ? "active" : ""} onClick={() => navigate({ screen: "tenants" })}>
              <Icon d={ICON.tenants} />
              Tenants
            </button>
            <button className={route.screen === "audit" ? "active" : ""} onClick={() => navigate({ screen: "audit" })}>
              <Icon d={ICON.audit} />
              Audit log
            </button>
          </nav>
        </div>
        <div className="side-foot">
          <div className="whoami" title={me.operator}>
            <span className="avatar">{me.operator.slice(0, 1)}</span>
            <span className="who">
              {me.operator}
              <span className="role">{me.role} operator</span>
            </span>
          </div>
          <button className="logout" onClick={signOut}>
            Sign out
          </button>
        </div>
      </aside>
      <main className="main">
        <div className="main-inner">
          {route.screen === "tenants" && !route.id && <Tenants onOpen={(id) => navigate({ screen: "tenants", id })} />}
          {route.screen === "tenants" && route.id && (
            <TenantDetail id={route.id} onBack={() => navigate({ screen: "tenants" })} onDeleted={() => navigate({ screen: "tenants" })} />
          )}
          {route.screen === "audit" && <Audit onOpenTenant={(id) => navigate({ screen: "tenants", id })} />}
        </div>
      </main>
    </div>
  );
}

function App() {
  const [me, setMe] = useState<api.Me | null>(null);
  const [loading, setLoading] = useState(true);
  const [probeError, setProbeError] = useState("");
  useEffect(() => {
    api.setUnauthorizedHandler(() => setMe(null));
    api
      .me()
      .then(setMe)
      .catch((e) => setProbeError(e instanceof Error ? e.message : String(e)))
      .finally(() => setLoading(false));
    return () => api.setUnauthorizedHandler(null);
  }, []);
  if (loading) return null;
  if (probeError) {
    return (
      <div className="login-wrap">
        <div className="login-card">
          <h1>Platform</h1>
          <p>The platform API did not answer.</p>
          <p className="err">{probeError}</p>
          <button className="btn ghost" type="button" onClick={() => location.reload()}>
            Try again
          </button>
        </div>
      </div>
    );
  }
  if (!me) return <SignIn />;
  return <Shell me={me} onOut={() => setMe(null)} />;
}

createRoot(document.getElementById("root")!).render(<App />);
