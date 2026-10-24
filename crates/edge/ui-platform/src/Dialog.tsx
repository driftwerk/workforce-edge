import { useEffect, type ReactNode } from "react";

/** A modal over the page: a title, a blurb, a body, and the caller's action
 *  row. Escape and a click on the backdrop close it. */
export function Dialog({
  title,
  blurb,
  onClose,
  children,
  actions,
}: {
  title: string;
  blurb?: ReactNode;
  onClose: () => void;
  children: ReactNode;
  actions: ReactNode;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal" role="dialog" aria-modal="true" aria-label={title}>
        <h3>{title}</h3>
        {blurb && <p className="muted blurb">{blurb}</p>}
        {children}
        <div className="actions">{actions}</div>
      </div>
    </div>
  );
}

/** One KPI tile: a small label over a large value, with a loading placeholder. */
export function StatCard({
  label,
  value,
  hint,
  isLoading,
  over,
}: {
  label: string;
  value: ReactNode;
  hint?: string;
  isLoading?: boolean;
  over?: boolean;
}) {
  return (
    <div className={over ? "stat-card over" : "stat-card"}>
      <div className="stat-label">{label}</div>
      <div className="stat-value">{isLoading ? <span className="stat-skel" /> : value}</div>
      {hint && <div className="stat-hint">{hint}</div>}
    </div>
  );
}

/** A status pill in one of the console's tones. */
export function Pill({ label, tone, title }: { label: string; tone: "ok" | "bad" | "warn" | "neutral" | "accent"; title?: string }) {
  const cls = tone === "accent" ? "pill" : `pill pill-${tone}`;
  return (
    <span className={cls} title={title}>
      {tone !== "accent" && tone !== "neutral" && <span className="dot" />}
      {label}
    </span>
  );
}

/** The message a failed call is shown with. */
export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
