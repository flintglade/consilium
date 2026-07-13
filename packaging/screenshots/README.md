# Store screenshots

The release owner must add real Linux screenshots here before an app-store
submission. Use a clean temporary Consilium data directory and test accounts;
never capture API keys, private prompts, session titles, usernames, or desktop
backgrounds.

Required capture for the initial listing:

- `consilium-main.png`: the unmaximized main window at no more than 1000x700,
  with the default Linux window decorations and a representative synthetic
  conversation;
- `consilium-routing.png`: the routing explanation with synthetic provider
  state; and
- `consilium-recovery.png`: the local backup/recovery state using synthetic
  session data.

Do not crop, annotate, frame, or add promotional text. After a release tag is
public, host the images at immutable tag- or commit-based HTTPS URLs and add
those URLs plus one-sentence captions to the AppStream `<screenshots>` block.
Run `packaging/check-distribution.sh --flathub-ready` afterward.
