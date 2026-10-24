//! Best-effort text extraction from uploaded training files.
//!
//! Operators upload documents (FAQs, policies, product PDFs) in the admin UI;
//! this turns each into plain text for [`crate::knowledge`] to chunk + embed.
//! Extraction is defensive — a malformed or unsupported file yields a clear
//! error for that file rather than failing the whole upload — and CPU-bound
//! parsers (PDF) are meant to run inside `spawn_blocking` so a slow/huge file
//! can't stall the async runtime (a panic there is isolated to its task).

use std::io::{Cursor, Read};

use quick_xml::events::Event;
use quick_xml::Reader;
use workforce_common::text::html_to_text;

/// Formats we can turn into text, for the UI to advertise + validate against.
pub const SUPPORTED: &[&str] = &[
    "txt", "text", "md", "markdown", "csv", "tsv", "json", "log", "html", "htm", "pdf", "docx",
];

/// Whether a filename's extension is one we can extract.
pub fn is_supported(filename: &str) -> bool {
    SUPPORTED.contains(&ext(filename).as_str())
}

fn ext(filename: &str) -> String {
    filename
        .rsplit('.')
        .next()
        .filter(|e| *e != filename) // no dot ⇒ no extension
        .unwrap_or("")
        .to_lowercase()
}

/// Extract plain text from an uploaded file's bytes, chosen by extension. Errors
/// carry a human message suitable for showing the operator.
pub fn extract(filename: &str, bytes: &[u8]) -> Result<String, String> {
    let text = match ext(filename).as_str() {
        "txt" | "text" | "md" | "markdown" | "csv" | "tsv" | "json" | "log" => utf8(bytes),
        "html" | "htm" => html_to_text(&utf8(bytes)),
        "pdf" => extract_pdf(bytes)?,
        "docx" => extract_docx(bytes)?,
        "" => return Err("file has no extension".into()),
        other => return Err(format!("unsupported file type .{other}")),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("no extractable text (empty, scanned, or image-only)".into());
    }
    Ok(trimmed.to_string())
}

/// Lossy UTF-8 — invalid bytes become U+FFFD rather than failing the upload.
fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Extract a PDF's text. `pdf-extract` can be slow and occasionally panics on
/// exotic files; the caller runs this in `spawn_blocking`, where a panic is
/// isolated. Returns a friendly error instead of the library's raw message.
fn extract_pdf(bytes: &[u8]) -> Result<String, String> {
    pdf_extract::extract_text_from_mem(bytes)
        .map_err(|_| "couldn't read this PDF (encrypted or malformed?)".to_string())
}

const DOCX_DOC_PART: &str = "word/document.xml";
/// Cap on the DECOMPRESSED document.xml. The 32 MiB request-body limit only
/// bounds the *compressed* upload; a zip can inflate 1000:1, so read at most this
/// much of the entry to defeat a decompression bomb (OOM DoS).
const MAX_DOCX_XML: u64 = 64 * 1024 * 1024;

/// Extract a .docx's text: unzip `word/document.xml` and concatenate the run
/// text (`<w:t>`), inserting a blank line at each paragraph (`</w:p>`) so the
/// knowledge chunker can split on paragraph boundaries.
fn extract_docx(bytes: &[u8]) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| "not a valid .docx (bad zip)".to_string())?;
    let entry = archive
        .by_name(DOCX_DOC_PART)
        .map_err(|_| "not a Word document (missing document.xml)".to_string())?;
    // Cap the decompressed read (zip-bomb defence), then lossily decode.
    let mut buf = Vec::new();
    std::io::Read::take(entry, MAX_DOCX_XML)
        .read_to_end(&mut buf)
        .map_err(|_| "couldn't read the document body".to_string())?;
    let xml = String::from_utf8_lossy(&buf);

    let mut reader = Reader::from_str(&xml);
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == b"w:t" => in_text = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"w:t" => in_text = false,
            Ok(Event::End(e)) if e.name().as_ref() == b"w:p" => out.push_str("\n\n"),
            Ok(Event::Text(t)) if in_text => {
                out.push_str(&t.unescape().unwrap_or_default());
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err("malformed .docx XML".into()),
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_and_support() {
        assert!(is_supported("policy.PDF"));
        assert!(is_supported("faq.md"));
        assert!(!is_supported("archive.zip"));
        assert!(!is_supported("noext"));
    }

    #[test]
    fn plain_text_round_trips_and_blanks_error() {
        assert_eq!(extract("a.txt", b"hello world").unwrap(), "hello world");
        assert!(extract("a.txt", b"   ").is_err(), "blank ⇒ error");
        assert!(extract("a.rando", b"x").is_err(), "unsupported ⇒ error");
    }

    #[test]
    fn html_is_stripped() {
        let h = "<html><style>.x{color:red}</style><body><h1>Hi</h1><p>a&amp;b</p><script>bad()</script></body></html>";
        let t = extract("p.html", h.as_bytes()).unwrap();
        assert!(t.contains("Hi") && t.contains("a&b"));
        assert!(
            !t.contains("color:red") && !t.contains("bad()"),
            "script/style dropped"
        );
    }

    #[test]
    fn html_keeps_text_after_script_and_survives_multibyte() {
        // Text after a </script> must NOT be swallowed.
        let t = extract("p.html", b"<script>x()</script>keep-me").unwrap();
        assert!(t.contains("keep-me"), "text after script survives");
        assert!(!t.contains("x()"));
        // A multibyte char before a tag must not panic or corrupt (İ lowercases
        // to a different byte length — the old byte-index-into-lowercased bug).
        let t = extract("p.html", "İ<b>bold</b> café".as_bytes()).unwrap();
        assert!(t.contains("bold") && t.contains("café") && t.contains('İ'));
    }

    #[test]
    fn docx_round_trips() {
        // Build a minimal .docx (a zip with word/document.xml) and extract it.
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            zip.start_file::<_, ()>("word/document.xml", zip::write::FileOptions::default())
                .unwrap();
            zip.write_all(
                br#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body>
                    <w:p><w:r><w:t>Hello</w:t><w:t xml:space="preserve"> world</w:t></w:r></w:p>
                    <w:p><w:r><w:t>Second para</w:t></w:r></w:p>
                </w:body></w:document>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        let text = extract("doc.docx", &buf).unwrap();
        assert!(text.contains("Hello world"), "runs joined: {text:?}");
        assert!(text.contains("Second para"));
    }
}
