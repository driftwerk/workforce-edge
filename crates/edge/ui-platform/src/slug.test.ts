import { describe, expect, it } from "vitest";

import { hostnameError, normaliseHostname, parseHostnames, suggestTenantId, tenantIdError } from "./slug";

describe("tenantIdError", () => {
  it("accepts a slug of three to sixty-three lowercase letters, digits and hyphens", () => {
    expect(tenantIdError("acme")).toBeNull();
    expect(tenantIdError("acme-ltd-2")).toBeNull();
    expect(tenantIdError("a".repeat(63))).toBeNull();
  });

  it("names what is wrong with a non-slug", () => {
    expect(tenantIdError("")).toMatch(/required/);
    expect(tenantIdError("Acme")).toMatch(/lowercase/i);
    expect(tenantIdError("acme ltd")).toMatch(/lowercase/i);
    expect(tenantIdError("-acme")).toMatch(/hyphen/);
    expect(tenantIdError("a".repeat(64))).toMatch(/63/);
  });

  it("refuses the ids the platform keeps: too short, or reserved", () => {
    expect(tenantIdError("a1")).toMatch(/at least 3/i);
    expect(tenantIdError("app")).toMatch(/reserved/);
    expect(tenantIdError("platform")).toMatch(/reserved/);
    expect(tenantIdError("default")).toMatch(/reserved/);
    expect(tenantIdError("apps")).toBeNull();
  });
});

describe("suggestTenantId", () => {
  it("slugifies a display name", () => {
    expect(suggestTenantId("Acme Ltd")).toBe("acme-ltd");
    expect(suggestTenantId("  Harbor & Co.  ")).toBe("harbor-co");
    expect(suggestTenantId("Ünïcode Name")).toBe("n-code-name");
  });

  it("stays within the id length", () => {
    expect(suggestTenantId("x".repeat(100))).toHaveLength(63);
  });
});

describe("hostnameError", () => {
  it("accepts lowercase DNS names, normalising case and a trailing dot", () => {
    expect(hostnameError("acme.marrilo.com")).toBeNull();
    expect(hostnameError("Acme.Marrilo.COM.")).toBeNull();
    expect(normaliseHostname(" Acme.Test. ")).toBe("acme.test");
  });

  it("refuses schemes, ports, paths and malformed labels", () => {
    expect(hostnameError("https://acme.test")).toMatch(/scheme/);
    expect(hostnameError("acme.test:8080")).toMatch(/scheme/);
    expect(hostnameError("acme..test")).toMatch(/Empty label/);
    expect(hostnameError("-acme.test")).toMatch(/hyphen/);
    expect(hostnameError("acme_1.test")).toMatch(/Lowercase/);
    expect(hostnameError("")).toMatch(/required/);
  });
});

describe("parseHostnames", () => {
  it("splits on newlines and commas, dedupes and normalises", () => {
    expect(parseHostnames("Acme.test\nchat.acme.test, acme.test\n\n")).toEqual({
      hostnames: ["acme.test", "chat.acme.test"],
    });
  });

  it("reports the first bad line with its fault", () => {
    expect(parseHostnames("acme.test\nhttp://bad")).toEqual({
      error: "http://bad: A bare hostname: no scheme, port or path.",
    });
  });

  it("is empty for an empty box", () => {
    expect(parseHostnames("  \n")).toEqual({ hostnames: [] });
  });
});
