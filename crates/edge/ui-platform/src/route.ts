// Where the console is, in the URL: /platform/tenants, /platform/tenants/<id>,
// /platform/audit. History routing, as the admin console does it, so a
// reload or a shared link lands on the same place.

export type Route = { screen: "tenants"; id?: string } | { screen: "audit" };

export const BASE = "/platform";

/** The route a pathname names; anything unknown is the tenants list. */
export function parseRoute(pathname: string): Route {
  const segs = pathname
    .replace(/^\/platform\/?/, "")
    .split("/")
    .filter(Boolean)
    .map((seg) => {
      try {
        return decodeURIComponent(seg);
      } catch {
        return seg;
      }
    });
  if (segs[0] === "audit") return { screen: "audit" };
  if (segs[0] === "tenants" && segs[1]) return { screen: "tenants", id: segs[1] };
  return { screen: "tenants" };
}

/** The pathname of a route. */
export function routePath(route: Route): string {
  if (route.screen === "audit") return `${BASE}/audit`;
  if (route.id) return `${BASE}/tenants/${encodeURIComponent(route.id)}`;
  return `${BASE}/tenants`;
}
