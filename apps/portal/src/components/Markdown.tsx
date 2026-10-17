import { Fragment, type ReactNode } from "react";

// The small Markdown subset the assistant writes: paragraphs, "-"/"1." lists,
// **bold**, `code` and [links](https://...). Rendered as React elements, never
// as HTML, so an answer can't inject markup. Links open in the same tab when
// they point into this cloud.

const INLINE = /\*\*([^*]+)\*\*|`([^`]+)`|\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)|(https?:\/\/[^\s<>)]+)/g;

function inline(text: string): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(INLINE)) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const key = m.index;
    if (m[1]) out.push(<strong key={key}>{m[1]}</strong>);
    else if (m[2]) out.push(<code key={key}>{m[2]}</code>);
    else out.push(<A key={key} href={m[4] ?? m[5]}>{m[3] ?? m[5]}</A>);
    last = m.index + m[0].length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

function A({ href, children }: { href: string; children: ReactNode }) {
  let url: URL;
  try {
    url = new URL(href);
  } catch {
    return <>{children}</>;
  }
  // INLINE only matches http(s) links; checked again so no other scheme can become a link.
  if (url.protocol !== "https:" && url.protocol !== "http:") return <>{children}</>;
  href = url.href;
  const internal = typeof window !== "undefined" && url.origin === window.location.origin;
  return internal ? <a href={href}>{children}</a> : <a href={href} target="_blank" rel="noopener noreferrer">{children}</a>;
}

export function Markdown({ text }: { text: string }) {
  const blocks = text.trim().split(/\n{2,}/);
  return (
    <>
      {blocks.map((block, i) => {
        const lines = block.split("\n");
        const bullet = lines.every((l) => /^\s*[-*•]\s+/.test(l));
        const numbered = lines.every((l) => /^\s*\d+[.)]\s+/.test(l));
        if (bullet || numbered) {
          const items = lines.map((l, j) => <li key={j}>{inline(l.replace(/^\s*([-*•]|\d+[.)])\s+/, ""))}</li>);
          return numbered ? <ol key={i}>{items}</ol> : <ul key={i}>{items}</ul>;
        }
        const heading = /^#{1,4}\s+(.*)$/.exec(block);
        if (heading && lines.length === 1) return <p key={i}><strong>{inline(heading[1])}</strong></p>;
        return (
          <p key={i}>
            {lines.map((l, j) => (
              <Fragment key={j}>
                {j > 0 && <br />}
                {inline(l)}
              </Fragment>
            ))}
          </p>
        );
      })}
    </>
  );
}
