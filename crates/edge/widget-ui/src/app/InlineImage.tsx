// An image attachment rendered as a picture.
//
// The file route is bearer-authenticated, so `<img src={url}>` cannot reach it —
// the bytes are fetched with the token and shown from an object URL, which is
// revoked on unmount so a long conversation does not leak one blob per image.
//
// Every failure degrades to the download button rather than an empty frame: an
// image that will not load must never cost the customer the file itself.

import { useEffect, useRef, useState } from "react";

import type { Attachment } from "./connection";

/** Types we will render inline. A deliberately closed list — `image/*` would
 *  include SVG, which is a script container, and these bytes are agent-authored
 *  rather than operator-authored. */
const RENDERABLE = ["image/png", "image/jpeg", "image/gif", "image/webp"];

export const isInlineImage = (file: Attachment): boolean =>
  RENDERABLE.includes(file.media_type.toLowerCase().split(";")[0].trim());

export function InlineImage({
  file,
  load,
  onDownload,
  fallback,
}: {
  file: Attachment;
  /** Fetch the bytes; injected so the component stays testable without a socket. */
  load: (file: Attachment) => Promise<Blob>;
  onDownload: () => void;
  /** Rendered instead of the picture when the bytes cannot be shown. */
  fallback: React.ReactNode;
}) {
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const objectUrl = useRef<string | null>(null);

  useEffect(() => {
    let live = true;
    load(file).then(
      (blob) => {
        if (!live) return;
        objectUrl.current = URL.createObjectURL(blob);
        setUrl(objectUrl.current);
      },
      () => live && setFailed(true),
    );
    return () => {
      live = false;
      if (objectUrl.current) {
        URL.revokeObjectURL(objectUrl.current);
        objectUrl.current = null;
      }
    };
  }, [file, load]);

  if (failed) return <>{fallback}</>;
  if (!url) {
    return (
      <div className="wf-img-skel" role="status">
        <span className="wf-skel-label">Loading image…</span>
      </div>
    );
  }
  return (
    <figure className={"wf-img" + (expanded ? " wf-img-expanded" : "")}>
      <button
        type="button"
        className="wf-img-btn"
        aria-label={expanded ? `Collapse ${file.filename}` : `Expand ${file.filename}`}
        onClick={() => setExpanded((e) => !e)}
      >
        <img src={url} alt={file.filename} onError={() => setFailed(true)} />
      </button>
      <figcaption>
        <span className="wf-img-name">{file.filename}</span>
        <button type="button" className="wf-img-dl" onClick={onDownload}>
          Download
        </button>
      </figcaption>
    </figure>
  );
}
