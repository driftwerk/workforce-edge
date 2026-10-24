// The boundary sources a wf-map can render. Each geography is a normalized
// GeoJSON asset (every feature's sole property is its canonical `name`) imported
// as a `?url` — so tsc never parses the ~160KB literal and each file is emitted
// as its own hashed, same-origin asset fetched only when a map of that geography
// first renders. Provenance, licences and attribution live in ./NOTICE.md.

import ukPostcodeAreas from "./uk-postcode-areas.json?url";
import worldCountries from "./world-countries.json?url";

import { retryImport } from "../lazyRetry";
import { GEOGRAPHY_IDS, type GeographyId } from "./ids";

export interface GeoFeature {
  type: "Feature";
  properties: { name: string };
  geometry: { type: string; coordinates: unknown };
}

export interface GeoFeatureCollection {
  type: "FeatureCollection";
  features: GeoFeature[];
}

export interface Geography {
  url: string;
  label: string;
  attribution?: string;
  /** Lowercased region-name variants the model may emit, mapped to the canonical
   *  feature name — the net for spellings that differ from the source data. */
  aliases?: Record<string, string>;
}

export const GEOGRAPHIES: Record<GeographyId, Geography> = {
  "uk-postcode-areas": {
    url: ukPostcodeAreas,
    label: "UK postcode areas",
    attribution: "UK postcode boundaries © Wikipedia contributors, CC BY-SA 3.0",
  },
  "world-countries": {
    url: worldCountries,
    label: "World countries",
    attribution: "Made with Natural Earth",
    aliases: {
      "united states": "United States of America",
      usa: "United States of America",
      us: "United States of America",
      uk: "United Kingdom",
      "great britain": "United Kingdom",
      britain: "United Kingdom",
      "czech republic": "Czechia",
      "ivory coast": "Côte d'Ivoire",
      "democratic republic of the congo": "Dem. Rep. Congo",
      "dr congo": "Dem. Rep. Congo",
      "republic of the congo": "Congo",
      "bosnia and herzegovina": "Bosnia and Herz.",
      "central african republic": "Central African Rep.",
      "dominican republic": "Dominican Rep.",
      "equatorial guinea": "Eq. Guinea",
      "south sudan": "S. Sudan",
      "western sahara": "W. Sahara",
      macedonia: "North Macedonia",
      burma: "Myanmar",
      swaziland: "eSwatini",
      "east timor": "Timor-Leste",
    },
  },
};

const cache = new Map<GeographyId, Promise<GeoFeatureCollection>>();

/** Fetch and parse a geography's boundaries, retried on transient failure and
 *  memoized so a second map of the same geography reuses the first load. */
export function loadGeography(id: GeographyId): Promise<GeoFeatureCollection> {
  const hit = cache.get(id);
  if (hit) return hit;
  const load = retryImport(async () => {
    const res = await fetch(GEOGRAPHIES[id].url, { cache: "force-cache" });
    if (!res.ok) throw new Error(`geography ${id} failed (${res.status})`);
    return (await res.json()) as GeoFeatureCollection;
  }).catch((err) => {
    cache.delete(id);
    throw err;
  });
  cache.set(id, load);
  return load;
}

export { GEOGRAPHY_IDS };
