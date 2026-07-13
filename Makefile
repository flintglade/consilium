TUI_BIN := $(CURDIR)/target/release/grok-chat
GUI_BIN := $(CURDIR)/target/release/grok-chat-desktop
ICON := $(CURDIR)/desktop/icons/consilium-appgrid.png
ICON_NAME := consilium
ICON_DEST := $(HOME)/.local/share/icons/hicolor/256x256/apps/$(ICON_NAME).png
OLD_ICONS := $(HOME)/.local/share/icons/hicolor/256x256/apps/grok-chat.png $(HOME)/.local/share/icons/hicolor/256x256/apps/grok-chat-build.png
APPS := $(HOME)/.local/share/applications
TUI_DESKTOP := $(APPS)/grok-chat.desktop
# GNOME matches a running window to its launcher by app_id/WM_CLASS, which is
# the binary name — so this filename must stay grok-chat-desktop.desktop even
# though the app is branded Consilium, or the dock shows a generic gear.
GUI_DESKTOP := $(APPS)/grok-chat-desktop.desktop
OLD_GUI_DESKTOP := $(APPS)/consilium.desktop

.PHONY: build install run run-desktop test clean

build:
	cargo build --release --workspace

test:
	cargo test --release --workspace
	cd desktop/ui && node markdown.test.js && node preamble.test.js && node request-state.test.js

install: build
	@mkdir -p $(APPS)
	@mkdir -p $(dir $(ICON_DEST))
	@rm -f $(OLD_ICONS) $(OLD_GUI_DESKTOP)
	@cp $(ICON) $(ICON_DEST)
	@if command -v alacritty >/dev/null 2>&1; then \
		EXEC="alacritty -e $(TUI_BIN)"; \
	elif command -v kitty >/dev/null 2>&1; then \
		EXEC="kitty $(TUI_BIN)"; \
	elif command -v wezterm >/dev/null 2>&1; then \
		EXEC="wezterm start -- $(TUI_BIN)"; \
	elif command -v gnome-terminal >/dev/null 2>&1; then \
		EXEC="gnome-terminal --maximize -- $(TUI_BIN)"; \
	else \
		EXEC="x-terminal-emulator -e $(TUI_BIN)"; \
	fi; \
	printf '%s\n' \
		'[Desktop Entry]' \
		'Name=Consilium (Terminal)' \
		'Comment=Terminal interface for the CONSILIUM AI harness' \
		"Exec=$$EXEC" \
		'Icon=utilities-terminal' \
		'Type=Application' \
		'Categories=Network;Chat;' \
		'Terminal=false' \
		> $(TUI_DESKTOP)
	@printf '%s\n' \
		'[Desktop Entry]' \
		'Name=Consilium' \
		'Comment=Multi-agent AI harness' \
		"Exec=$(GUI_BIN)" \
		"Icon=$(ICON_DEST)" \
		'Type=Application' \
		'Categories=Network;Chat;' \
		'StartupNotify=true' \
		'StartupWMClass=grok-chat-desktop' \
		'Terminal=false' \
		> $(GUI_DESKTOP)
	@gtk-update-icon-cache -q $(HOME)/.local/share/icons/hicolor 2>/dev/null || true
	@update-desktop-database $(APPS) 2>/dev/null || true
	@echo "Installed: $(GUI_DESKTOP) (desktop app)"
	@echo "Installed: $(TUI_DESKTOP) (terminal app)"
	@echo "Binaries: $(GUI_BIN), $(TUI_BIN)"

run:
	cargo run --release -p grok-chat

run-desktop:
	cargo run --release -p grok-chat-desktop

clean:
	cargo clean
	rm -f $(TUI_DESKTOP) $(GUI_DESKTOP) $(OLD_GUI_DESKTOP)
	rm -f $(ICON_DEST) $(OLD_ICONS)
