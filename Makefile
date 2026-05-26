# Voice Input Daemon — Build & Install
#
# Usage:
#   make build        Build the release binary
#   make install      Build and install binary + systemd service
#   make uninstall    Remove binary and systemd service
#   make run          Run in debug mode (foreground)
#   make clean        Remove build artifacts

.PHONY: build install uninstall run clean check test

BIN_NAME := daapstt
BIN_DIR := $(HOME)/.local/bin
SERVICE_DIR := $(HOME)/.config/systemd/user
CONFIG_DIR := $(HOME)/.config/voice-daemon

CARGO := cargo

build:
	$(CARGO) build --release
	@echo "Binary: target/release/$(BIN_NAME)"

check:
	$(CARGO) check

test:
	$(CARGO) test

run:
	RUST_LOG=debug $(CARGO) run

install: build
	@echo "Installing $(BIN_NAME)..."
	@mkdir -p $(BIN_DIR)
	cp target/release/$(BIN_NAME) $(BIN_DIR)/$(BIN_NAME)
	@echo "  → $(BIN_DIR)/$(BIN_NAME)"
	@mkdir -p $(SERVICE_DIR)
	cp contrib/voice-daemon.service $(SERVICE_DIR)/voice-daemon.service
	@echo "  → $(SERVICE_DIR)/voice-daemon.service"
	@mkdir -p $(CONFIG_DIR)
	@test -f $(CONFIG_DIR)/env || echo "# Groq API key — get one at https://console.groq.com/keys\nGROQ_API_KEY=" > $(CONFIG_DIR)/env
	@echo "  → $(CONFIG_DIR)/env (created if missing — fill in your API key)"
	@echo ""
	@echo "Run the following to start the daemon:"
	@echo "  systemctl --user daemon-reload"
	@echo "  systemctl --user enable --now voice-daemon"
	@echo ""
	@echo "Check logs:"
	@echo "  journalctl --user -u voice-daemon -f"

uninstall:
	@echo "Uninstalling $(BIN_NAME)..."
	-systemctl --user stop voice-daemon 2>/dev/null || true
	-systemctl --user disable voice-daemon 2>/dev/null || true
	-rm -f $(BIN_DIR)/$(BIN_NAME)
	-rm -f $(SERVICE_DIR)/voice-daemon.service
	-systemctl --user daemon-reload 2>/dev/null || true
	@echo "Done. Config preserved at $(CONFIG_DIR)/env"

clean:
	$(CARGO) clean
