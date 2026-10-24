import { useCallback, useEffect, useState } from "react";

interface AsyncState<T> {
  data: T | null;
  loading: boolean;
  error: string | null;
}

/**
 * A small useQuery-style hook over a promise-returning `fn`: runs on mount
 * and whenever `deps` change, drops a result that a newer run has
 * overtaken, and hands back `reload` for after a write. `setData` lets a
 * write's response replace the data without a round trip.
 */
export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]): AsyncState<T> & {
  reload: () => void;
  setData: (data: T) => void;
} {
  const [state, setState] = useState<AsyncState<T>>({ data: null, loading: true, error: null });
  const [generation, setGeneration] = useState(0);
  useEffect(() => {
    let alive = true;
    setState((s) => ({ data: s.data, loading: true, error: null }));
    fn()
      .then((data) => {
        if (alive) setState({ data, loading: false, error: null });
      })
      .catch((e) => {
        if (alive) setState((s) => ({ data: s.data, loading: false, error: e instanceof Error ? e.message : String(e) }));
      });
    return () => {
      alive = false;
    };
    // `fn` is re-created each render on purpose; the query key is `deps`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, generation]);
  const reload = useCallback(() => setGeneration((g) => g + 1), []);
  const setData = useCallback((data: T) => setState({ data, loading: false, error: null }), []);
  return { ...state, reload, setData };
}
