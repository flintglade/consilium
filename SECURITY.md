# Security

Report vulnerabilities privately through [GitHub private vulnerability reporting](https://github.com/flintglade/consilium/security/advisories/new) or support@flintglade.com. Include the affected version, steps to reproduce, and a synthetic example; omit credentials and private conversations.

Use the latest tested revision. The desktop renderer treats model output as untrusted text, provider HTTP clients refuse redirects, and Chat and Agent routes have separate capability checks. Agent tools run with the chosen vendor CLI permissions and can modify local files; inspect the selected provider and mode before sending a request.

Configuration may select executables and credential recipients. Consilium reads only the per-user configuration file or an explicit CONSILIUM_ENV_FILE; do not select files from untrusted projects. Existing process variables take precedence.

The Antigravity Gemini route passes prompts in process arguments because its supported CLI has no private prompt transport. Other local users or monitoring tools may read them. The route stays unavailable unless CONSILIUM_ALLOW_VISIBLE_PROMPTS=1 explicitly opts in. Use the direct Google API connector for confidential Gemini text. Session files and local CLI credential caches remain sensitive even when the application is closed.

Dependency audits run on changes and weekly. GTK3/Tauri currently retains transitive maintenance warnings and the upstream GLib VariantStrIter unsoundness advisory; this application does not call that iterator directly. These warnings are retained, not suppressed. Upstream GUI dependency migration needs compatible Tauri support.
