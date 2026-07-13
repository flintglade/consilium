"use strict";

const assert = require("node:assert/strict");
const {
  snapshotRuntime,
  canSubmit,
  controlsDisabled,
  providerSessionForRestore,
  supportsImplicitModelDefault,
  normalizeImplicitModel,
  implicitDefaultModelMetadata,
  explicitStatusModel,
  agentModeForRoutingProfile,
  routingProfileForAgentMode,
  savedModelMatchesStatus,
  sanitizeAttachmentMetadata,
  normalizeStoredMessage,
  persistableMessage,
  hasRenderableAssistantOutput,
} = require("./request-state.js");

const mutable = {
  provider: "claude",
  model: "sonnet",
  effort: "high",
  agent: true,
  routingProfile: "balanced",
};
const snapshot = snapshotRuntime(mutable);
mutable.provider = "codex";
mutable.agent = false;
assert.deepEqual(snapshot, {
  provider: "claude",
  model: "sonnet",
  effort: "high",
  agent: true,
  routingProfile: "balanced",
});
assert.equal(Object.isFrozen(snapshot), true);

assert.equal(canSubmit({ busy: true, text: "do work", attachmentCount: 0 }), false);
assert.equal(canSubmit({ busy: false, text: "  ", attachmentCount: 1 }), true);
assert.equal(canSubmit({ busy: false, text: "hello", attachmentCount: 0 }), true);
assert.equal(canSubmit({ busy: false, text: "  ", attachmentCount: 0 }), false);

assert.deepEqual(controlsDisabled(false, true, "manual"), {
  provider: false,
  model: false,
  effort: false,
  agent: false,
  routing: false,
  session: false,
  attachment: false,
});
assert.equal(controlsDisabled(false, false, "balanced").provider, true);
assert.equal(controlsDisabled(true, true, "manual").effort, true);

assert.equal(
  providerSessionForRestore("legacy-grok-session", null, "grok"),
  "legacy-grok-session"
);
assert.equal(providerSessionForRestore("legacy-grok-session", null, "claude"), null);
assert.equal(
  providerSessionForRestore(
    "native-claude-session",
    { provider: "claude" },
    "claude"
  ),
  "native-claude-session"
);
assert.equal(
  providerSessionForRestore("native-claude-session", { provider: "claude" }, "grok"),
  null
);

assert.equal(supportsImplicitModelDefault("grok"), true);
assert.equal(supportsImplicitModelDefault("openai-compatible"), false);
assert.equal(normalizeImplicitModel("default", "claude"), null);
assert.equal(normalizeImplicitModel("antigravity-default", "gemini"), null);
assert.equal(normalizeImplicitModel("gemini-cli-default", "gemini"), null);
assert.equal(normalizeImplicitModel("grok-4.5", "grok"), "grok-4.5");
assert.equal(normalizeImplicitModel("default", "openai-compatible"), "default");
const codexDefaultMetadata = {
  id: "default",
  supports_effort: true,
  efforts: [{ id: "high" }],
};
assert.equal(
  implicitDefaultModelMetadata([codexDefaultMetadata], "codex", "grok-4.5"),
  codexDefaultMetadata
);
assert.equal(
  implicitDefaultModelMetadata([{ id: "grok-4.5" }], "grok", "grok-4.5").id,
  "grok-4.5"
);
assert.equal(implicitDefaultModelMetadata([{ id: "model" }], "openai-compatible"), null);
assert.equal(explicitStatusModel("Codex default", "codex"), null);
assert.equal(explicitStatusModel("Gemini via Antigravity", "gemini"), null);
assert.equal(explicitStatusModel("default (grok-4.5)", "grok"), null);
assert.equal(explicitStatusModel("gpt-5.3-codex", "codex"), "gpt-5.3-codex");
assert.equal(explicitStatusModel("claude-sonnet-4-6", "claude"), "claude-sonnet-4-6");
assert.equal(agentModeForRoutingProfile("agent-tools", false), true);
assert.equal(agentModeForRoutingProfile("balanced", false), false);
assert.equal(agentModeForRoutingProfile("balanced", true), true);
assert.equal(routingProfileForAgentMode("agent-tools", false), "balanced");
assert.equal(routingProfileForAgentMode("agent-tools", true), "agent-tools");
assert.equal(routingProfileForAgentMode("privacy", false), "privacy");
assert.equal(savedModelMatchesStatus(null, "default (grok-4.5)", "grok"), true);
assert.equal(savedModelMatchesStatus("default", "Claude Code default", "claude"), true);
assert.equal(
  savedModelMatchesStatus("antigravity-default", "Gemini via Antigravity", "gemini"),
  true
);
assert.equal(savedModelMatchesStatus(null, "local/model", "openai-compatible"), false);
assert.equal(savedModelMatchesStatus("local/model", "local/model", "openai-compatible"), true);
assert.equal(savedModelMatchesStatus("old/model", "new/model", "openai-compatible"), false);
assert.equal(savedModelMatchesStatus(null, "gpt-5.3-codex", "codex"), false);
assert.equal(savedModelMatchesStatus("gpt-5.3-codex", "gpt-5.3-codex", "codex"), true);

const safeMetadata = sanitizeAttachmentMetadata([
  {
    kind: "image",
    name: "diagram.png",
    mime: "image/png",
    bytes: 1234,
    data: "base64 payload must not survive",
  },
  {
    kind: "text",
    name: "notes.txt",
    mime: "text/plain",
    bytes: 42,
    text: "file body must not survive",
  },
]);
assert.deepEqual(safeMetadata, [
  { kind: "image", name: "diagram.png", mime: "image/png", size_bytes: 1234 },
  { kind: "text", name: "notes.txt", mime: "text/plain", size_bytes: 42 },
]);
assert.equal(JSON.stringify(safeMetadata).includes("payload"), false);
assert.equal(JSON.stringify(safeMetadata).includes("file body"), false);

const prepared = persistableMessage({
  role: "user",
  content: "question\n\n[Attached file: notes.txt]\n```\nprivate body\n```",
  displayContent: "question",
  attachments: [{ kind: "text", name: "notes.txt", text: "private body", bytes: 42 }],
});
assert.deepEqual(prepared, {
  role: "user",
  content: "question",
  attachments: [{ kind: "text", name: "notes.txt", mime: null, size_bytes: 42 }],
});
assert.equal(JSON.stringify(prepared).includes("private body"), false);

assert.deepEqual(
  normalizeStoredMessage({
    role: "user",
    content: "legacy question\n\n[Attached file: old.txt]\n```\nlegacy body\n```",
  }),
  {
    role: "user",
    content: "legacy question",
    attachments: [{ kind: "text", name: "old.txt", mime: null, size_bytes: 0 }],
  }
);
assert.equal(hasRenderableAssistantOutput(""), false);
assert.equal(hasRenderableAssistantOutput("answer"), true);

assert.deepEqual(
  snapshotRuntime({
    provider: "gemini",
    model: "antigravity-default",
    effort: "default",
    agent: true,
    routingProfile: "manual",
  }),
  {
    provider: "gemini",
    model: null,
    effort: null,
    agent: true,
    routingProfile: "manual",
  }
);

console.log("request-state tests: ok");
