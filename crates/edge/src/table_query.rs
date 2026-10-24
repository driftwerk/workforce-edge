//! Server-side filter/sort/paginate over staged tabular rows, for the widget's
//! artifact-backed data tables. Pure and semantics-matched to the widget's inline
//! table (`spec.ts`) so a small inline table and a large server-backed one behave
//! identically. Row objects are keyed by column name (`ArtifactDescriptor::schema`
//! carries the order); a page materialises only its own cells.

use std::cmp::Ordering;

use serde_json::Value;

use workforce_agent::artifacts::tabular::rows_to_cells;

/// Inferred per-column type, driving comparators, filter widgets, and alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Number,
    Date,
    Text,
}

impl ColumnType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Date => "date",
            Self::Text => "text",
        }
    }
}

/// One resolved per-column predicate (column already resolved to an index).
#[derive(Debug)]
pub enum Filter {
    /// Case-insensitive substring on the cell's display text (needle lowercased).
    Contains { col: usize, needle: String },
    /// Numeric/date value ≥ bound (dates as epoch millis).
    Min { col: usize, bound: f64 },
    /// Numeric/date value ≤ bound.
    Max { col: usize, bound: f64 },
}

/// A resolved page request. Columns/values are already validated against the
/// schema by the caller; `search`/`Contains` needles are lowercased.
#[derive(Debug, Default)]
pub struct TableQuery {
    pub offset: usize,
    pub limit: usize,
    /// `(column index, descending)`.
    pub sort: Option<(usize, bool)>,
    pub search: Option<String>,
    pub filters: Vec<Filter>,
}

/// One page of cells (schema order) plus the total row count after filtering.
#[derive(Debug)]
pub struct QueriedPage {
    pub rows: Vec<Vec<Value>>,
    pub total: usize,
}

/// The cell value for `key`, treating JSON null, a missing key, and the empty
/// string all as "no value" (`None`) — matching the client's `cellValue`.
fn value<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    match row.get(key) {
        Some(Value::Null) | None => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(v) => Some(v),
    }
}

/// A number or a finite numeric string (an LLM habit), else `None`.
fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Epoch millis (UTC) for an ISO date or datetime string, else `None`. Server-side
/// parsing is uniform (no host timezone), so ordering is self-consistent.
fn as_date(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp_millis() as f64);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis() as f64);
    }
    [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|fmt| chrono::NaiveDateTime::parse_from_str(s, fmt).ok())
    .map(|ndt| ndt.and_utc().timestamp_millis() as f64)
}

/// Epoch millis (UTC) for an ISO date/datetime string — the range-filter bound
/// parser for date columns, matching cell parsing.
pub fn parse_date_millis(s: &str) -> Option<f64> {
    as_date(s)
}

/// The cell's display text: null → "", bool → `true`/`false`, else its scalar
/// text. Used for search and `Contains`.
fn display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// The numeric key for a range filter under the column's type.
fn range_value(v: &Value, ty: ColumnType) -> Option<f64> {
    match ty {
        ColumnType::Number => as_number(v),
        ColumnType::Date => v.as_str().and_then(as_date),
        ColumnType::Text => None,
    }
}

/// Per-column type: `Number` if every present cell is numeric, `Date` if every
/// present cell is an ISO date, else `Text` (all-empty ⇒ `Text`).
pub fn infer_types(schema: &[String], rows: &[Value]) -> Vec<ColumnType> {
    schema
        .iter()
        .map(|key| {
            let mut seen = 0usize;
            let mut nums = 0usize;
            let mut dates = 0usize;
            for row in rows {
                let Some(v) = value(row, key) else { continue };
                seen += 1;
                if as_number(v).is_some() {
                    nums += 1;
                } else if v.as_str().and_then(as_date).is_some() {
                    dates += 1;
                }
            }
            if seen == 0 || nums != seen && dates != seen {
                ColumnType::Text
            } else if nums == seen {
                ColumnType::Number
            } else {
                ColumnType::Date
            }
        })
        .collect()
}

/// Sort key for one cell: numeric for `Number`/`Date`, text otherwise; `None`
/// (empty/unparseable) sorts last in both directions.
enum SortKey {
    Num(f64),
    Text(String),
}

fn sort_key(row: &Value, key: &str, ty: ColumnType) -> Option<SortKey> {
    let v = value(row, key)?;
    match ty {
        ColumnType::Number => as_number(v).map(SortKey::Num),
        ColumnType::Date => v.as_str().and_then(as_date).map(SortKey::Num),
        ColumnType::Text => Some(SortKey::Text(display(v))),
    }
}

fn cmp_keys(a: &SortKey, b: &SortKey) -> Ordering {
    match (a, b) {
        (SortKey::Num(x), SortKey::Num(y)) => x.total_cmp(y),
        (SortKey::Text(x), SortKey::Text(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

fn ordering(a: Option<&SortKey>, b: Option<&SortKey>, descending: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let o = cmp_keys(x, y);
            if descending {
                o.reverse()
            } else {
                o
            }
        }
    }
}

fn passes(row: &Value, schema: &[String], types: &[ColumnType], q: &TableQuery) -> bool {
    if let Some(needle) = &q.search {
        let hit = schema
            .iter()
            .filter_map(|k| value(row, k))
            .any(|v| display(v).to_lowercase().contains(needle));
        if !hit {
            return false;
        }
    }
    q.filters.iter().all(|f| match f {
        Filter::Contains { col, needle } => {
            value(row, &schema[*col]).is_some_and(|v| display(v).to_lowercase().contains(needle))
        }
        Filter::Min { col, bound } => value(row, &schema[*col])
            .and_then(|v| range_value(v, types[*col]))
            .is_some_and(|x| x >= *bound),
        Filter::Max { col, bound } => value(row, &schema[*col])
            .and_then(|v| range_value(v, types[*col]))
            .is_some_and(|x| x <= *bound),
    })
}

/// Filter → sort → paginate, materialising only the requested page.
pub fn query(
    schema: &[String],
    types: &[ColumnType],
    rows: &[Value],
    q: &TableQuery,
) -> QueriedPage {
    let mut idx: Vec<usize> = (0..rows.len())
        .filter(|&i| passes(&rows[i], schema, types, q))
        .collect();
    let total = idx.len();
    if let Some((col, descending)) = q.sort {
        let (key, ty) = (&schema[col], types[col]);
        let mut decorated: Vec<(Option<SortKey>, usize)> = idx
            .iter()
            .map(|&i| (sort_key(&rows[i], key, ty), i))
            .collect();
        decorated.sort_by(|(a, _), (b, _)| ordering(a.as_ref(), b.as_ref(), descending));
        idx = decorated.into_iter().map(|(_, i)| i).collect();
    }
    let page: Vec<Value> = idx
        .into_iter()
        .skip(q.offset)
        .take(q.limit)
        .map(|i| rows[i].clone())
        .collect();
    QueriedPage {
        rows: rows_to_cells(&page, schema),
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows() -> Vec<Value> {
        vec![
            json!({"name": "Acme", "amount": 100, "due": "2026-07-03"}),
            json!({"name": "Birch", "amount": "40", "due": "2026-07-01"}),
            json!({"name": "cedar", "amount": 250, "due": "2026-07-02"}),
            json!({"name": "Delta", "amount": null, "due": ""}),
        ]
    }
    fn schema() -> Vec<String> {
        ["name", "amount", "due"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn infers_number_date_text_with_numeric_strings_and_nulls() {
        let s = schema();
        let types = infer_types(&s, &rows());
        assert_eq!(
            types,
            vec![ColumnType::Text, ColumnType::Number, ColumnType::Date]
        );
        // A column mixing a number and a word is Text; all-empty is Text.
        let mixed = vec![json!({"c": 1}), json!({"c": "x"}), json!({"c": null})];
        assert_eq!(infer_types(&["c".into()], &mixed), vec![ColumnType::Text]);
        assert_eq!(
            infer_types(&["c".into()], &[json!({"c": ""})]),
            vec![ColumnType::Text]
        );
    }

    #[test]
    fn sorts_numerically_with_nulls_last_both_directions() {
        let (s, r) = (schema(), rows());
        let t = infer_types(&s, &r);
        let asc = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 0,
                limit: 10,
                sort: Some((1, false)),
                ..Default::default()
            },
        );
        // amount asc: 40, 100, 250, then null (Delta) last.
        assert_eq!(
            asc.rows
                .iter()
                .map(|row| row[0].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["Birch", "Acme", "cedar", "Delta"]
        );
        let desc = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 0,
                limit: 10,
                sort: Some((1, true)),
                ..Default::default()
            },
        );
        // desc: 250, 100, 40, null still last.
        assert_eq!(
            desc.rows
                .iter()
                .map(|row| row[0].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["cedar", "Acme", "Birch", "Delta"]
        );
    }

    #[test]
    fn sorts_dates_and_text_case_sensitively() {
        let (s, r) = (schema(), rows());
        let t = infer_types(&s, &r);
        let by_date = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 0,
                limit: 10,
                sort: Some((2, false)),
                ..Default::default()
            },
        );
        assert_eq!(by_date.rows[0][0].as_str().unwrap(), "Birch"); // 07-01 first, empty last
        assert_eq!(by_date.rows[3][0].as_str().unwrap(), "Delta");
        let by_name = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 0,
                limit: 10,
                sort: Some((0, false)),
                ..Default::default()
            },
        );
        // ASCII/lexicographic: uppercase before lowercase, so "cedar" sorts last.
        assert_eq!(by_name.rows[3][0].as_str().unwrap(), "cedar");
    }

    #[test]
    fn contains_and_range_filters_and_search() {
        let (s, r) = (schema(), rows());
        let t = infer_types(&s, &r);
        let contains = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 0,
                limit: 10,
                filters: vec![Filter::Contains {
                    col: 0,
                    needle: "c".into(),
                }],
                ..Default::default()
            },
        );
        assert_eq!(contains.total, 3); // "Acme", "birCh", "cedar" — case-insensitive substring
        let range = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 0,
                limit: 10,
                filters: vec![
                    Filter::Min {
                        col: 1,
                        bound: 100.0,
                    },
                    Filter::Max {
                        col: 1,
                        bound: 200.0,
                    },
                ],
                ..Default::default()
            },
        );
        assert_eq!(range.total, 1); // only 100 (null excluded, 250 over, 40 under)
        assert_eq!(range.rows[0][0].as_str().unwrap(), "Acme");
        let search = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 0,
                limit: 10,
                search: Some("birch".into()),
                ..Default::default()
            },
        );
        assert_eq!(search.total, 1);
    }

    #[test]
    fn paginates_with_total_reflecting_filter() {
        let (s, r) = (schema(), rows());
        let t = infer_types(&s, &r);
        let page = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 2,
                limit: 1,
                sort: Some((0, false)),
                ..Default::default()
            },
        );
        assert_eq!(page.total, 4);
        assert_eq!(page.rows.len(), 1);
        // Offset past the end yields an empty page but the true total.
        let past = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 99,
                limit: 10,
                ..Default::default()
            },
        );
        assert_eq!(past.total, 4);
        assert!(past.rows.is_empty());
    }

    #[test]
    fn missing_cells_fill_null_in_schema_order() {
        let r = vec![json!({"a": 1})];
        let s: Vec<String> = ["a", "b"].iter().map(|x| x.to_string()).collect();
        let t = infer_types(&s, &r);
        let page = query(
            &s,
            &t,
            &r,
            &TableQuery {
                offset: 0,
                limit: 10,
                ..Default::default()
            },
        );
        assert_eq!(page.rows[0], vec![json!(1), Value::Null]);
    }
}
