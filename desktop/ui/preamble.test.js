// Run with: node preamble.test.js
const assert = require("assert");
const { stripAgentPreamble, FALLBACK } = require("./preamble.js");

// stall: the whole reply is a bare workspace preamble -> fallback
assert.strictEqual(
  stripAgentPreamble("Checking the workspace and recent activity to see what we were working on."),
  FALLBACK
);
assert.strictEqual(
  stripAgentPreamble("Inspecting the grok-chat project state and recent changes."),
  FALLBACK
);

// preamble line followed by a real answer -> keep only the answer
assert.strictEqual(
  stripAgentPreamble(
    "Checking the workspace and recent context to see what we're continuing.\nI don't have earlier context in this chat — what would you like to do?"
  ),
  "I don't have earlier context in this chat — what would you like to do?"
);

// blank line between preamble and answer is handled
assert.strictEqual(
  stripAgentPreamble("Picking up where we left off.\n\nHere is the real answer."),
  "Here is the real answer."
);

// a normal answer that merely starts with "The" is untouched
const normal = "The sky is blue because of Rayleigh scattering. Shorter wavelengths scatter more.";
assert.strictEqual(stripAgentPreamble(normal), normal);

// a legitimate answer that happens to start with "Checking" but isn't a
// workspace preamble must NOT be stripped (needs a workspace-ish keyword)
const legit = "Checking your math: 2 + 2 does indeed equal 4, so your result is correct.";
assert.strictEqual(stripAgentPreamble(legit), legit);

// code/technical answer is preserved verbatim
const code = "Here's how:\n\n```js\nconsole.log(1);\n```";
assert.strictEqual(stripAgentPreamble(code), code);

console.log("preamble.test.js: all assertions passed");
