// Grok's headless coding agent sometimes opens with a workspace "status"
// preamble ("Checking the workspace and recent activity…") that has no place
// in a chat and, on ambiguous prompts, is the entire reply (a stall). Strip a
// leading preamble line; if nothing real remains, return a clarification.
// UMD-ish so the same file runs in the webview and under node for tests.
(function (global) {
  const PREAMBLE_RE =
    /^(checking|inspecting|picking up|continuing|auditing|verifying|scanning|reviewing|looking (at|into)|let me)\b.*\b(workspace|recent|terminal|project|in progress|working on|left off|continu|context|state)\b.*$/i;

  const FALLBACK =
    "I didn't catch a clear request there — what would you like help with?";

  function stripAgentPreamble(text) {
    const lines = String(text).split("\n");
    // drop up to 2 leading preamble/blank lines; stop at the first real line
    let start = 0;
    while (start < lines.length && start < 2) {
      const t = lines[start].trim();
      if (t === "" || PREAMBLE_RE.test(t)) {
        start++;
      } else {
        break;
      }
    }
    const cleaned = lines.slice(start).join("\n").trim();
    return cleaned || FALLBACK;
  }

  if (typeof module !== "undefined" && module.exports) {
    module.exports = { stripAgentPreamble, PREAMBLE_RE, FALLBACK };
  } else {
    global.stripAgentPreamble = stripAgentPreamble;
  }
})(this);
