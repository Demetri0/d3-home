# Installation for people building from source, and the foundation every
# package here builds on: the deb, the rpm and the PKGBUILD all call
# `make install` with their own DESTDIR rather than each listing the files
# again in a slightly different way.
#
#   make
#   sudo make install                     # into /usr/local
#   make install PREFIX=$HOME/.local      # no root needed
#   make install DESTDIR=/tmp/stage       # for a package builder

PREFIX ?= /usr/local
DESTDIR ?=
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
MANDIR ?= $(DATADIR)/man
DOCDIR ?= $(DATADIR)/doc/d3home
ICONDIR ?= $(DATADIR)/icons/hicolor

CARGO ?= cargo
TARGETDIR ?= target/release
BIN := $(TARGETDIR)/d3home

COMPDIR ?= $(TARGETDIR)/completions
ICON_SIZES := 16 24 32 48 64 128 256

.PHONY: all build test completions install uninstall clean

all: build

build:
	$(CARGO) build --release --locked

test:
	$(CARGO) test --workspace --all-features

# Written by the program itself, so a completion cannot fall out of step
# with the commands that exist. Kept as a separate target because cargo-deb
# cannot run a program to make them and needs the files to already be there.
completions: $(BIN)
	mkdir -p $(COMPDIR)
	$(BIN) completions bash > $(COMPDIR)/d3home.bash
	$(BIN) completions zsh > $(COMPDIR)/_d3home
	$(BIN) completions fish > $(COMPDIR)/d3home.fish

install: $(BIN)
	install -Dm755 $(BIN) $(DESTDIR)$(BINDIR)/d3home
	install -Dm644 packaging/d3home.1 $(DESTDIR)$(MANDIR)/man1/d3home.1
	install -Dm644 packaging/d3home.desktop $(DESTDIR)$(DATADIR)/applications/d3home.desktop
	for size in $(ICON_SIZES); do \
		install -Dm644 assets/icons/hicolor/$${size}x$${size}/apps/d3home.png \
			$(DESTDIR)$(ICONDIR)/$${size}x$${size}/apps/d3home.png; \
	done
	install -Dm644 assets/icons/d3home.png $(DESTDIR)$(ICONDIR)/512x512/apps/d3home.png
	install -Dm644 README.md $(DESTDIR)$(DOCDIR)/README.md
	install -Dm644 docs/protocol.md $(DESTDIR)$(DOCDIR)/protocol.md
	install -Dm644 LICENSE-MIT $(DESTDIR)$(DOCDIR)/LICENSE-MIT
	install -Dm644 LICENSE-APACHE $(DESTDIR)$(DOCDIR)/LICENSE-APACHE
	for unit in contrib/*; do \
		install -Dm644 $$unit $(DESTDIR)$(DOCDIR)/$$unit; \
	done
# Skipped when the binary will not run on the build machine, which is what
# cross-compiling looks like from here.
	if $(BIN) --version >/dev/null 2>&1; then \
		$(MAKE) completions; \
		install -Dm644 $(COMPDIR)/d3home.bash \
			$(DESTDIR)$(DATADIR)/bash-completion/completions/d3home; \
		install -Dm644 $(COMPDIR)/_d3home \
			$(DESTDIR)$(DATADIR)/zsh/site-functions/_d3home; \
		install -Dm644 $(COMPDIR)/d3home.fish \
			$(DESTDIR)$(DATADIR)/fish/vendor_completions.d/d3home.fish; \
	else \
		echo "note: $(BIN) will not run here, so shell completions were not generated"; \
	fi

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/d3home
	rm -f $(DESTDIR)$(MANDIR)/man1/d3home.1
	rm -f $(DESTDIR)$(DATADIR)/applications/d3home.desktop
	for size in $(ICON_SIZES) 512; do \
		rm -f $(DESTDIR)$(ICONDIR)/$${size}x$${size}/apps/d3home.png; \
	done
	rm -f $(DESTDIR)$(DATADIR)/bash-completion/completions/d3home
	rm -f $(DESTDIR)$(DATADIR)/zsh/site-functions/_d3home
	rm -f $(DESTDIR)$(DATADIR)/fish/vendor_completions.d/d3home.fish
	rm -rf $(DESTDIR)$(DOCDIR)

$(BIN): build

clean:
	$(CARGO) clean
