//! System-prompt guidance for the widget's rich-output fences.
//!
//! When an agent has `data_tables` / `charts` enabled, its replies may carry
//! `wf-table` / `wf-chart` fenced blocks that the widget renders as interactive
//! components. The block below teaches the model the exact JSON contract —
//! including few-shot examples, which measurably raise spec validity — and is
//! only injected for enabled features, so the model never learns a syntax the
//! widget would refuse to render.

const HEADER: &str = "\n\n## Rich output\n\
    When you present structured data, use the special fenced blocks below. Each block \
    must contain ONLY strict JSON — double-quoted keys and strings, no trailing commas, \
    no comments. Numbers must be JSON numbers, not strings — the widget adds thousands \
    separators and sensible decimals when it displays them. In prose and plain markdown \
    tables, write numbers the way a person reads them: `523,797.33`, not `523797.33`. \
    Never invent or pad data. Add a one-sentence takeaway near the block, and do not \
    repeat the full data in prose.";

const TABLES: &str = "\n\n### Data tables\n\
    For any tabular result (lists of records, comparisons, breakdowns) of 3 or more rows, \
    emit a `wf-table` block:\n\n\
    ```wf-table\n\
    {\"title\":\"Q3 invoices\",\"columns\":[\"Customer\",\
    {\"key\":\"invoiced\",\"label\":\"Invoiced\",\"format\":\"currency\",\"unit\":\"£\"},\"Due date\"],\
    \"rows\":[[\"Acme Ltd\",12800.5,\"2026-07-01\"],[\"Birch & Co\",9400,\"2026-07-14\"],\
    [\"Cedar Partners\",6250,\"2026-07-21\"]]}\n\
    ```\n\n\
    - `columns`: an array of names, or objects `{\"key\":\"...\",\"label\":\"...\",\
    \"align\":\"left|right\",\"format\":\"...\",\"unit\":\"£\"}`.\n\
    - `rows`: arrays aligned with `columns`. Cells are strings, numbers, booleans, or null.\n\
    - Emit dates as ISO `YYYY-MM-DD` strings and money as plain numbers.\n\
    - Numeric cells display with thousands separators and sensible decimals. Set `format` \
    when the column name alone would mislead: `currency` (always two decimals, prefixed \
    with `unit`) for money, `percent` for percentages, `integer` for counts, `id` for \
    references, codes and years that must never be grouped, `number` for any other measure.\n\
    - For a LARGE result staged out of context (you were handed an `artifact_id` for \
    a spilled tool result or a `data.query` result), reference it instead of inlining \
    the rows — the widget loads, sorts, filters and pages the whole dataset. You may add \
    `columns` objects to relabel or format its columns by key:\n\n\
    ```wf-table\n\
    {\"title\":\"Recent leads\",\"artifact_id\":\"art_0123456789abcdef0123456789abcdef\",\
    \"columns\":[{\"key\":\"total_fee\",\"label\":\"Fee\",\"format\":\"currency\",\"unit\":\"£\"}]}\n\
    ```\n\
    Never paste a staged result's preview rows into an inline table — reference the \
    artifact so the customer gets the full, browsable table.";

const CHARTS: &str = "\n\n### Charts\n\
    When a trend, share, or comparison is clearer as a picture, emit a `wf-chart` block. \
    `kind` is one of `pie`, `bar`, `line`, `area`, `scatter`:\n\n\
    ```wf-chart\n\
    {\"kind\":\"bar\",\"title\":\"Signups by plan\",\"labels\":[\"Free\",\"Pro\",\"Team\"],\
    \"series\":[{\"name\":\"March\",\"data\":[420,180,60]},{\"name\":\"April\",\"data\":[510,220,90]}]}\n\
    ```\n\n\
    ```wf-chart\n\
    {\"kind\":\"pie\",\"title\":\"Ticket status\",\"labels\":[\"Open\",\"Pending\",\"Closed\"],\
    \"series\":[{\"data\":[14,8,52]}]}\n\
    ```\n\n\
    - `labels`: category names (x-axis, or pie slice names). `series[].data`: numbers \
    aligned with `labels`; for `scatter`, `[x,y]` number pairs instead.\n\
    - Optional: `\"stacked\":true` (bar/line/area), `\"horizontal\":true` (bar), \
    `\"x_label\"`, `\"y_label\"`, `\"unit\"` (e.g. `\"£\"`, shown in tooltips).\n\
    - One series per pie, at most 8 series per chart. Prefer a table when there are \
    more than ~8 categories.";

const FORMS: &str = "\n\n### Forms\n\
    When you need the customer to supply structured input before you can act (create a \
    record, update fields), emit a `wf-form` block. They fill it in and their exact values \
    return to you next turn as the arguments for that action:\n\n\
    ```wf-form\n\
    {\"title\":\"New lead\",\"submit_label\":\"Create lead\",\"note\":\"We'll create this in the CRM.\",\
    \"fields\":[\
    {\"name\":\"company\",\"label\":\"Company\",\"type\":\"text\",\"required\":true,\"max_length\":120},\
    {\"name\":\"email\",\"label\":\"Email\",\"type\":\"email\",\"required\":true},\
    {\"name\":\"value\",\"label\":\"Deal value (£)\",\"type\":\"number\",\"min\":0},\
    {\"name\":\"status\",\"label\":\"Status\",\"type\":\"select\",\"required\":true,\
    \"options\":[{\"value\":\"open\",\"label\":\"Open\"},{\"value\":\"won\",\"label\":\"Won\"},\
    {\"value\":\"lost\",\"label\":\"Lost\"}],\"value\":\"open\"},\
    {\"name\":\"owner\",\"label\":\"Owner\",\"type\":\"select\",\"required\":true,\
    \"options_ref\":{\"artifact_id\":\"art_0123456789abcdef0123456789abcdef\",\"value\":\"id\",\"label\":\"name\"}}\
    ]}\n\
    ```\n\n\
    - Every field has `name` (the unique tool-argument key) and `type`, plus optional `label` \
    (defaults to `name`), `required` (default false), `help` (one line under the control), \
    `value` (prefill), and `error` (a message shown under the field).\n\
    - Types: `text`/`textarea` (`placeholder`, `min_length`, `max_length`, `pattern`, `rows`); \
    `email`; `number` (`min`, `max`, `step`); `date` (ISO `YYYY-MM-DD`, `min`, `max`); `select` \
    (`options` or `options_ref`; add `multiple` for an array value); `checkbox` (no `options` is \
    a single boolean, with `options` a multi-select); `radio` (`options` required); `hidden` (a \
    `value` carried straight through). An option is `{\"value\":\"id\",\"label\":\"Name\"}` — label \
    defaults to value.\n\
    - Resolve options BEFORE emitting the form. A fixed enum you already hold: inline it in \
    `options` (only for small sets, roughly 50 or fewer). A dynamic list (owners, accounts, \
    pipelines): call the relevant list tool, then inline it if small, or — if it is large — \
    stage it as a tabular artifact and reference it with `options_ref` (its `artifact_id` plus \
    the `value` and `label` column names). Never inline a large list; it overflows the reply and \
    truncates the block.\n\
    - Prefill `value` with sensible defaults, or the record's current values when editing.\n\
    - When the customer submits, take the action with the exact values they returned. If a tool \
    REJECTS a value, re-emit the SAME `wf-form` with every field's `value` preserved and a \
    per-field `error` on the ones at fault, so they fix only what is wrong — never make them \
    re-enter the whole form.\n\
    - Never put secrets or fields the customer cannot see in a `hidden` field.";

const MAPS: &str = "\n\n### Maps\n\
    When geography itself is the point — a metric that varies by region and reads best \
    as a shaded map — emit a `wf-map` choropleth. Only when the geography matters: \
    otherwise keep using a chart or table, and never map data that has no geography:\n\n\
    ```wf-map\n\
    {\"kind\":\"choropleth\",\"title\":\"January 2026 leads by postcode area\",\
    \"geo\":\"uk-postcode-areas\",\"value_label\":\"Leads\",\"unit\":\"\",\
    \"regions\":[{\"name\":\"W\",\"value\":489},{\"name\":\"SW\",\"value\":451},\
    {\"name\":\"EC\",\"value\":92}]}\n\
    ```\n\n\
    - `kind` is always `\"choropleth\"`. `regions`: one `{\"name\":…,\"value\":…}` per \
    region, values plain numbers. `geo` names the geography and fixes how you spell \
    each region:\n\
    - `geo`:`\"uk-postcode-areas\"` — `name` is the UK postcode AREA code: a postcode's \
    leading letters with the district digit(s) and the rest removed, uppercase \
    (`SW1A 1AA` → `\"SW\"`, `EC2R` → `\"EC\"`, `M1` → `\"M\"`, `LS6` → `\"LS\"`).\n\
    - `geo`:`\"world-countries\"` — `name` is the country's English name as Natural \
    Earth spells it (`\"United Kingdom\"`, `\"United States of America\"`, `\"France\"`, \
    `\"Czechia\"`).\n\
    - Optional: `\"value_label\"` (legend/tooltip label), `\"unit\"` (tooltip prefix, \
    e.g. `\"£\"`), and `\"min\"`/`\"max\"`. For a non-count metric with a narrow range \
    (e.g. revenue 200k–500k), set `min`/`max` so the scale spans the data rather than \
    0→max.\n\
    - Use the exact codes/names above; unmatched regions are dropped with a visible \
    notice, so never invent them. You may pair a table of the same data for an exact, \
    accessible view.";

const DIAGRAMS: &str = "\n\n### Diagrams\n\
    When the answer is a STRUCTURE — a process, a decision path, a hand-off between \
    people or systems, or an ordering of events — emit a `wf-diagram` block. Describe \
    it in meaning only: name the boxes and what connects to what, never positions or \
    coordinates. `kind` is `flow` or `sequence`:\n\n\
    ```wf-diagram\n\
    {\"kind\":\"flow\",\"title\":\"Order pipeline\",\"direction\":\"down\",\
    \"nodes\":[{\"id\":\"in\",\"label\":\"Order received\",\"shape\":\"round\"},\
    {\"id\":\"chk\",\"label\":\"Stock available?\",\"shape\":\"diamond\"},\
    {\"id\":\"ship\",\"label\":\"Pick and ship\",\"group\":\"Warehouse\"},\
    {\"id\":\"bo\",\"label\":\"Backorder\",\"group\":\"Warehouse\",\"tone\":\"warn\"}],\
    \"edges\":[{\"from\":\"in\",\"to\":\"chk\"},\
    {\"from\":\"chk\",\"to\":\"ship\",\"label\":\"yes\"},\
    {\"from\":\"chk\",\"to\":\"bo\",\"label\":\"no\"}]}\n\
    ```\n\n\
    ```wf-diagram\n\
    {\"kind\":\"sequence\",\"title\":\"Refund request\",\
    \"participants\":[{\"id\":\"c\",\"label\":\"Customer\"},{\"id\":\"a\",\"label\":\"Agent\"},\
    {\"id\":\"p\",\"label\":\"Payments\"}],\
    \"steps\":[{\"from\":\"c\",\"to\":\"a\",\"label\":\"Requests refund\"},\
    {\"from\":\"a\",\"to\":\"p\",\"label\":\"Check eligibility\"},\
    {\"from\":\"p\",\"to\":\"a\",\"label\":\"Eligible\",\"style\":\"dashed\"},\
    {\"from\":\"a\",\"to\":\"c\",\"label\":\"Confirms refund\"}]}\n\
    ```\n\n\
    - Use a diagram only for structure: steps, decisions, connections, ordering. Never \
    for quantities, never for a list of records, and never for something with no \
    structure to show.\n\
    - KEEP IT SMALL: aim for about 12 nodes and never more than 20 — it is read in a \
    narrow panel, and beyond that a table is clearer. Labels of at most 6 words.\n\
    - `flow`: every node needs a unique short `id` and a `label`; every edge's `from` \
    and `to` must name a declared node (unmatched ones are dropped with a visible \
    notice, so never invent ids). Optional per edge: `label`, `\"style\":\"dashed\"`, \
    `\"arrow\":\"both\"` or `\"none\"`.\n\
    - `shape`: `round` for a start or end, `diamond` for a decision (label it as a \
    question and label its outgoing edges with the answers), `cylinder` for a store, \
    `box` otherwise. `tone` (`accent`, `good`, `warn`, `bad`) only to mark an \
    exception or failure path — never to decorate.\n\
    - `group` bands together nodes owned by the same team or system, and each \
    group is given its own colour automatically — so grouping is worth doing \
    whenever a process crosses more than one team. Never ask for colours \
    yourself; there is no colour field.\n\
    - `\"direction\":\"down\"` (the default) suits a narrow panel; use `\"right\"` only \
    for a short chain.\n\
    - `sequence`: 2 to 12 `participants` with unique ids, then `steps` in time order. \
    `\"style\":\"dashed\"` reads as a reply, `from` equal to `to` is a self-step, and a \
    step may carry a `note` shown beneath it.\n\
    - Leave the visual style alone unless the customer asks for a hand-drawn or \
    whiteboard look, then add `\"style\":\"sketch\"`.\n\
    - Add a one-sentence takeaway near the block and do not restate the whole diagram \
    in prose.";

const IMAGES: &str = "\n\n### Showing pictures\n\
    You CAN show a picture back to the customer. Any file with an `artifact_id` \
    that you have been told about — one they attached earlier in this \
    conversation, or one a tool returned to you — can be put in the reply by \
    emitting a `wf-image` block naming that id:\n\n\
    ```wf-image\n\
    {\"images\":[{\"artifact_id\":\"art_0123456789abcdef0123456789abcdef\",\
    \"caption\":\"the photo you sent\"}]}\n\
    ```\n\n\
    - ALWAYS a list, even for one picture — several images in one block are laid \
    out together, which is what \"show me the photos for this item\" wants.\n\
    - Use ONLY an `artifact_id` you have actually been given. Never invent one, \
    never guess at the format, and never put a web address here — an id you made \
    up shows the customer a broken picture.\n\
    - `caption` is optional and shown beneath. Add `alt` when the picture needs \
    describing for someone who cannot see it.\n\
    - When the customer asks to see something again, show it rather than \
    explaining that you cannot.\n\
    - If a tool gave you image URLs rather than ids, pass them to `image.fetch` \
    first — it returns an `artifact_id` per picture. Never put a URL in a \
    `wf-image` block, and never hand a raw URL to the customer instead of \
    showing the picture.";

/// The rich-output system block for the enabled features, or `None` when they
/// are all off.
pub fn system_block(
    tables: bool,
    charts: bool,
    forms: bool,
    maps: bool,
    diagrams: bool,
    images: bool,
) -> Option<String> {
    if !tables && !charts && !forms && !maps && !diagrams && !images {
        return None;
    }
    let mut block = String::from(HEADER);
    if tables {
        block.push_str(TABLES);
    }
    if charts {
        block.push_str(CHARTS);
    }
    if forms {
        block.push_str(FORMS);
    }
    if maps {
        block.push_str(MAPS);
    }
    if diagrams {
        block.push_str(DIAGRAMS);
    }
    if images {
        block.push_str(IMAGES);
    }
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fenced example in `block` must parse as strict JSON — the model
    /// copies these verbatim, so a malformed example would teach it garbage.
    fn assert_examples_parse(block: &str) {
        let mut rest = block;
        let mut seen = 0;
        while let Some(open) = rest.find("```wf-") {
            let after = &rest[open..];
            let body_start = after.find('\n').expect("fence has a body") + 1;
            let body_end = after[body_start..].find("```").expect("fence closes");
            let json = &after[body_start..body_start + body_end];
            serde_json::from_str::<serde_json::Value>(json.trim())
                .unwrap_or_else(|e| panic!("example must be strict JSON: {e}\n{json}"));
            seen += 1;
            rest = &after[body_start + body_end..];
        }
        assert!(seen > 0, "block contains at least one example");
    }

    #[test]
    fn disabled_features_produce_no_block() {
        assert!(system_block(false, false, false, false, false, false).is_none());
    }

    #[test]
    fn tables_only_block_teaches_tables_not_charts() {
        let block = system_block(true, false, false, false, false, false).unwrap();
        assert!(block.contains("wf-table"));
        assert!(!block.contains("wf-chart"));
        assert!(!block.contains("wf-form"));
        assert!(!block.contains("wf-map"));
        assert!(!block.contains("wf-diagram"));
        assert_eq!(block.matches("## Rich output").count(), 1);
        assert_examples_parse(&block);
    }

    #[test]
    fn charts_only_block_teaches_charts_not_tables() {
        let block = system_block(false, true, false, false, false, false).unwrap();
        assert!(block.contains("wf-chart"));
        assert!(!block.contains("wf-table"));
        assert!(!block.contains("wf-form"));
        assert!(!block.contains("wf-map"));
        assert!(!block.contains("wf-diagram"));
        assert_examples_parse(&block);
    }

    #[test]
    fn forms_only_block_teaches_forms_not_tables_or_charts() {
        let block = system_block(false, false, true, false, false, false).unwrap();
        assert!(block.contains("wf-form"));
        assert!(!block.contains("wf-map"));
        assert!(!block.contains("wf-table"));
        assert!(!block.contains("wf-chart"));
        assert!(!block.contains("wf-diagram"));
        assert_eq!(block.matches("## Rich output").count(), 1);
        assert_examples_parse(&block);
    }

    #[test]
    fn maps_only_block_teaches_maps_not_others() {
        let block = system_block(false, false, false, true, false, false).unwrap();
        assert!(block.contains("wf-map"));
        assert!(!block.contains("wf-form"));
        assert!(!block.contains("wf-table"));
        assert!(!block.contains("wf-chart"));
        assert!(!block.contains("wf-diagram"));
        assert_eq!(block.matches("## Rich output").count(), 1);
        assert_examples_parse(&block);
    }

    #[test]
    fn diagrams_only_block_teaches_diagrams_not_others() {
        let block = system_block(false, false, false, false, true, false).unwrap();
        assert!(block.contains("wf-diagram"));
        assert!(!block.contains("wf-map"));
        assert!(!block.contains("wf-form"));
        assert!(!block.contains("wf-table"));
        assert!(!block.contains("wf-chart"));
        assert_eq!(block.matches("## Rich output").count(), 1);
        assert_examples_parse(&block);
    }

    /// Both diagram kinds must be taught — a `flow`-only block would leave the
    /// model no way to express an interaction over time.
    #[test]
    fn diagrams_block_teaches_both_kinds() {
        let block = system_block(false, false, false, false, true, false).unwrap();
        assert!(block.contains(r#""kind":"flow""#));
        assert!(block.contains(r#""kind":"sequence""#));
    }

    #[test]
    fn images_only_block_teaches_images_not_others() {
        let block = system_block(false, false, false, false, false, true).unwrap();
        assert!(block.contains("wf-image"));
        assert!(!block.contains("wf-diagram"));
        assert!(!block.contains("wf-map"));
        assert!(!block.contains("wf-form"));
        assert!(!block.contains("wf-table"));
        assert!(!block.contains("wf-chart"));
        assert_eq!(block.matches("## Rich output").count(), 1);
        assert_examples_parse(&block);
    }

    /// A `wf-image` names an artifact by id. A model that invents one shows the
    /// customer a broken picture, so the block must say so outright — and must
    /// never suggest a URL belongs in that field.
    #[test]
    fn images_block_forbids_invented_ids_and_urls() {
        let block = system_block(false, false, false, false, false, true).unwrap();
        assert!(block.contains("Never invent one"));
        assert!(block.to_lowercase().contains("never put a web address"));
        // Always a list: a singular form would give the model two names for one
        // concept, which is how the diagram/chart routing bug happened.
        assert!(block.contains(r#""images":["#));
    }

    #[test]
    fn all_features_share_one_header() {
        let block = system_block(true, true, true, true, true, true).unwrap();
        assert!(block.contains("wf-table"));
        assert!(block.contains("wf-chart"));
        assert!(block.contains("wf-form"));
        assert!(block.contains("wf-map"));
        assert!(block.contains("wf-diagram"));
        assert!(block.contains("wf-image"));
        assert_eq!(block.matches("## Rich output").count(), 1);
        assert_examples_parse(&block);
    }
}
