//! `export_xlsx` for the edge: build a spreadsheet from the agent's tabular
//! output and deliver it to the widget as an authenticated download.
//!
//! The channel `ExportXlsxTool` reads a `CURRENT_AGENT_ID` task-local and the
//! process-global store; the edge sets neither, so this tool takes the agent,
//! the caller (owner), and the store as constructor arguments — mirroring the
//! other edge built-ins. It reuses the shared, task-local-free
//! [`build_xlsx_from_input`] core, stamps the artifact with the owner so the
//! download route can scope reads, and emits a [`CustomerEvent::Attachment`]
//! referencing the staged file — the bytes never ride the socket or re-enter
//! the LLM context.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;

use workforce_agent::artifacts::{ArtifactBody, ArtifactDescriptor, ArtifactStore, ContentType};
use workforce_agent::export::XLSX_MIME;
use workforce_agent::tools::export_xlsx::{
    build_xlsx_from_input, export_xlsx_input_schema, EXPORT_XLSX_DESCRIPTION, EXPORT_XLSX_TOOL_NAME,
};
use workforce_agent::tools::{Tool, ToolResult};

use crate::host::CustomerEvent;
use crate::wire::AttachmentRef;

/// Turn-scoped sink of the files this turn produced, so the turn can persist
/// them onto the assistant message after the run (the socket frame is live-only;
/// this is what lets a download button survive a widget reload).
pub type AttachmentSink = Arc<std::sync::Mutex<Vec<AttachmentRef>>>;

/// The edge's `export_xlsx` built-in — one per turn, carrying that turn's agent,
/// owner, store handle, and customer event channel.
pub struct EdgeExportXlsxTool {
    store: Arc<dyn ArtifactStore>,
    events: Sender<CustomerEvent>,
    agent_id: String,
    owner: String,
    ttl: Duration,
    collected: AttachmentSink,
}

impl EdgeExportXlsxTool {
    pub fn new(
        store: Arc<dyn ArtifactStore>,
        events: Sender<CustomerEvent>,
        agent_id: String,
        owner: String,
        ttl: Duration,
        collected: AttachmentSink,
    ) -> Self {
        Self {
            store,
            events,
            agent_id,
            owner,
            ttl,
            collected,
        }
    }
}

#[async_trait]
impl Tool for EdgeExportXlsxTool {
    fn name(&self) -> &str {
        EXPORT_XLSX_TOOL_NAME
    }

    fn description(&self) -> &str {
        EXPORT_XLSX_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        export_xlsx_input_schema()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;

        let (bytes, filename, total_rows, sheet_count) =
            match build_xlsx_from_input(&input, self.store.as_ref(), &self.agent_id) {
                Ok(v) => v,
                Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
            };

        // Owner-stamped so the download route scopes reads to this customer.
        // The host sets the turn's user/session task-locals, so `from_context`
        // already carries them; we still set `user_id` explicitly so the
        // download gate holds regardless of ambient scope.
        let descriptor = ArtifactDescriptor {
            row_count: Some(total_rows),
            user_id: Some(self.owner.clone()),
            ttl_seconds: self.ttl.as_secs(),
            ..ArtifactDescriptor::from_context(
                EXPORT_XLSX_TOOL_NAME,
                ContentType::OctetStream,
                self.agent_id.clone(),
            )
            .with_file_meta(filename.clone(), XLSX_MIME)
        };
        let handle = match self
            .store
            .write(&self.agent_id, descriptor, ArtifactBody::Bytes(bytes))
        {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage the spreadsheet for delivery: {e}"),
                    elapsed(start),
                ))
            }
        };

        let file = AttachmentRef {
            artifact_id: handle.id.as_str().to_string(),
            filename: filename.clone(),
            media_type: XLSX_MIME.to_string(),
        };
        // Record it so the turn can persist it onto the assistant message (the
        // download button then survives a widget reload). A poisoned lock just
        // means this file won't be rehydrated — never fail the export for it.
        if let Ok(mut sink) = self.collected.lock() {
            sink.push(file.clone());
        }
        // Reference only — the widget downloads the bytes from the authenticated
        // route. Best-effort: a disconnected widget simply abandons the turn.
        let _ = self.events.send(CustomerEvent::Attachment { file }).await;

        let output = json!({
            "rows": total_rows,
            "sheets": sheet_count,
            "filename": filename,
            "note": "Spreadsheet generated and delivered to the user as a downloadable .xlsx — this \
                     happens outside your context, so you will NOT see the file. That is expected, \
                     NOT a failure: do not try to read, re-upload, or verify it. Just tell the user \
                     the file is ready.",
        });
        Ok(ToolResult::success(output, elapsed(start)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::Receiver;
    use workforce_agent::artifacts::{ArtifactId, ArtifactStore, FsArtifactStore};

    const OWNER: &str = "ext:issuer\u{1f}user-1";

    fn tool(
        store: Arc<dyn ArtifactStore>,
    ) -> (EdgeExportXlsxTool, Receiver<CustomerEvent>, AttachmentSink) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let collected = AttachmentSink::default();
        let tool = EdgeExportXlsxTool::new(
            store,
            tx,
            "agent-a".to_string(),
            OWNER.to_string(),
            Duration::from_secs(3600),
            collected.clone(),
        );
        (tool, rx, collected)
    }

    #[tokio::test]
    async fn export_stages_owner_stamped_xlsx_and_emits_attachment() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn ArtifactStore> = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let (tool, mut rx, collected) = tool(store.clone());

        let result = tool
            .execute(
                json!({ "data": [{ "Name": "Ann", "Salary": 100 }], "filename": "staff.xlsx" }),
            )
            .await
            .unwrap();
        assert!(result.success);

        let CustomerEvent::Attachment { file } =
            rx.try_recv().expect("an attachment event is streamed")
        else {
            panic!("expected an attachment event");
        };
        assert_eq!(file.filename, "staff.xlsx");
        assert_eq!(file.media_type, XLSX_MIME);

        // The same reference is recorded for persistence onto the assistant message.
        let saved = collected.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].artifact_id, file.artifact_id);

        let id = ArtifactId::parse(&file.artifact_id).unwrap();
        let descriptor = store.metadata("agent-a", &id).unwrap().unwrap();
        assert_eq!(descriptor.user_id.as_deref(), Some(OWNER), "owner-stamped");
        assert_eq!(descriptor.filename.as_deref(), Some("staff.xlsx"));
        assert_eq!(descriptor.media_type.as_deref(), Some(XLSX_MIME));
        let bytes = store.read("agent-a", &id).unwrap().into_bytes().unwrap();
        assert_eq!(&bytes[0..2], b"PK", "xlsx is a zip");
    }

    #[tokio::test]
    async fn export_with_no_rows_fails_and_stages_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn ArtifactStore> = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let (tool, mut rx, collected) = tool(store);

        let result = tool.execute(json!({ "data": [] })).await.unwrap();
        assert!(!result.success);
        assert!(
            rx.try_recv().is_err(),
            "no attachment is emitted on failure"
        );
        assert!(
            collected.lock().unwrap().is_empty(),
            "nothing is recorded for persistence on failure"
        );
    }
}
