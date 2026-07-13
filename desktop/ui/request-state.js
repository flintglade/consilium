(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.ConsiliumRequestState = api;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";

  function snapshotRuntime(runtime) {
    const provider = String(runtime.provider || "");
    return Object.freeze({
      provider,
      model: normalizeImplicitModel(runtime.model, provider),
      effort:
        runtime.effort && runtime.effort !== "default" ? String(runtime.effort) : null,
      agent: !!runtime.agent,
      routingProfile: String(runtime.routingProfile || "manual"),
    });
  }

  function canSubmit({ busy, text, attachmentCount }) {
    return !busy && (String(text || "").trim().length > 0 || Number(attachmentCount) > 0);
  }

  function controlsDisabled(busy, supportsEffort, routingProfile) {
    return {
      provider: !!busy || routingProfile !== "manual",
      model: !!busy,
      effort: !!busy || !supportsEffort,
      agent: !!busy,
      routing: !!busy,
      session: !!busy,
      attachment: !!busy,
    };
  }

  function providerSessionForRestore(sessionId, runtime, selectedProvider) {
    if (!sessionId) return null;
    // Before runtime metadata existed, this field exclusively held Grok
    // sessions. Newer records name their owning provider explicitly.
    const owner = String(runtime?.provider || "grok");
    return owner === String(selectedProvider || "") ? String(sessionId) : null;
  }

  function supportsImplicitModelDefault(providerId) {
    return ["grok", "claude", "codex", "gemini"].includes(String(providerId || ""));
  }

  function normalizeImplicitModel(modelId, providerId) {
    if (modelId === null || modelId === undefined || String(modelId).trim() === "") return null;
    const model = String(modelId).trim();
    if (!supportsImplicitModelDefault(providerId)) return model;
    if (model.toLowerCase() === "default") return null;
    if (
      String(providerId) === "gemini" &&
      ["gemini-cli-default", "antigravity-default"].includes(model)
    ) {
      return null;
    }
    return model;
  }

  function implicitDefaultModelMetadata(models, providerId, preferredModelId) {
    if (!supportsImplicitModelDefault(providerId) || !Array.isArray(models)) return null;
    return (
      models.find((model) => normalizeImplicitModel(model?.id, providerId) === null) ||
      models.find((model) => String(model?.id || "") === String(preferredModelId || "")) ||
      models[0] ||
      null
    );
  }

  function explicitStatusModel(statusModel, providerId) {
    const value = String(statusModel || "").trim();
    if (!value) return null;
    const implicitLabels = {
      claude: "Claude Code default",
      codex: "Codex default",
      gemini: "Gemini via Antigravity",
    };
    if (implicitLabels[String(providerId)] === value) return null;
    if (String(providerId) === "grok" && /^default \(.+\)$/.test(value)) return null;
    return normalizeImplicitModel(value, providerId);
  }

  function agentModeForRoutingProfile(profileId, currentAgent) {
    return String(profileId || "") === "agent-tools" ? true : !!currentAgent;
  }

  function routingProfileForAgentMode(profileId, requestedAgent) {
    return String(profileId || "") === "agent-tools" && !requestedAgent
      ? "balanced"
      : String(profileId || "manual");
  }

  function savedModelMatchesStatus(savedModel, statusModel, providerId) {
    const normalized = normalizeImplicitModel(savedModel, providerId);
    const explicitStatus = explicitStatusModel(statusModel, providerId);
    if (!normalized) return supportsImplicitModelDefault(providerId) && !explicitStatus;
    return normalized === explicitStatus;
  }

  function cleanShortText(value, maxCharacters) {
    return [...String(value || "")]
      .filter((character) => !/[\u0000-\u001f\u007f]/.test(character))
      .slice(0, maxCharacters)
      .join("");
  }

  function sanitizeAttachmentMetadata(value) {
    if (!Array.isArray(value)) return [];
    return value.slice(0, 12).flatMap((attachment) => {
      const kind = attachment?.kind === "image" ? "image" : attachment?.kind === "text" ? "text" : null;
      const name = cleanShortText(attachment?.name, 120);
      if (!kind || !name) return [];
      const rawMime = String(attachment?.mime || "");
      const mime = /^[\x20-\x7e]{1,128}$/.test(rawMime) && (kind !== "image" || rawMime.startsWith("image/"))
        ? rawMime
        : null;
      const numericSize = Number(attachment?.size_bytes ?? attachment?.bytes ?? 0);
      const size_bytes = Number.isFinite(numericSize)
        ? Math.max(0, Math.min(32 * 1024 * 1024, Math.trunc(numericSize)))
        : 0;
      return [{ kind, name, mime, size_bytes }];
    });
  }

  function legacyTextAttachments(content) {
    const value = String(content || "");
    const marker = "\n\n[Attached file: ";
    const markerIndex = value.indexOf(marker);
    if (markerIndex < 0) return { content: value, attachments: [] };
    const suffix = value.slice(markerIndex);
    const attachments = [];
    const pattern = /\[Attached file: ([^\]\n]{1,120})\]\n```/g;
    for (const match of suffix.matchAll(pattern)) {
      attachments.push({ kind: "text", name: match[1], mime: null, size_bytes: 0 });
    }
    return attachments.length
      ? { content: value.slice(0, markerIndex), attachments }
      : { content: value, attachments: [] };
  }

  function normalizeStoredMessage(message) {
    const role = String(message?.role || "");
    let content = String(message?.content || "");
    let attachments = sanitizeAttachmentMetadata(message?.attachments);
    if (role === "user" && attachments.length === 0) {
      const legacy = legacyTextAttachments(content);
      content = legacy.content;
      attachments = legacy.attachments;
    }
    return {
      role,
      content,
      ...(message?.thinking ? { thinking: String(message.thinking) } : {}),
      ...(attachments.length ? { attachments } : {}),
    };
  }

  function persistableMessage(message) {
    const normalized = normalizeStoredMessage({
      ...message,
      content:
        typeof message?.displayContent === "string"
          ? message.displayContent
          : message?.content,
    });
    return normalized;
  }

  function hasRenderableAssistantOutput(text) {
    return String(text || "").length > 0;
  }

  return {
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
  };
});
