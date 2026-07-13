// Run with: node markdown.test.js
const assert = require("assert");
const { renderMarkdown } = require("./markdown.js");

// HTML is escaped — script injection from model output is neutralized
assert.strictEqual(
  renderMarkdown('<script>alert("x")</script>'),
  '<p>&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;</p>'
);

// fenced code blocks render verbatim inside pre/code, no inline styling
const code = renderMarkdown("before\n\n```rust\nlet x = 1; // **not bold**\n```\n\nafter");
assert.ok(code.includes("<pre><code>let x = 1; // **not bold**</code></pre>"), code);
assert.ok(code.includes("<p>before</p>"));
assert.ok(code.includes("<p>after</p>"));

// inline code, bold, italic
assert.strictEqual(renderMarkdown("use `cargo build` now"), "<p>use <code>cargo build</code> now</p>");
assert.strictEqual(renderMarkdown("**bold** and *slanted*"), "<p><strong>bold</strong> and <em>slanted</em></p>");

// single newlines become <br>, blank lines split paragraphs
assert.strictEqual(renderMarkdown("a\nb\n\nc"), "<p>a<br>b</p><p>c</p>");

// bold/italic markers inside inline code stay literal
assert.strictEqual(renderMarkdown("`**raw**`"), "<p><code>**raw**</code></p>");

// the code-span placeholder must not eat ordinary numbers
assert.strictEqual(renderMarkdown("there are 0 problems"), "<p>there are 0 problems</p>");

console.log("markdown.test.js: all assertions passed");
