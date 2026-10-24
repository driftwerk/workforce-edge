//! The tiny JSON protocol the widget and the public socket speak. Kept edge-
//! native (no shared gateway protocol type) so the customer surface exposes
//! exactly these fields and nothing more.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A frame from the widget. Only `chat` and `form_submit` frames are acted on.
#[derive(Debug, Deserialize)]
pub struct ClientFrame {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub content: Option<String>,
    /// Optional, UNTRUSTED screen context — what the signed-in user is currently
    /// viewing (route + a few labelled entities, optionally their org). Absent on
    /// old widgets, so `serde(default)` keeps those frames parsing. It arrives via
    /// the host page's postMessage bridge and is therefore tamperable: it is
    /// capped on ingest ([`ScreenContext::sanitized`]) and injected only as fenced
    /// background in the USER turn, never at system-prompt trust.
    ///
    /// Deserialized leniently: a present-but-malformed value (wrong type, e.g. an
    /// array) degrades to `None` rather than failing the whole frame parse and
    /// silently dropping the customer's chat message — a future host bug in the
    /// context shape must never break chat.
    #[serde(default, deserialize_with = "lenient_context")]
    pub context: Option<ScreenContext>,
    /// Artifact ids of files staged by `POST /public/upload/{agent}` for this
    /// message. Ids only — bytes never ride the socket — and each is re-checked
    /// against the caller's ownership before it is read, so a forged or
    /// borrowed id buys nothing. Lenient like the fields above: a malformed
    /// value drops the attachments, never the customer's message.
    #[serde(default, deserialize_with = "lenient_attachments")]
    pub attachments: Vec<String>,
    /// The customer's filled-in form values, present only on a `form_submit`
    /// frame. Deserialized leniently exactly like [`ClientFrame::context`]: a
    /// present-but-malformed value degrades to `None` so a tampered frame can
    /// never break the socket, and the capped, scalar values are bounded on
    /// ingest ([`FormSubmission::sanitized`]). Unlike screen context this is the
    /// customer's OWN action — trusted, the same class as their typed message —
    /// so the host injects it without the untrusted-content fence.
    #[serde(default, deserialize_with = "lenient_submission")]
    pub submission: Option<FormSubmission>,
    /// An opaque, client-minted reference for this turn, echoed back onto the
    /// assistant message the turn produces. It exists so a widget recovering a
    /// dropped turn from the transcript can identify its OWN answer instead of
    /// inferring it from position — which is wrong exactly when two questions
    /// are outstanding at once. Untrusted and never interpreted: only stored and
    /// compared. Absent from older widgets, which fall back to position.
    ///
    /// Capped on ingest like every other host-supplied field here: it is
    /// persisted onto the transcript, so an oversized or malformed value must
    /// never reach the session blob.
    #[serde(default, deserialize_with = "lenient_client_ref")]
    pub client_ref: Option<String>,
}

/// Parse an optional [`ScreenContext`], degrading a present-but-malformed value
/// to `None` (see [`ClientFrame::context`]).
/// Attachment ids, leniently: anything that is not an array of strings degrades
/// to none rather than failing the frame and losing the customer's message.
/// Parse an optional turn reference: anything that is not a non-empty string
/// within the cap degrades to `None`, and the frame still carries the customer's
/// message.
///
/// Dropped rather than truncated, deliberately. A truncated reference is stored
/// on the answer but never matches the full one the client polls with — and a
/// stamped answer suppresses the positional fallback, so that turn could never
/// be recovered by any route. Storing nothing leaves the fallback available,
/// which is exactly how a reload (whose reference died with the page) recovers.
fn lenient_client_ref<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(
        match serde::Deserialize::deserialize(deserializer).unwrap_or(serde_json::Value::Null) {
            serde_json::Value::String(s) => Some(s)
                .filter(|s| !s.trim().is_empty() && s.chars().count() <= MAX_CLIENT_REF_CHARS),
            _ => None,
        },
    )
}

fn lenient_attachments<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(
        match serde::Deserialize::deserialize(deserializer).unwrap_or(serde_json::Value::Null) {
            serde_json::Value::Array(items) => items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .take(MAX_FRAME_ATTACHMENTS)
                .collect(),
            _ => Vec::new(),
        },
    )
}

/// Cap on ids accepted from one frame, mirroring the upload endpoint's own
/// per-request cap so a crafted frame cannot ask the turn to resolve hundreds.
pub const MAX_FRAME_ATTACHMENTS: usize = 5;

/// Cap on [`ClientFrame::client_ref`]. A UUID is 36 characters; this leaves room
/// for a different scheme without letting the field become a storage channel.
const MAX_CLIENT_REF_CHARS: usize = 128;

fn lenient_context<'de, D>(deserializer: D) -> Result<Option<ScreenContext>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Only a JSON object is a context; null / array / string / number / bool (a
    // host bug) degrades to `None`. This must reject arrays explicitly — serde
    // would otherwise deserialize a struct from a sequence positionally, so `[]`
    // would wrongly become an all-default `ScreenContext`.
    match serde_json::Value::deserialize(deserializer)? {
        value @ serde_json::Value::Object(_) => Ok(serde_json::from_value(value).ok()),
        _ => Ok(None),
    }
}

/// Parse an optional [`FormSubmission`], degrading a present-but-malformed value
/// to `None` and clamping a valid one ([`FormSubmission::sanitized`]). Mirrors
/// [`lenient_context`]: only a JSON object is a submission; anything else (or a
/// value the typed shape rejects) degrades rather than failing the whole frame.
/// The server threads the result straight through, so the caps are applied here.
fn lenient_submission<'de, D>(deserializer: D) -> Result<Option<FormSubmission>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        value @ serde_json::Value::Object(_) => Ok(serde_json::from_value::<
            BTreeMap<String, SubmissionValue>,
        >(value)
        .ok()
        .map(|values| FormSubmission { values }.sanitized())),
        _ => Ok(None),
    }
}

/// A customer's form submission: the exact field values to hand the model as an
/// action's arguments. Kept small and scalar and bounded on ingest
/// ([`Self::sanitized`]). Present only on a `form_submit` frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FormSubmission {
    pub values: BTreeMap<String, SubmissionValue>,
}

/// One submitted field value: a scalar, or an array for a multi-select /
/// multi-checkbox field. `#[serde(untagged)]` maps each JSON value by its shape
/// (string → `Text`, number → `Number`, bool → `Bool`, array → `List`); a value
/// of any other shape (null, object) fails the parse, degrading the whole
/// submission to `None` — safe, since the readable summary still drives the turn.
/// The number keeps its wire form ([`serde_json::Number`]) so an integer id
/// reaches the model as `1000`, not `1000.0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubmissionValue {
    Text(String),
    Number(serde_json::Number),
    Bool(bool),
    List(Vec<String>),
}

impl FormSubmission {
    /// Max distinct fields kept.
    const MAX_FIELDS: usize = 64;
    /// Max key length (bytes); a longer key is dropped, never truncated — a
    /// mangled tool-argument name is worse than an absent one.
    const MAX_KEY_BYTES: usize = 128;
    /// Max length (bytes) of a string value or list item.
    const MAX_VALUE_BYTES: usize = 4096;
    /// Max items in a list value.
    const MAX_LIST_LEN: usize = 256;

    /// A bounded copy safe to hand the model. Defence-in-depth against a tampered
    /// iframe ballooning a turn: over-long keys are dropped, the field count is
    /// capped, and every string / list item is byte-clamped on a char boundary so
    /// a multi-byte value is never split mid-codepoint.
    pub fn sanitized(self) -> FormSubmission {
        let values = self
            .values
            .into_iter()
            .filter(|(k, _)| !k.is_empty() && k.len() <= Self::MAX_KEY_BYTES)
            .take(Self::MAX_FIELDS)
            .map(|(k, v)| (k, v.clamped()))
            .collect();
        FormSubmission { values }
    }
}

impl SubmissionValue {
    /// Clamp a string / list value to the ingest caps; scalars pass through.
    fn clamped(self) -> SubmissionValue {
        match self {
            SubmissionValue::Text(s) => {
                SubmissionValue::Text(clamp_bytes(s, FormSubmission::MAX_VALUE_BYTES))
            }
            SubmissionValue::List(items) => SubmissionValue::List(
                items
                    .into_iter()
                    .take(FormSubmission::MAX_LIST_LEN)
                    .map(|s| clamp_bytes(s, FormSubmission::MAX_VALUE_BYTES))
                    .collect(),
            ),
            scalar => scalar,
        }
    }
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 codepoint.
fn clamp_bytes(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

/// A bounded snapshot of the screen the signed-in user is viewing on the host
/// page, forwarded by the widget on a `chat` frame. Host-supplied and therefore
/// UNTRUSTED — see [`Self::sanitized`] for the ingest caps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ScreenContext {
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub org: Option<String>,
    #[serde(default)]
    pub items: Vec<ScreenItem>,
}

/// One labelled entity visible on the screen (e.g. `Selected order: Express`).
/// Both fields default so a malformed item degrades to empty (then dropped on
/// sanitize) rather than failing the whole frame's parse and losing the chat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ScreenItem {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub value: String,
}

impl ScreenContext {
    /// Max characters kept per text field (route/title/org and each item's
    /// label/value).
    const MAX_FIELD_CHARS: usize = 200;
    /// Max labelled items kept.
    const MAX_ITEMS: usize = 16;

    /// A trimmed, bounded copy safe to inject — or `None` when nothing meaningful
    /// survives. Defence-in-depth over the wire struct: the context is
    /// host-supplied and user-influenceable, so every field is trimmed and capped
    /// (≤ [`Self::MAX_FIELD_CHARS`] chars) and items are capped (≤
    /// [`Self::MAX_ITEMS`], each needing a non-empty label AND value) so it can
    /// never grow the prompt without bound. Truncation is by character, never
    /// byte, so a multi-byte value is never split mid-codepoint.
    pub fn sanitized(&self) -> Option<ScreenContext> {
        fn cap(s: &str) -> Option<String> {
            let t = s.trim();
            (!t.is_empty()).then(|| t.chars().take(ScreenContext::MAX_FIELD_CHARS).collect())
        }
        let route = self.route.as_deref().and_then(cap);
        let title = self.title.as_deref().and_then(cap);
        let org = self.org.as_deref().and_then(cap);
        let items: Vec<ScreenItem> = self
            .items
            .iter()
            .filter_map(|it| {
                Some(ScreenItem {
                    label: cap(&it.label)?,
                    value: cap(&it.value)?,
                })
            })
            .take(Self::MAX_ITEMS)
            .collect();
        if route.is_none() && title.is_none() && org.is_none() && items.is_empty() {
            None
        } else {
            Some(ScreenContext {
                route,
                title,
                org,
                items,
            })
        }
    }
}

/// A downloadable file the agent produced this turn. The widget fetches it from
/// the authenticated `/public/files/{agent}/{artifact_id}` route (building the
/// URL from its own base/agent/token) — the bytes never ride the socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub artifact_id: String,
    pub filename: String,
    pub media_type: String,
}

/// A frame to the widget. `type` is one of `connected`, `typing`, `token`,
/// `done`, `error`, `attachment`, `reconnect`, `ping` (keepalive), `progress`
/// (working label), or `resume` (accumulated answer replayed on re-attach).
#[derive(Debug, Serialize)]
pub struct ServerFrame {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The assistant message's id — set only on the `done` frame, so the widget
    /// can key a per-message feedback rating back to this exact answer. Absent
    /// on every other frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// The downloadable file — set only on the `attachment` frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentRef>,
    /// A machine-readable reason on an `error` frame the widget should act on
    /// rather than merely display (e.g. `tenant_suspended`). Absent on an
    /// ordinary turn error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
}

impl ServerFrame {
    pub fn new(kind: &'static str, content: Option<String>, session_id: &str) -> Self {
        Self {
            kind,
            content,
            session_id: Some(session_id.to_string()),
            message_id: None,
            attachment: None,
            code: None,
        }
    }

    /// An `error` frame with a reason code, for a condition that is not the
    /// turn's doing and will not clear by retrying.
    pub fn error(code: &'static str, message: String, session_id: &str) -> Self {
        Self {
            code: Some(code),
            ..Self::new("error", Some(message), session_id)
        }
    }

    /// An `attachment` frame carrying a downloadable file the agent produced.
    pub fn attachment(attachment: AttachmentRef, session_id: &str) -> Self {
        Self {
            attachment: Some(attachment),
            ..Self::new("attachment", None, session_id)
        }
    }

    /// Attach the assistant `message_id` (for the `done` frame). Chainable off
    /// [`Self::new`]; a `None` leaves the field absent from the wire.
    pub fn with_message_id(mut self, message_id: Option<String>) -> Self {
        self.message_id = message_id;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_frame_parses_type_and_content() {
        let f: ClientFrame = serde_json::from_str(r#"{"type":"chat","content":"hi"}"#).unwrap();
        assert_eq!(f.kind, "chat");
        assert_eq!(f.content.as_deref(), Some("hi"));
    }

    #[test]
    fn attachment_frame_carries_the_file_and_others_omit_it() {
        let frame = ServerFrame::attachment(
            AttachmentRef {
                artifact_id: "art_1".into(),
                filename: "export.xlsx".into(),
                media_type: "application/octet-stream".into(),
            },
            "sess-1",
        );
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "attachment");
        assert_eq!(json["attachment"]["artifact_id"], "art_1");
        assert_eq!(json["attachment"]["filename"], "export.xlsx");
        assert_eq!(json["session_id"], "sess-1");
        // Every other frame omits the attachment field entirely.
        let token =
            serde_json::to_value(ServerFrame::new("token", Some("hi".into()), "sess-1")).unwrap();
        assert!(token.get("attachment").is_none());
    }

    #[test]
    fn client_frame_parses_without_context_backcompat() {
        // An old widget sends no `context` — `serde(default)` keeps it parsing.
        let f: ClientFrame = serde_json::from_str(r#"{"type":"chat","content":"hi"}"#).unwrap();
        assert!(f.context.is_none());
    }

    #[test]
    fn client_frame_parses_with_context() {
        let f: ClientFrame = serde_json::from_str(
            r#"{"type":"chat","content":"hi","context":{"route":"/shifts/42",
                "title":"Shift detail","org":"Trust A",
                "items":[{"label":"Selected shift","value":"Night"}]}}"#,
        )
        .unwrap();
        let ctx = f.context.expect("context parsed");
        assert_eq!(ctx.route.as_deref(), Some("/shifts/42"));
        assert_eq!(ctx.title.as_deref(), Some("Shift detail"));
        assert_eq!(ctx.org.as_deref(), Some("Trust A"));
        assert_eq!(ctx.items.len(), 1);
        assert_eq!(ctx.items[0].label, "Selected shift");
        assert_eq!(ctx.items[0].value, "Night");
    }

    #[test]
    fn client_frame_with_malformed_context_keeps_the_message() {
        // A host bug sends the wrong shape (array / string / number) for `context`.
        // It must degrade to `None` and still deliver the chat message, not fail
        // the whole frame parse and silently drop the turn.
        for bad in [r#"[]"#, r#""oops""#, r#"42"#, r#"true"#] {
            let json = format!(r#"{{"type":"chat","content":"hi","context":{bad}}}"#);
            let f: ClientFrame = serde_json::from_str(&json).expect("frame still parses");
            assert_eq!(f.kind, "chat");
            assert_eq!(f.content.as_deref(), Some("hi"));
            assert!(f.context.is_none(), "malformed context degrades to None");
        }
    }

    #[test]
    fn client_frame_parses_a_form_submission() {
        let f: ClientFrame = serde_json::from_str(
            r#"{"type":"form_submit","content":"Create lead: Acme",
                "submission":{
                    "company":"Acme","value":1000,"amount":12.5,"active":true,
                    "channels":["email","phone"]}}"#,
        )
        .unwrap();
        assert_eq!(f.kind, "form_submit");
        assert_eq!(f.content.as_deref(), Some("Create lead: Acme"));
        let sub = f.submission.expect("submission parsed");
        // Each JSON shape maps to the right typed value — ids, not prose labels.
        assert_eq!(sub.values["company"], SubmissionValue::Text("Acme".into()));
        // An integer stays an integer and a decimal stays a decimal — neither the
        // parse nor the caps widen or degrade a numeric field.
        assert_eq!(
            sub.values["value"],
            SubmissionValue::Number(serde_json::Number::from(1000))
        );
        assert_eq!(
            sub.values["amount"],
            SubmissionValue::Number(serde_json::Number::from_f64(12.5).unwrap())
        );
        assert_eq!(sub.values["active"], SubmissionValue::Bool(true));
        assert_eq!(
            sub.values["channels"],
            SubmissionValue::List(vec!["email".into(), "phone".into()])
        );
    }

    #[test]
    fn client_frame_with_malformed_submission_degrades_to_none() {
        // A wrong-shape submission (or one carrying a null / object value the
        // typed shape rejects) must degrade to None and still deliver the frame —
        // the human summary in `content` drives the turn regardless.
        for bad in [
            r#"[]"#,
            r#""oops""#,
            r#"42"#,
            r#"true"#,
            r#"{"x":null}"#,
            r#"{"x":{"nested":1}}"#,
        ] {
            let json = format!(r#"{{"type":"form_submit","content":"hi","submission":{bad}}}"#);
            let f: ClientFrame = serde_json::from_str(&json).expect("frame still parses");
            assert_eq!(f.kind, "form_submit");
            assert_eq!(f.content.as_deref(), Some("hi"));
            assert!(f.submission.is_none(), "malformed submission → None: {bad}");
        }
    }

    #[test]
    fn empty_submission_object_is_an_empty_submission_not_none() {
        // `{}` is a present-but-empty submission — Some(empty), distinct from a
        // malformed value (None). It still drives the turn on the summary alone.
        let f: ClientFrame =
            serde_json::from_str(r#"{"type":"form_submit","content":"go","submission":{}}"#)
                .unwrap();
        let sub = f.submission.expect("present empty submission is Some");
        assert!(sub.values.is_empty());
    }

    #[test]
    fn form_submission_caps_clamp_oversized_input() {
        let mut values = serde_json::Map::new();
        // 80 scalar fields (sorted last) → the field count is capped to MAX_FIELDS.
        for i in 0..80 {
            values.insert(format!("z{i:03}"), serde_json::json!(i));
        }
        // An over-long key is dropped, never truncated.
        values.insert("k".repeat(200), serde_json::json!("x"));
        // A long string value is byte-clamped on a char boundary.
        values.insert("bio".into(), serde_json::json!("é".repeat(4000)));
        // A long list is length-capped and each item byte-clamped.
        values.insert(
            "tags".into(),
            serde_json::json!((0..400).map(|_| "z".repeat(5000)).collect::<Vec<_>>()),
        );
        let frame = serde_json::json!({
            "type": "form_submit", "content": "go", "submission": values
        });
        let f: ClientFrame = serde_json::from_value(frame).unwrap();
        let sub = f.submission.expect("submission parsed");

        // Field count capped; the over-long key never counts toward it.
        assert_eq!(sub.values.len(), 64);
        assert!(
            !sub.values.keys().any(|k| k.len() > 128),
            "over-long key dropped"
        );
        // `bio` and `tags` sort ahead of the `z…` fillers, so both survive the cap.
        match &sub.values["bio"] {
            SubmissionValue::Text(s) => {
                assert_eq!(s.len(), 4096, "clamped to the byte cap on a char boundary");
                assert_eq!(s.chars().count(), 2048, "no codepoint split");
            }
            other => panic!("bio should be text, got {other:?}"),
        }
        match &sub.values["tags"] {
            SubmissionValue::List(items) => {
                assert_eq!(items.len(), 256, "list length capped");
                assert!(
                    items.iter().all(|s| s.len() <= 4096),
                    "each item byte-clamped"
                );
            }
            other => panic!("tags should be a list, got {other:?}"),
        }
    }

    #[test]
    fn chat_and_token_frames_carry_no_submission() {
        let chat: ClientFrame = serde_json::from_str(r#"{"type":"chat","content":"hi"}"#).unwrap();
        assert!(chat.submission.is_none());
        let token: ClientFrame =
            serde_json::from_str(r#"{"type":"token","content":"jwt"}"#).unwrap();
        assert!(token.submission.is_none());
        // A form_submit with no submission field parses with None (no values).
        let bare: ClientFrame =
            serde_json::from_str(r#"{"type":"form_submit","content":"go"}"#).unwrap();
        assert_eq!(bare.kind, "form_submit");
        assert!(bare.submission.is_none());
    }

    #[test]
    fn screen_context_sanitize_caps_and_trims() {
        let ctx = ScreenContext {
            route: Some(format!("  /{}  ", "a".repeat(300))),
            title: Some("  Dashboard  ".into()),
            org: Some("Trust A".into()),
            items: (0..40)
                .map(|i| ScreenItem {
                    label: format!("l{i}"),
                    value: format!("v{i}"),
                })
                .chain([
                    // Dropped: empty value / empty label.
                    ScreenItem {
                        label: "no-value".into(),
                        value: "   ".into(),
                    },
                    ScreenItem {
                        label: "".into(),
                        value: "orphan".into(),
                    },
                ])
                .collect(),
        };
        let s = ctx.sanitized().expect("non-empty context survives");
        // Text fields are trimmed then capped by CHARACTER count.
        assert_eq!(s.route.as_ref().unwrap().chars().count(), 200);
        assert!(s.route.as_ref().unwrap().starts_with("/a"));
        assert_eq!(s.title.as_deref(), Some("Dashboard"));
        assert_eq!(s.org.as_deref(), Some("Trust A"));
        // Items capped to MAX_ITEMS; the two empties never counted.
        assert_eq!(s.items.len(), 16);
        assert!(s
            .items
            .iter()
            .all(|it| !it.label.is_empty() && !it.value.is_empty()));
    }

    #[test]
    fn screen_context_sanitize_none_when_empty() {
        // All-empty (blank strings + blank items) collapses to None so an absent
        // context and an empty one are indistinguishable downstream.
        let ctx = ScreenContext {
            route: Some("   ".into()),
            title: None,
            org: Some(String::new()),
            items: vec![ScreenItem {
                label: " ".into(),
                value: "".into(),
            }],
        };
        assert!(ctx.sanitized().is_none());
        assert!(ScreenContext::default().sanitized().is_none());
    }

    #[test]
    fn screen_context_sanitize_truncates_item_fields_on_char_boundary() {
        let ctx = ScreenContext {
            items: vec![ScreenItem {
                label: "é".repeat(300),
                value: "v".into(),
            }],
            ..Default::default()
        };
        let s = ctx.sanitized().expect("one item survives");
        // 200 chars, not bytes — multi-byte content is never split mid-codepoint.
        assert_eq!(s.items[0].label.chars().count(), 200);
    }

    #[test]
    fn server_frame_omits_empty_content() {
        let json = serde_json::to_string(&ServerFrame::new("done", None, "sid")).unwrap();
        assert!(!json.contains("content"));
        assert!(json.contains(r#""type":"done""#));
        assert!(json.contains(r#""session_id":"sid""#));
        // No message_id was attached, so it stays off the wire entirely.
        assert!(!json.contains("message_id"));
    }

    #[test]
    fn done_frame_carries_the_message_id_when_attached() {
        let json = serde_json::to_string(
            &ServerFrame::new("done", Some("answer".into()), "sid")
                .with_message_id(Some("msg-42".into())),
        )
        .unwrap();
        assert!(json.contains(r#""type":"done""#));
        assert!(json.contains(r#""message_id":"msg-42""#));
        // A `None` attachment leaves the field absent (parity with new()).
        let bare = serde_json::to_string(
            &ServerFrame::new("done", Some("answer".into()), "sid").with_message_id(None),
        )
        .unwrap();
        assert!(!bare.contains("message_id"));
    }

    fn chat_frame(client_ref: serde_json::Value) -> ClientFrame {
        serde_json::from_value(serde_json::json!({
            "type": "chat",
            "content": "hello",
            "client_ref": client_ref,
        }))
        .expect("a chat frame always parses")
    }

    #[test]
    fn a_turn_reference_survives_ingest_but_cannot_grow_unbounded() {
        assert_eq!(
            chat_frame(serde_json::json!("ref-a")).client_ref.as_deref(),
            Some("ref-a")
        );

        // Dropped, never truncated: a shortened reference would be stored on the
        // answer, never match the full one the client polls with, and suppress
        // the positional fallback — leaving that turn unrecoverable by any route.
        let huge = "x".repeat(MAX_CLIENT_REF_CHARS * 10);
        assert!(chat_frame(serde_json::json!(huge)).client_ref.is_none());
    }

    #[test]
    fn a_malformed_turn_reference_costs_the_recovery_not_the_message() {
        for bad in [
            serde_json::json!(null),
            serde_json::json!(42),
            serde_json::json!(["a"]),
            serde_json::json!("   "),
        ] {
            let frame = chat_frame(bad.clone());
            assert!(frame.client_ref.is_none(), "{bad} should degrade to None");
            assert_eq!(
                frame.content.as_deref(),
                Some("hello"),
                "{bad} must never cost the customer their message"
            );
        }
    }
}
