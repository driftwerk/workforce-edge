// A `wf-image` block: pictures the agent chose to show in its reply.
//
// The artifact id in the fence is a REQUEST to render, never a grant. Every
// image goes through the same bearer-authenticated, owner-scoped file route an
// attachment uses, so an id the model invented — or lifted from somewhere it
// should not have — simply 404s and shows a "no longer available" frame. There
// is no path by which naming an id in a reply widens what the customer can see.
//
// Bearer auth is also why this cannot be `<img src={url}>`: the bytes are
// fetched with the token and shown from an object URL, revoked on unmount so a
// long conversation does not leak one blob per picture.

import { useContext, useEffect, useRef, useState } from "react";

import { EdgeApiContext } from "./apiContext";
import { fetchFile } from "../api";
import type { ImageRef, ImageSpec } from "./spec";

function OneImage({ image }: { image: ImageRef }) {
  const api = useContext(EdgeApiContext);
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const objectUrl = useRef<string | null>(null);

  const artifactId = image.artifactId;
  useEffect(() => {
    if (!api) {
      setFailed(true);
      return;
    }
    let live = true;
    fetchFile(api.base, api.agent, api.token.current ?? "", artifactId).then(
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
  }, [api, artifactId]);

  // A named file that will not load is the expected outcome of a hallucinated
  // id and of an artifact swept by TTL. Say so plainly — an empty frame reads
  // as the widget being broken.
  if (failed) {
    return (
      <figure className="wf-rimg wf-rimg-gone">
        <div className="wf-rimg-missing" role="img" aria-label="Image unavailable">
          This image is no longer available.
        </div>
        {image.caption && <figcaption>{image.caption}</figcaption>}
      </figure>
    );
  }
  if (!url) {
    return (
      <div className="wf-img-skel" role="status">
        <span className="wf-skel-label">Loading image…</span>
      </div>
    );
  }
  // `alt` first, then the caption; an empty string is the correct alt for a
  // picture with nothing to say about it, so never fall back to the id.
  const alt = image.alt ?? image.caption ?? "";
  return (
    <figure className={"wf-rimg" + (expanded ? " wf-rimg-expanded" : "")}>
      <button
        type="button"
        className="wf-img-btn"
        aria-label={expanded ? "Collapse image" : "Expand image"}
        onClick={() => setExpanded((e) => !e)}
      >
        <img src={url} alt={alt} onError={() => setFailed(true)} />
      </button>
      {image.caption && <figcaption>{image.caption}</figcaption>}
    </figure>
  );
}

export default function WfImage({ spec }: { spec: ImageSpec }) {
  return (
    <div
      className={"wf-rimgs" + (spec.images.length > 1 ? " wf-rimgs-grid" : "")}
      data-count={spec.images.length}
    >
      {spec.images.map((image) => (
        <OneImage key={image.artifactId} image={image} />
      ))}
    </div>
  );
}
