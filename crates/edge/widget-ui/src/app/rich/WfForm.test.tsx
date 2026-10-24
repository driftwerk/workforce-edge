// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import WfForm from "./WfForm";
import { validateForm, type FormSpec } from "./spec";
import { EdgeApiContext, type EdgeApi } from "./apiContext";
import { EdgeSubmitContext, type EdgeSubmit } from "./submitContext";
import * as api from "../api";

// The options_ref typeahead reuses fetchTablePage; stub it while keeping the real
// TableExpired class (WfForm narrows on it with `instanceof`).
vi.mock("../api", async (orig) => ({
  ...(await orig<typeof import("../api")>()),
  fetchTablePage: vi.fn(),
}));

const ARTIFACT = "art_0123456789abcdef0123456789abcdef";
const disabled = (el: Element) => (el as HTMLButtonElement).disabled;

function validSpec(raw: unknown): FormSpec {
  const v = validateForm(raw);
  if (!v.ok) throw new Error(v.error);
  return v.spec;
}

function renderForm(spec: FormSpec, submitForm = vi.fn().mockResolvedValue(true), edgeApi: EdgeApi | null = null) {
  const submit: EdgeSubmit = { submitForm };
  const utils = render(
    <EdgeApiContext.Provider value={edgeApi}>
      <EdgeSubmitContext.Provider value={submit}>
        <WfForm spec={spec} />
      </EdgeSubmitContext.Provider>
    </EdgeApiContext.Provider>,
  );
  return { submitForm, form: utils.container.querySelector("form")!, ...utils };
}

afterEach(() => {
  cleanup();
  vi.mocked(api.fetchTablePage).mockReset();
});

describe("WfForm", () => {
  it("blocks an invalid submit, then submits the summary + values once fixed", () => {
    const spec = validSpec({
      title: "Lead",
      submit_label: "Create",
      fields: [
        { name: "company", label: "Company", type: "text", required: true },
        { name: "status", label: "Status", type: "select", options: [{ value: "open", label: "Open" }], value: "open" },
      ],
    });
    const { submitForm, form } = renderForm(spec);
    const button = screen.getByRole("button", { name: "Create" });
    expect(disabled(button)).toBe(false);

    fireEvent.submit(form);
    expect(submitForm).not.toHaveBeenCalled();
    expect(screen.getByText("Required")).toBeTruthy();
    expect(disabled(button)).toBe(true);

    fireEvent.change(screen.getByLabelText(/Company/), { target: { value: "Acme" } });
    expect(disabled(button)).toBe(false);

    fireEvent.submit(form);
    expect(submitForm).toHaveBeenCalledTimes(1);
    expect(submitForm).toHaveBeenCalledWith(
      'Submitted "Lead"\n- Company: Acme\n- Status: Open',
      { company: "Acme", status: "open" },
    );
    // Locked after submit.
    expect(screen.getByRole("button", { name: "Submitted" })).toBeTruthy();
  });

  it("unlocks again when the submission is dropped before delivery", async () => {
    const spec = validSpec({
      submit_label: "Create",
      fields: [{ name: "company", label: "Company", type: "text", required: true, value: "Acme" }],
    });
    // A turn interrupted out of the queue before it runs resolves `false`.
    const { submitForm } = renderForm(spec, vi.fn().mockResolvedValue(false));
    const form = screen.getByRole("button", { name: "Create" }).closest("form")!;

    fireEvent.submit(form);
    expect(submitForm).toHaveBeenCalledTimes(1);
    // Optimistically locked, then reopened once delivery reports it never sent —
    // so the customer can resubmit instead of a form falsely reading "Submitted".
    expect(screen.getByRole("button", { name: "Submitted" })).toBeTruthy();
    await screen.findByRole("button", { name: "Create" });
  });

  it("maps options_ref rows to typeahead options and submits the picked value", async () => {
    vi.mocked(api.fetchTablePage).mockResolvedValue({
      columns: [
        { key: "id", label: "Id", type: "text" },
        { key: "name", label: "Name", type: "text" },
      ],
      rows: [
        ["u_1", "Ann"],
        ["u_2", "Bob"],
      ],
      total: 2,
    });
    const spec = validSpec({
      title: "Assign",
      submit_label: "Save",
      fields: [
        { name: "owner", label: "Owner", type: "select", required: true, options_ref: { artifact_id: ARTIFACT, value: "id", label: "name" } },
      ],
    });
    const edgeApi: EdgeApi = { base: "https://edge.test", agent: "support", token: { current: "tok" } };
    const { submitForm, form } = renderForm(spec, vi.fn().mockResolvedValue(true), edgeApi);

    const combo = screen.getByRole("combobox");
    fireEvent.focus(combo);
    fireEvent.change(combo, { target: { value: "an" } });

    // The debounced fetch resolves and its rows become options.
    const option = await screen.findByRole("button", { name: "Ann" });
    fireEvent.mouseDown(option);

    expect(api.fetchTablePage).toHaveBeenCalledWith(
      "https://edge.test",
      "support",
      "tok",
      ARTIFACT,
      expect.objectContaining({ offset: 0, search: "an" }),
    );

    fireEvent.submit(form);
    expect(submitForm).toHaveBeenCalledWith('Submitted "Assign"\n- Owner: Ann', { owner: "u_1" });
  });
});
