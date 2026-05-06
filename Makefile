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

# Icon: rendered from an emoji at build time so the binary stays self-
# contained and the icon source-of-truth lives next to the menu bar emoji
# in app_delegate.rs. Override via `make build ICON_EMOJI=…`.
ICON_EMOJI ?= 🎄
ICON_ICNS  = build/grinch.icns
ICONSET    = build/grinch.iconset

DMG       = Grinch.dmg
DMG_STAGE = build/dmg-staging

.PHONY: build run clean test loc icon clean-icon dmg

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

# Build the .icns from the emoji. Renders 10 sizes covering all macOS
# icon rendering contexts (menu bar to Finder cover-flow), then folds
# them into a single .icns via iconutil.
$(ICON_ICNS): tools/render-icon.swift
	@rm -rf $(ICONSET)
	@mkdir -p $(ICONSET)
	@swift tools/render-icon.swift $(ICON_EMOJI) 16   $(ICONSET)/icon_16x16.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 32   $(ICONSET)/icon_16x16@2x.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 32   $(ICONSET)/icon_32x32.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 64   $(ICONSET)/icon_32x32@2x.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 128  $(ICONSET)/icon_128x128.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 256  $(ICONSET)/icon_128x128@2x.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 256  $(ICONSET)/icon_256x256.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 512  $(ICONSET)/icon_256x256@2x.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 512  $(ICONSET)/icon_512x512.png
	@swift tools/render-icon.swift $(ICON_EMOJI) 1024 $(ICONSET)/icon_512x512@2x.png
	@iconutil -c icns $(ICONSET) -o $(ICON_ICNS)
	@rm -rf $(ICONSET)
	@echo "Built $(ICON_ICNS) from $(ICON_EMOJI)"

icon: $(ICON_ICNS)

clean-icon:
	rm -rf build/

build: $(ICON_ICNS)
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
	@cp $(ICON_ICNS) $(BUNDLE)/Resources/grinch.icns
ifeq ($(MACOS_CODESIGN_IDENTITY),)
	@codesign --deep --force --sign - $(APP)
	@echo "Built $(APP) (ad-hoc signed — Accessibility grant won't survive rebuilds)"
else
	@codesign --deep --force --options runtime --timestamp \
	    --entitlements Grinch.entitlements \
	    --sign "$(MACOS_CODESIGN_IDENTITY)" $(APP)
	@echo "Built $(APP) (signed with $(MACOS_CODESIGN_IDENTITY))"
endif
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
dmg: build
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
