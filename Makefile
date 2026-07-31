APP     := splaude
BUNDLE  := build/$(APP).app
CONFIG  := release
BINARY  := $(shell swift build -c $(CONFIG) --show-bin-path 2>/dev/null)/$(APP)

# TCC pins the Accessibility grant to the code signature. An ad-hoc signature
# gets a fresh cdhash on every rebuild, which reads as a different app and makes
# you re-grant each time — so prefer a real identity when one exists.
SIGN ?= $(shell security find-identity -v -p codesigning 2>/dev/null | sed -n '1s/.*"\(.*\)".*/\1/p')
SIGN := $(if $(SIGN),$(SIGN),-)

.PHONY: all build bundle install run check icon clean

all: bundle

build:
	swift build -c $(CONFIG)

# The .icns is committed, so this only needs running when the mark changes.
TINT ?= D97757

icon:
	rm -rf build/$(APP).iconset
	mkdir -p build
	swift Script/makeicon.swift $(TINT) build/$(APP).iconset
	iconutil -c icns build/$(APP).iconset -o Resource/$(APP).icns
	@echo "wrote Resource/$(APP).icns — tint #$(TINT)"

# TCC ties microphone and accessibility grants to a signed bundle identity, so
# the executable has to live inside a real .app and carry a stable signature.
bundle: build
	rm -rf $(BUNDLE)
	mkdir -p $(BUNDLE)/Contents/MacOS $(BUNDLE)/Contents/Resources
	cp Resource/Info.plist $(BUNDLE)/Contents/Info.plist
	cp Resource/$(APP).icns $(BUNDLE)/Contents/Resources/$(APP).icns
	cp $(BINARY) $(BUNDLE)/Contents/MacOS/$(APP)
	codesign --force --sign "$(SIGN)" --identifier com.bygelo.splaude $(BUNDLE)
	@echo "built $(BUNDLE) — signed with $(SIGN)"

LSREGISTER := /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

# Quit first, then replace. Swapping the bundle out from under a running copy
# leaves LaunchServices holding a stale registration, and the next `open` fails
# with -600.
install: bundle
	-pkill -x $(APP) 2>/dev/null || true
	rm -rf /Applications/$(APP).app
	cp -R $(BUNDLE) /Applications/
	$(LSREGISTER) -f /Applications/$(APP).app
	@echo "installed to /Applications/$(APP).app"

# The one command to use while iterating.
run: install
	open /Applications/$(APP).app
	@sleep 1
	@pgrep -x $(APP) >/dev/null && echo "running" || echo "FAILED to launch — check $(HOME)/Library/Logs/splaude.log"

# Credential + permission diagnostic, no UI.
check: bundle
	$(BUNDLE)/Contents/MacOS/$(APP) --check

clean:
	rm -rf build .build
