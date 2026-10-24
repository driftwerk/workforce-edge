import { describe, expect, it } from "vitest";

import { parseRoute, routePath } from "./route";

describe("parseRoute", () => {
  it("reads the three places, defaulting to the tenants list", () => {
    expect(parseRoute("/platform")).toEqual({ screen: "tenants" });
    expect(parseRoute("/platform/")).toEqual({ screen: "tenants" });
    expect(parseRoute("/platform/tenants")).toEqual({ screen: "tenants" });
    expect(parseRoute("/platform/tenants/acme")).toEqual({ screen: "tenants", id: "acme" });
    expect(parseRoute("/platform/audit")).toEqual({ screen: "audit" });
    expect(parseRoute("/platform/nothing/here")).toEqual({ screen: "tenants" });
  });

  it("decodes the id and survives a malformed escape", () => {
    expect(parseRoute("/platform/tenants/acme%2Dltd")).toEqual({ screen: "tenants", id: "acme-ltd" });
    expect(parseRoute("/platform/tenants/%E0%A4%A")).toEqual({ screen: "tenants", id: "%E0%A4%A" });
  });
});

describe("routePath", () => {
  it("round-trips through parseRoute", () => {
    for (const route of [{ screen: "tenants" as const }, { screen: "tenants" as const, id: "acme" }, { screen: "audit" as const }]) {
      expect(parseRoute(routePath(route))).toEqual(route);
    }
  });
});
