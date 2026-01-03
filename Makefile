prefix ?= $(PREFIX)

# this avoids /usr/local/usr/* when
# installing with prefix=/usr/local
ifeq ($(prefix), /usr/local)
exec_prefix ?= $(prefix)
datarootdir ?= $(prefix)/share
else
exec_prefix ?= $(prefix)/usr
datarootdir ?= $(prefix)/usr/share
endif

bindir ?= $(exec_prefix)/bin
libdir ?= $(exec_prefix)/lib
datadir ?= $(datarootdir)
mandir ?= $(datarootdir)/man

sysconfdir ?= $(prefix)/etc
localstatedir ?= $(prefix)/var

CARGO ?= cargo
CARGO_FLAGS ?= --release

GITB := $(shell command -v git 2>/dev/null)
ifdef GITB
REPO := $(shell git rev-parse --is-inside-work-tree 2>/dev/null)
endif

LIB_T := $(DESTDIR)$(localstatedir)/lib/systemd-swap
BIN_T := $(DESTDIR)$(bindir)/systemd-swap
PRE_BIN_T := $(DESTDIR)$(bindir)/pre-systemd-swap
SVC_T := $(DESTDIR)$(libdir)/systemd/system/systemd-swap.service
PRE_SVC_T := $(DESTDIR)$(libdir)/systemd/system/pre-systemd-swap.service
DFL_T := $(DESTDIR)$(datadir)/systemd-swap/swap-default.conf
CNF_T := $(DESTDIR)$(sysconfdir)/systemd/swap.conf
MAN5_T := $(DESTDIR)$(mandir)/man5/swap.conf.5
MAN8_T := $(DESTDIR)$(mandir)/man8/systemd-swap.8

.PHONY: build files dirs install uninstall clean help

default: build

build: ## Build Rust binary
	$(CARGO) build $(CARGO_FLAGS)

$(LIB_T):
	mkdir -p $@

dirs: $(LIB_T)

$(BIN_T): target/release/systemd-swap
	install -p -Dm755 $< $@

$(PRE_BIN_T): src/pre-systemd-swap
	install -p -Dm755 $< $@

$(SVC_T): include/systemd-swap.service
	install -p -Dm644 $< $@

$(PRE_SVC_T): include/pre-systemd-swap.service
	install -p -Dm644 $< $@

$(DFL_T): include/swap-default.conf
	install -p -Dm644 $< $@

$(CNF_T): swap.conf
	install -p -bDm644 -S .old $< $@

$(MAN5_T): man/swap.conf.5
	install -p -Dm644 $< $@

$(MAN8_T): man/systemd-swap.8
	install -p -Dm644 $< $@

define banner
#  This file is part of systemd-swap.\n#\n# Entries in this file show the systemd-swap defaults as\n# specified in $(datarootdir)/systemd-swap/swap-default.conf\n# You can change settings by editing this file.\n# Defaults can be restored by simply deleting this file.\n#\n# See swap.conf(5) and $(datarootdir)/systemd-swap/swap-default.conf for details.\n\n
endef

swap.conf: include/swap-default.conf ## Generate swap.conf
	@echo '** Generating swap.conf..'
	@printf "$(banner)" > $@
	@cat $< >> $@

target/release/systemd-swap: build

files: $(BIN_T) $(PRE_BIN_T) $(SVC_T) $(PRE_SVC_T) $(DFL_T) $(CNF_T) $(MAN5_T) $(MAN8_T)

install: ## Install systemd-swap
install: build dirs files

uninstall: ## Delete systemd-swap (stop systemd-swap first)
uninstall:
	test ! -f /run/systemd/swap/swap.conf
	rm -v $(BIN_T) $(PRE_BIN_T) $(SVC_T) $(PRE_SVC_T) $(DFL_T) $(CNF_T) $(MAN5_T) $(MAN8_T)
	rm -rv $(LIB_T) $(DESTDIR)$(datadir)/systemd-swap

clean: ## Remove generated files
ifdef REPO
	git clean -fxd
else
	rm -vf swap.conf *.new
	$(CARGO) clean
endif

help: ## Show help
	@grep -h "##" $(MAKEFILE_LIST) | grep -v grep | sed 's/\\$$//;s/##/\t/'
