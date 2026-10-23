APP     = Grinch.app
BUNDLE  = $(APP)/Contents
BINARY  = $(BUNDLE)/MacOS/Grinch

# Single source of truth for the version. Read from Cargo.toml and stamped
# into the bundled Info.plist at build time so CFBundleShortVersionString
# can't drift from the crate version (and therefore from the release tag).
CARGO_VERSION := $(shell awk -F'"' '/^version =/ {print $$2; exit}' Cargo.toml)

# UNIVERSAL=1 builds both aarch64 and x86_64 and fuses them with lipo so the
# resulting bundle runs natively on Apple Silicon and Intel.
UNIVERSAL ?= 0
ifeq ($(UNIVERSAL),1)
RELEASE_BIN = target/universal/release/Grinch
else
RELEASE_BIN = target/release/Grinch
endif

# Brand assets. The iconset is the source for the Finder/Dock icon; the
# template PNGs let AppKit tint the menu-bar mark for light and dark modes.
ICON_ICNS      = build/grinch.icns
ICONSET        = brand/Grinch.iconset
ICON_SOURCES   = $(wildcard $(ICONSET)/*.png)
STATUS_ICON    = brand/grinchTemplate.png
STATUS_ICON_2X = brand/grinchTemplate@2x.png

DMG       = Grinch.dmg
DMG_STAGE = build/dmg-staging

# Sparkle auto-update framework. The SHA-256 is the GitHub release asset
# digest; bump both together. The extracted bin/generate_appcast is used by
# the release workflow, so the app and the appcast tooling share one pin.
SPARKLE_VERSION   = 2.10.0
SPARKLE_SHA256    = c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c
SPARKLE_DIR       = build/sparkle-$(SPARKLE_VERSION)
SPARKLE_ARCHIVE   = $(SPARKLE_DIR)/Sparkle-$(SPARKLE_VERSION).tar.xz
SPARKLE_FRAMEWORK = $(SPARKLE_DIR)/Sparkle.framework

.PHONY: build run clean test loc icon clean-icon dmg sparkle

LSREGISTER = /System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister

# Code-signing identity. Auto-detects a Developer ID Application certificate
# in the user's keychain; falls back to ad-hoc (`-`) if none is found.
# Override explicitly: `make build MACOS_CODESIGN_IDENTITY="Developer ID Application: ..."`
#
# Why it matters for Grinch specifically: TCC (Privacy database) keys
# Accessibility grants by `(bundle_id, cdhash)` for ad-hoc apps and by
# `(bundle_id, team_id)` for Developer-ID-signed apps. Ad-hoc means every
# rebuild looks like a "new app" to TCC and the user has to re-grant
# Accessibility every time. Signing with a stable Developer ID makes the
# grant survive rebuilds.
MACOS_CODESIGN_IDENTITY ?= $(shell security find-identity -v -p codesigning 2>/dev/null | awk '/Developer ID Application/ { print $$2; exit }')

# Fold the supplied 10-resolution iconset into the bundle's .icns file.
$(ICON_ICNS): $(ICON_SOURCES)
	@mkdir -p $(dir $(ICON_ICNS))
	@iconutil -c icns $(ICONSET) -o $(ICON_ICNS)
	@echo "Built $(ICON_ICNS) from $(ICONSET)"

icon: $(ICON_ICNS)

$(SPARKLE_FRAMEWORK):
	@mkdir -p $(SPARKLE_DIR)
	@curl -fsSL -o $(SPARKLE_ARCHIVE) \
	    https://github.com/sparkle-project/Sparkle/releases/download/$(SPARKLE_VERSION)/Sparkle-$(SPARKLE_VERSION).tar.xz
	@echo "$(SPARKLE_SHA256)  $(SPARKLE_ARCHIVE)" | shasum -a 256 -c - >/dev/null || \
	    { echo "error: $(SPARKLE_ARCHIVE) does not match SPARKLE_SHA256" >&2; rm -f $(SPARKLE_ARCHIVE); exit 1; }
	@tar -xJf $(SPARKLE_ARCHIVE) -C $(SPARKLE_DIR) Sparkle.framework bin/generate_appcast
	@echo "Fetched Sparkle $(SPARKLE_VERSION)"

sparkle: $(SPARKLE_FRAMEWORK)

clean-icon:
	rm -f $(ICON_ICNS)

build: $(ICON_ICNS) $(STATUS_ICON) $(STATUS_ICON_2X) $(SPARKLE_FRAMEWORK)
ifeq ($(UNIVERSAL),1)
	cargo build --release --target aarch64-apple-darwin
	cargo build --release --target x86_64-apple-darwin
	@mkdir -p $(dir $(RELEASE_BIN))
	@lipo -create \
	    target/aarch64-apple-darwin/release/Grinch \
	    target/x86_64-apple-darwin/release/Grinch \
	    -output $(RELEASE_BIN)
else
	cargo build --release
endif
	@mkdir -p $(BUNDLE)/MacOS $(BUNDLE)/Resources
	@cp $(RELEASE_BIN) $(BINARY)
	@cp Info.plist $(BUNDLE)/Info.plist
	@/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $(CARGO_VERSION)" $(BUNDLE)/Info.plist
	@/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $(CARGO_VERSION)" $(BUNDLE)/Info.plist
	@rm -rf $(BUNDLE)/Frameworks
	@mkdir -p $(BUNDLE)/Frameworks
	@ditto $(SPARKLE_FRAMEWORK) $(BUNDLE)/Frameworks/Sparkle.framework
	@# Grinch isn't sandboxed, so Sparkle's XPC services are unused.
	@rm -rf $(BUNDLE)/Frameworks/Sparkle.framework/XPCServices \
	    $(BUNDLE)/Frameworks/Sparkle.framework/Versions/B/XPCServices
	@cp $(ICON_ICNS) $(BUNDLE)/Resources/grinch.icns
	@cp $(STATUS_ICON) $(BUNDLE)/Resources/grinchTemplate.png
	@cp $(STATUS_ICON_2X) $(BUNDLE)/Resources/
	@bash scripts/sign-app.sh "$(APP)" "$(MACOS_CODESIGN_IDENTITY)"
	@$(LSREGISTER) -f $(APP)
	@touch $(APP)

# Open the app and register with Launch Services
run: build
	@pkill -f "Grinch.app/Contents/MacOS/Grinch" 2>/dev/null || true
	@sleep 0.2
	@$(LSREGISTER) -f $(APP)
	open $(APP)
	@echo "Grinch running. Set as default browser in System Settings → Desktop & Dock → Default web browser."

# Test a URL against current config without launching a browser
test: build
	$(BINARY) --test "$(URL)"

# Package Grinch.app inside a UDZO disk image with the standard
# /Applications symlink so end users get the drag-to-install layout that
# Chrome / Edge / Firefox use. The DMG is signed with the same Developer
# ID Application cert as the .app — DMGs use Application certs, not
# Installer certs (those are .pkg-only). Signing here lets the workflow's
# notarytool step staple a Gatekeeper ticket onto the .dmg itself.
#
# `dmg` deliberately does NOT depend on `build`. The release workflow
# notarizes and staples Grinch.app between `make build UNIVERSAL=1` and
# `make dmg`; if `dmg` re-ran `build` it would (a) overwrite the universal
# binary with a single-arch one and (b) re-codesign, invalidating the
# stapled ticket. Run `make build dmg` (in that order) for a local build.
dmg:
	@if [ ! -d "$(APP)" ]; then \
	    echo "error: $(APP) not built — run 'make build' (or 'make build UNIVERSAL=1') first" >&2; \
	    exit 1; \
	fi
	@rm -rf $(DMG_STAGE) $(DMG)
	@mkdir -p $(DMG_STAGE)
	@cp -R $(APP) $(DMG_STAGE)/
	@ln -s /Applications $(DMG_STAGE)/Applications
	@hdiutil create \
	    -volname "Grinch $(CARGO_VERSION)" \
	    -srcfolder $(DMG_STAGE) \
	    -format UDZO -ov -quiet \
	    $(DMG)
	@rm -rf $(DMG_STAGE)
ifneq ($(MACOS_CODESIGN_IDENTITY),)
	@codesign --sign "$(MACOS_CODESIGN_IDENTITY)" --timestamp $(DMG)
	@echo "Built and signed $(DMG)"
else
	@echo "Built $(DMG) (unsigned — set MACOS_CODESIGN_IDENTITY for distribution)"
endif

clean:
	cargo clean
	rm -rf $(APP) $(DMG) build/

# Count source lines
loc:
	@wc -l src/*.rs | tail -1
