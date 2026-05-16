# Build, install, and repository maintenance shortcuts for TickClaw.

CARGO ?= cargo
CARGO_HOME ?= $(HOME)/.cargo
CARGO_INSTALL_ARGS ?= --force
SYSTEMD_USER_DIR ?= $(HOME)/.config/systemd/user
SERVICE_NAME ?= tickclaw.service
SERVICE_FILE := $(SYSTEMD_USER_DIR)/$(SERVICE_NAME)

.DEFAULT_GOAL := build

.PHONY: build fmt check test clippy verify install uninstall service-status syncdoc

# Build the debug binary for local development.
build:
	$(CARGO) build

# Format Rust code.
fmt:
	$(CARGO) fmt

# Type-check Rust code.
check:
	$(CARGO) check

# Run repository tests.
test:
	$(CARGO) test

# Run clippy with warnings denied.
clippy:
	$(CARGO) clippy -- -D warnings

# Run the standard pre-commit Rust checks.
verify: fmt check test clippy

# Install with Cargo and enable the user-level systemd daemon.
install:
	$(CARGO) install --path . $(CARGO_INSTALL_ARGS)
	install -d "$(SYSTEMD_USER_DIR)"
	@install_root="$${CARGO_INSTALL_ROOT:-$${CARGO_HOME:-$(CARGO_HOME)}}"; \
	set -- $(CARGO_INSTALL_ARGS); \
	while [ "$$#" -gt 0 ]; do \
		case "$$1" in \
			--root) shift; install_root="$$1" ;; \
			--root=*) install_root="$${1#--root=}" ;; \
		esac; \
		shift || true; \
	done; \
	bin="$${install_root%/}/bin/tickclaw"; \
	printf '%s\n' \
		'[Unit]' \
		'Description=TickClaw scheduler daemon' \
		'After=network-online.target' \
		'Wants=network-online.target' \
		'' \
		'[Service]' \
		'Type=simple' \
		"ExecStart=$$bin daemon" \
		'Restart=on-failure' \
		'RestartSec=5' \
		'Environment=RUST_LOG=tickclaw=info' \
		'' \
		'[Install]' \
		'WantedBy=default.target' \
		> "$(SERVICE_FILE)"; \
	printf '%s\n' "Wrote $(SERVICE_FILE) with ExecStart=$$bin daemon"
	@if command -v loginctl >/dev/null 2>&1; then \
		loginctl enable-linger "$$USER" || printf '%s\n' 'warning: failed to enable lingering; user service may start only after login'; \
	fi
	systemctl --user daemon-reload
	systemctl --user enable --now "$(SERVICE_NAME)"
	@printf '%s\n' "Enabled $(SERVICE_NAME)"

# Stop and remove the user-level systemd daemon and installed binary.
uninstall:
	-systemctl --user disable --now "$(SERVICE_NAME)"
	@install_root="$${CARGO_INSTALL_ROOT:-$${CARGO_HOME:-$(CARGO_HOME)}}"; \
	set -- $(CARGO_INSTALL_ARGS); \
	while [ "$$#" -gt 0 ]; do \
		case "$$1" in \
			--root) shift; install_root="$$1" ;; \
			--root=*) install_root="$${1#--root=}" ;; \
		esac; \
		shift || true; \
	done; \
	rm -f "$(SERVICE_FILE)" "$${install_root%/}/bin/tickclaw"
	-systemctl --user daemon-reload

# Show the installed user service status.
service-status:
	systemctl --user status "$(SERVICE_NAME)"

# Commit and push the shared agent development guide.
syncdoc:
	git add AGENTS.md
	git commit -m "sync"
	git push
