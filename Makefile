# system-builder Makefile
#
#   make debug            build dev binaries (target/debug/gb-server, gb-client)
#   make release          build optimized binaries (target/release/...)
#   make install-server   install gb-server + seed config + systemd unit if missing,
#                         then enable/start (restarts if already running)
#   make install-client   install gb-client CLI only
#   make package          release binary tarball in dist/ for GitHub
#                         releases (see scripts/package.sh)
#
# Run make as your normal user -- never `sudo make`. Cargo builds always run
# unprivileged; only the install steps escalate via $(SUDO).
#
# Variables:
#   PREFIX=/usr/local   binary install prefix
#   DESTDIR=            stage into DIR instead of / (no sudo, skips systemctl)
#   ENABLE=yes          set ENABLE=no to install files without enabling/starting
#   SUDO=sudo           privilege prefix for live installs (auto-empty when
#                       staging or already root; SUDO= forces no escalation)
#   PACKAGE_ARGS=...    args for scripts/package.sh (--allow-dirty to
#                       package HEAD with uncommitted changes present,
#                       --upload to tag, push, and publish the release)

PREFIX  ?= /usr/local
DESTDIR ?=
ENABLE  ?= yes
SUDO    ?= sudo
PACKAGE_ARGS ?=

# No escalation needed when staging or already root. (Plain assignments
# below never override a SUDO passed on the command line.)
ifneq ($(strip $(DESTDIR)),)
SUDO :=
endif
ifeq ($(shell id -u),0)
SUDO :=
endif

BINDIR  := $(DESTDIR)$(PREFIX)/bin
SYSDIR  := $(DESTDIR)/etc/systemd/system
CONFDIR := $(DESTDIR)/etc/system-builder
UNIT    := $(SYSDIR)/system-builder.service

SERVICE_SRC := packaging/system-builder.service
CONFIG_SRC  := config.example.toml

.PHONY: debug release install-server install-client package clean help

debug:
	cargo build

release:
	cargo build --release

install-server: release
	@if [ -n "$(SUDO)" ] && ! command -v "$(SUDO)" >/dev/null 2>&1; then \
		echo "error: $(SUDO) not found (live install needs privilege escalation)" >&2; exit 1; \
	fi
	@echo "==> install gb-server -> $(BINDIR)/gb-server"
	$(SUDO) install -Dm755 target/release/gb-server $(BINDIR)/gb-server
	@echo "==> seed config (never overwrites)"
	@$(SUDO) mkdir -p $(CONFDIR)
	@if [ -f "$(CONFDIR)/config.toml" ]; then \
		echo "    keeping existing $(CONFDIR)/config.toml"; \
	else \
		$(SUDO) install -Dm600 $(CONFIG_SRC) $(CONFDIR)/config.toml; \
		echo "    wrote $(CONFDIR)/config.toml (edit systems/bind)"; \
	fi
	@if [ -f "$(UNIT)" ]; then \
		echo "==> systemd unit exists, keeping $(UNIT)"; \
		if [ -z "$(DESTDIR)" ] && command -v systemctl >/dev/null 2>&1; then \
			echo "==> systemctl daemon-reload + try-restart"; \
			$(SUDO) systemctl daemon-reload; \
			$(SUDO) systemctl try-restart system-builder; \
		else \
			echo "==> skipped restart (DESTDIR set or no systemctl)"; \
		fi; \
	else \
		echo "==> install systemd unit -> $(UNIT)"; \
		$(SUDO) mkdir -p $(SYSDIR); \
		sed "s|/usr/local/bin/gb-server|$(PREFIX)/bin/gb-server|" $(SERVICE_SRC) | $(SUDO) tee $(UNIT) >/dev/null; \
		$(SUDO) chmod 644 $(UNIT); \
		if [ -z "$(DESTDIR)" ] && command -v systemctl >/dev/null 2>&1 && [ "$(ENABLE)" = "yes" ]; then \
			echo "==> systemctl daemon-reload + enable --now"; \
			$(SUDO) systemctl daemon-reload; \
			$(SUDO) systemctl enable --now system-builder; \
			$(SUDO) systemctl --no-pager status system-builder || true; \
		else \
			echo "==> skipped enable (DESTDIR set, no systemctl, or ENABLE=no)"; \
			echo "    next: edit /etc/system-builder/config.toml, then:"; \
			echo "      systemctl daemon-reload && systemctl enable --now system-builder"; \
		fi; \
	fi

install-client: release
	@if [ -n "$(SUDO)" ] && ! command -v "$(SUDO)" >/dev/null 2>&1; then \
		echo "error: $(SUDO) not found (live install needs privilege escalation)" >&2; exit 1; \
	fi
	@echo "==> install gb-client -> $(BINDIR)/gb-client"
	$(SUDO) install -Dm755 target/release/gb-client $(BINDIR)/gb-client
	@echo "==> done. First run creates ~/.config/gb-client/config.toml;"
	@echo "    set the server there or export GB_SERVER, then:"
	@echo "    gb-client systems"

package:
	scripts/package.sh $(PACKAGE_ARGS)

clean:
	cargo clean

help:
	@echo "Targets: debug release install-server install-client package clean help"
	@echo "Vars:    PREFIX=$(PREFIX) DESTDIR='$(DESTDIR)' ENABLE=$(ENABLE) SUDO='$(SUDO)' PACKAGE_ARGS='$(PACKAGE_ARGS)'"
