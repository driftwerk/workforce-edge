import { createContext, type RefObject } from "react";

/** The authed edge endpoint a server-backed table fetches from. `token` is a ref
 *  so a refresh updates it without re-rendering (and re-fetching) the table. */
export interface EdgeApi {
  base: string;
  agent: string;
  token: RefObject<string>;
}

export const EdgeApiContext = createContext<EdgeApi | null>(null);
