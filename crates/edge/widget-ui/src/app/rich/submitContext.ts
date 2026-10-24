import { createContext } from "react";

/** A single submitted form value. Scalars for text/number/checkbox/select-single;
 *  a string array for a multi-value select or checkbox group. */
export type SubmitValue = string | number | boolean | string[];

/** Delivers a completed form back to the running conversation: the readable
 *  `summary` becomes the user turn, `values` the exact machine arguments. Resolves
 *  `true` once the turn is on its way, `false` if it was dropped from the interrupt
 *  queue before running — so the form only stays locked when it really was sent. */
export interface EdgeSubmit {
  submitForm: (summary: string, values: Record<string, SubmitValue>) => Promise<boolean>;
}

export const EdgeSubmitContext = createContext<EdgeSubmit | null>(null);
