import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";

// Mirrors widget-ui's renderer: wide tables scroll instead of overflowing, and
// links open in a new tab. `node` is react-markdown's AST handle — strip it so
// it isn't forwarded onto the DOM element. rehype-sanitize is the XSS guard for
// stored transcript content (customer + agent text is untrusted).
const COMPONENTS: Components = {
  table: ({ node, ...props }) => {
    void node;
    return (
      <div className="md-tablewrap">
        <table {...props} />
      </div>
    );
  },
  a: ({ node, ...props }) => {
    void node;
    return <a {...props} target="_blank" rel="noopener noreferrer" />;
  },
};

/** Render agent/markdown text as sanitized HTML, styled like the chat widget. */
export function Markdown({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeSanitize]} components={COMPONENTS}>
        {text}
      </ReactMarkdown>
    </div>
  );
}
