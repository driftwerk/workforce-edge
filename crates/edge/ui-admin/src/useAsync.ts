import { useEffect, useState } from "react";

interface AsyncState<T> {
  data: T | null;
  loading: boolean;
  error: string | null;
}

/**
 * A tiny useQuery-style hook over any promise-returning `fn`: re-runs whenever a
 * value in `deps` changes (pass the serialized filter as the dep), and drops a
 * resolved result if a newer run has started. No react-query — consistent with
 * this SPA's minimal stack.
 */
export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]): AsyncState<T> {
  const [state, setState] = useState<AsyncState<T>>({ data: null, loading: true, error: null });
  useEffect(() => {
    let alive = true;
    setState((s) => ({ data: s.data, loading: true, error: null }));
    fn()
      .then((data) => {
        if (alive) setState({ data, loading: false, error: null });
      })
      .catch((e) => {
        if (alive) setState({ data: null, loading: false, error: e instanceof Error ? e.message : String(e) });
      });
    return () => {
      alive = false;
    };
    // The caller passes the query key (the serialized filter) as `deps`; `fn` is
    // intentionally re-created each render and not a dependency.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return state;
}
