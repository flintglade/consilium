// Minimal, safe markdown subset: fenced code blocks, inline code, bold,
// italic, paragraphs. All input is HTML-escaped first — model output is
// untrusted. UMD-ish so the same file runs in the webview and under node
// for tests.
(function (global) {
  function escapeHtml(s) {
    return s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function renderInline(s) {
    // shelter inline code from the bold/italic passes
    const codes = [];
    s = s.replace(/`([^`\n]+)`/g, (_, c) => {
      codes.push(c);
      return "\u0000" + (codes.length - 1) + "\u0000";
    });
    s = s
      .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|\W)\*([^*\n]+)\*(?=\W|$)/g, "$1<em>$2</em>");
    return s.replace(/\u0000(\d+)\u0000/g, (_, i) => "<code>" + codes[i] + "</code>");
  }

  function renderMarkdown(text) {
    const escaped = escapeHtml(text);
    const parts = escaped.split(/^```[^\n]*$/m);
    let html = "";
    for (let i = 0; i < parts.length; i++) {
      if (i % 2 === 1) {
        // inside a fence — verbatim
        html += "<pre><code>" + parts[i].replace(/^\n|\n$/g, "") + "</code></pre>";
      } else {
        const paragraphs = parts[i].split(/\n{2,}/);
        for (const p of paragraphs) {
          const trimmed = p.trim();
          if (!trimmed) continue;
          html += "<p>" + renderInline(trimmed).replace(/\n/g, "<br>") + "</p>";
        }
      }
    }
    return html;
  }

  if (typeof module !== "undefined" && module.exports) {
    module.exports = { renderMarkdown, escapeHtml };
  } else {
    global.renderMarkdown = renderMarkdown;
  }
})(this);
