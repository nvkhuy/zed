.DEFAULT_GOAL := help

APP_NAME := Zed
BUNDLE_ID := dev.zed.Zed
BUNDLE_DIR := target/bundle/$(APP_NAME).app
INSTALL_DIR := /Applications/$(APP_NAME).app
CLI_LINK := /opt/homebrew/bin/zed
ARGS ?=

.PHONY: help run run-fast run-release open check check-sidebar build build-release \
        bundle install test fmt fmt-check clippy clean clean-debug size

help: ## Show available commands
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make <target> [ARGS=...]\n\nTargets:\n"} \
		/^[a-zA-Z0-9_-]+:.*?##/ { printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@echo ""

run: ## Build and run Zed (debug profile)
	cargo run -p zed --bin zed -- $(ARGS)

run-fast: ## Build and run Zed (release-fast profile: optimized, fast link)
	cargo run --profile release-fast -p zed --bin zed -- $(ARGS)

run-release: ## Build and run Zed (full release profile)
	cargo run --release -p zed --bin zed -- $(ARGS)

open: ## Launch already-built debug binary directly (no cargo check)
	./target/debug/zed $(ARGS)

check: ## Fast type/compile check for the zed binary
	cargo check -p zed --bin zed

check-sidebar: ## Fast compile check for sidebar + workspace crates
	cargo check -p sidebar -p workspace

build: ## Build debug binary
	cargo build -p zed --bin zed

build-release: ## Build release zed + cli binaries
	cargo build --release -p zed -p cli --bin zed --bin cli

bundle: build-release ## Package a standalone macOS .app bundle in target/bundle/
	@rm -rf "$(BUNDLE_DIR)"
	@mkdir -p "$(BUNDLE_DIR)/Contents/MacOS" "$(BUNDLE_DIR)/Contents/Resources"
	@cp target/release/zed "$(BUNDLE_DIR)/Contents/MacOS/zed"
	@cp target/release/cli "$(BUNDLE_DIR)/Contents/MacOS/cli"
	@cp crates/zed/resources/Document.icns "$(BUNDLE_DIR)/Contents/Resources/Document.icns"
	@rm -rf target/bundle/AppIcon.iconset && mkdir -p target/bundle/AppIcon.iconset
	@sips -z 16 16     crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_16x16.png >/dev/null
	@sips -z 32 32     crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_16x16@2x.png >/dev/null
	@sips -z 32 32     crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_32x32.png >/dev/null
	@sips -z 64 64     crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_32x32@2x.png >/dev/null
	@sips -z 128 128   crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_128x128.png >/dev/null
	@sips -z 256 256   crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_128x128@2x.png >/dev/null
	@sips -z 256 256   crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_256x256.png >/dev/null
	@sips -z 512 512   crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_256x256@2x.png >/dev/null
	@sips -z 512 512   crates/zed/resources/app-icon@2x.png --out target/bundle/AppIcon.iconset/icon_512x512.png >/dev/null
	@cp crates/zed/resources/app-icon@2x.png target/bundle/AppIcon.iconset/icon_512x512@2x.png
	@iconutil -c icns target/bundle/AppIcon.iconset -o "$(BUNDLE_DIR)/Contents/Resources/Zed.icns"
	@rm -rf target/bundle/AppIcon.iconset
	@printf '%s\n' \
		'<?xml version="1.0" encoding="UTF-8"?>' \
		'<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
		'<plist version="1.0"><dict>' \
		'  <key>CFBundleDevelopmentRegion</key><string>en</string>' \
		'  <key>CFBundleDisplayName</key><string>$(APP_NAME)</string>' \
		'  <key>CFBundleExecutable</key><string>zed</string>' \
		'  <key>CFBundleIconFile</key><string>Zed.icns</string>' \
		'  <key>CFBundleIdentifier</key><string>$(BUNDLE_ID)</string>' \
		'  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>' \
		'  <key>CFBundleName</key><string>$(APP_NAME)</string>' \
		'  <key>CFBundlePackageType</key><string>APPL</string>' \
		'  <key>CFBundleShortVersionString</key><string>1.0.0</string>' \
		'  <key>CFBundleVersion</key><string>1</string>' \
		'  <key>LSMinimumSystemVersion</key><string>12.0</string>' \
		'  <key>NSHighResolutionCapable</key><true/>' \
		'  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>' \
		'  <key>CFBundleURLTypes</key><array><dict>' \
		'    <key>CFBundleURLName</key><string>Zed URL</string>' \
		'    <key>CFBundleURLSchemes</key><array><string>zed</string><string>zed-cli</string></array>' \
		'  </dict></array>' > "$(BUNDLE_DIR)/Contents/Info.plist"
	@cat crates/zed/resources/info/DocumentTypes.plist >> "$(BUNDLE_DIR)/Contents/Info.plist"
	@cat crates/zed/resources/info/Permissions.plist >> "$(BUNDLE_DIR)/Contents/Info.plist"
	@printf '%s\n' '</dict></plist>' >> "$(BUNDLE_DIR)/Contents/Info.plist"
	@plutil -lint "$(BUNDLE_DIR)/Contents/Info.plist" >/dev/null
	@codesign --force --deep --sign - "$(BUNDLE_DIR)"
	@echo "Bundled: $(BUNDLE_DIR)"

install: bundle ## Build, install /Applications/Zed.app, and link zed CLI
	@rm -rf "$(INSTALL_DIR)"
	@cp -R "$(BUNDLE_DIR)" "$(INSTALL_DIR)"
	@ln -sf "$(INSTALL_DIR)/Contents/MacOS/cli" "$(CLI_LINK)"
	@touch "$(INSTALL_DIR)"
	@echo "Installed: $(INSTALL_DIR)"
	@echo "CLI linked: $(CLI_LINK) -> $(INSTALL_DIR)/Contents/MacOS/cli"
	@echo "If Zed is currently open, quit (Cmd+Q) and reopen it to use the new build."

test: ## Run sidebar unit tests
	cargo test -p sidebar

fmt: ## Format Rust source files
	cargo fmt --all

fmt-check: ## Check Rust formatting without modifying files
	cargo fmt --all -- --check

clippy: ## Run clippy on zed and sidebar
	cargo clippy -p zed --bin zed -p sidebar

clean: ## Clean entire target/ directory
	cargo clean

clean-debug: ## Remove only target/debug to save disk space
	rm -rf target/debug

size: ## Show repo, crates, target, and free disk space
	@du -sh . .git crates target 2>/dev/null || true
	@df -h / | tail -n 1
