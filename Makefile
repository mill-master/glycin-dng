PREFIX ?= $(HOME)/.local
LIBEXECDIR = $(PREFIX)/libexec
DATADIR = $(PREFIX)/share
CARGO ?= cargo

build:
	$(CARGO) build --release

install: build
	install -Dm755 target/release/glycin-dng $(DESTDIR)$(LIBEXECDIR)/glycin-dng
	install -d $(DESTDIR)$(DATADIR)/glycin-loaders/2+/conf.d
	sed 's|@LIBEXECDIR@|$(LIBEXECDIR)|' data/glycin-dng.conf.in > $(DESTDIR)$(DATADIR)/glycin-loaders/2+/conf.d/glycin-dng.conf
ifeq ($(PREFIX),/usr)
	install -Dm644 data/glycin-dng.thumbnailer $(DESTDIR)$(DATADIR)/thumbnailers/glycin-dng.thumbnailer
endif

uninstall:
	rm -f $(DESTDIR)$(LIBEXECDIR)/glycin-dng
	rm -f $(DESTDIR)$(DATADIR)/glycin-loaders/2+/conf.d/glycin-dng.conf
	rm -f $(DESTDIR)$(DATADIR)/thumbnailers/glycin-dng.thumbnailer

test:
	$(CARGO) test

.PHONY: build install uninstall test
