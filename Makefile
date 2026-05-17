# Build, install, and repository maintenance shortcuts for TinyButler.

CARGO ?= cargo
CARGO_HOME ?= $(HOME)/.cargo
CARGO_INSTALL_ARGS ?= --force
TINYBUTLER_HOME ?= $(HOME)/.tinybutler
TINYBUTLER_SKILLS_DIR ?= $(TINYBUTLER_HOME)/.agents/skills
TINYBUTLER_SKILL_SOURCE_DIR ?= templates/.agents/skills
SYSTEMD_USER_DIR ?= $(HOME)/.config/systemd/user
SERVICE_NAME ?= tinybutler.service
SERVICE_FILE := $(SYSTEMD_USER_DIR)/$(SERVICE_NAME)

.DEFAULT_GOAL := build

.PHONY: build fmt check test clippy verify install install-skills uninstall service-status sync syncdoc

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
	bin="$${install_root%/}/bin/tinybutler"; \
	"$$bin" --home "$(TINYBUTLER_HOME)" init; \
	if [ ! -d "$(TINYBUTLER_HOME)/.git" ]; then \
		git -C "$(TINYBUTLER_HOME)" init; \
	else \
		printf '%s\n' "TinyButler home git repository already exists at $(TINYBUTLER_HOME)/.git"; \
	fi; \
	printf '%s\n' \
		'[Unit]' \
		'Description=TinyButler scheduler daemon' \
		'After=network-online.target' \
		'Wants=network-online.target' \
		'' \
		'[Service]' \
		'Type=simple' \
		"ExecStart=$$bin --home $(TINYBUTLER_HOME) daemon" \
		'Restart=on-failure' \
		'RestartSec=5' \
		'Environment=RUST_LOG=tinybutler=info' \
		'' \
		'[Install]' \
		'WantedBy=default.target' \
		> "$(SERVICE_FILE)"; \
	printf '%s\n' "Wrote $(SERVICE_FILE) with ExecStart=$$bin --home $(TINYBUTLER_HOME) daemon"
	$(MAKE) install-skills
	@if command -v loginctl >/dev/null 2>&1; then \
		loginctl enable-linger "$$USER" || printf '%s\n' 'warning: failed to enable lingering; user service may start only after login'; \
	fi
	systemctl --user daemon-reload
	systemctl --user enable "$(SERVICE_NAME)"
	systemctl --user restart "$(SERVICE_NAME)"
	@printf '%s\n' "Enabled and restarted $(SERVICE_NAME)"

# Install project-owned Codex skills into TinyButler home repo scope.
install-skills:
	install -d "$(TINYBUTLER_SKILLS_DIR)"
	@rm -rf "$(HOME)/.codex/skills/tinybutler-operations"
	@for skill in "$(TINYBUTLER_SKILL_SOURCE_DIR)"/*; do \
		if [ -d "$$skill" ]; then \
			name="$$(basename "$$skill")"; \
			rm -rf "$(TINYBUTLER_SKILLS_DIR)/$$name"; \
			cp -R "$$skill" "$(TINYBUTLER_SKILLS_DIR)/$$name"; \
			printf '%s\n' "Installed TinyButler skill $(TINYBUTLER_SKILLS_DIR)/$$name"; \
		fi; \
	done

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
	rm -f "$(SERVICE_FILE)" "$${install_root%/}/bin/tinybutler"; \
	rm -rf "$(TINYBUTLER_SKILLS_DIR)/tinybutler-operations" "$(HOME)/.codex/skills/tinybutler-operations"
	-systemctl --user daemon-reload

# Show the installed user service status.
service-status:
	systemctl --user status "$(SERVICE_NAME)"

# Commit and push the shared agent development guide and focused design notes.
sync:
	git add AGENTS.md docs/*.md
	git commit -m "sync"
	git push

# Backward-compatible alias for the old documentation sync target.
syncdoc: sync
