# QuicKVM — quickvm 主控端的選單列外殼。Swift 殼 + 嵌入的 Rust helper，包成 .app 並 codesign。
# 無 Xcode：SwiftPM build executable + 手刻 bundle。完整流程見 ../README 與 kilo-sense 參考實作。
APP_NAME   := QuicKVM
SWIFT_BIN  := .build/release/$(APP_NAME)
RUST_BIN   := ../target/release/quickvm
APP_BUNDLE := build/$(APP_NAME).app
CONTENTS   := $(APP_BUNDLE)/Contents
VERSION    := $(shell /usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" Resources/Info.plist)
DMG        := build/$(APP_NAME)-$(VERSION).dmg

# 開發態預設 ad-hoc；Apple Development cert hash 放 Makefile.local（gitignored）覆蓋以保 TCC 跨 rebuild。
SIGN_ID ?= -
# 分發用 Developer ID Application cert + notarytool keychain profile，都放 Makefile.local。
DEV_ID_APP ?=
NOTARY_PROFILE ?= quickvm-notary
-include Makefile.local

.PHONY: all rust swift build bundle run rebuild install logs release publish clean

all: bundle

# Rust helper（主控端核心）：從 workspace root 編 release，只 build quickvm bin。
rust:
	cargo build --release --manifest-path ../Cargo.toml --bin quickvm

# Swift 選單列殼。
swift:
	swift build -c release

build: rust swift

# 組 .app：殼進 MacOS/、helper 進 Helpers/（不同目錄 —— APFS case-insensitive 下 quickvm 會跟 QuicKVM 撞）。
# inside-out 簽：先簽 helper 再簽 bundle。
bundle: build
	@rm -rf $(APP_BUNDLE)
	@mkdir -p $(CONTENTS)/MacOS $(CONTENTS)/Helpers $(CONTENTS)/Resources
	@cp $(SWIFT_BIN) $(CONTENTS)/MacOS/$(APP_NAME)
	@cp $(RUST_BIN) $(CONTENTS)/Helpers/quickvm
	@cp Resources/Info.plist $(CONTENTS)/Info.plist
	@cp Resources/AppIcon.icns $(CONTENTS)/Resources/AppIcon.icns
	@cp Resources/BoltMark.png $(CONTENTS)/Resources/BoltMark.png
	@codesign --force --options runtime --sign $(SIGN_ID) $(CONTENTS)/Helpers/quickvm
	@codesign --force --options runtime --entitlements Resources/QuicKVM.entitlements --sign $(SIGN_ID) $(APP_BUNDLE)
	@codesign --verify --strict --verbose=2 $(APP_BUNDLE)
	@echo "[OK] $(APP_BUNDLE) signed with $(SIGN_ID)"

run: bundle
	open $(APP_BUNDLE)

rebuild: clean bundle

# 安裝到 /Applications（開機自啟與穩定 TCC 都需要 app 在這裡）。
install: bundle
	@rm -rf /Applications/$(APP_NAME).app
	@cp -R $(APP_BUNDLE) /Applications/
	@echo "[OK] installed to /Applications/$(APP_NAME).app"

logs:
	@tail -f "$$HOME/Library/Logs/QuicKVM.log"

# 完整分發：Developer ID 簽 + notarize + staple。前置同 kilo-sense（store-credentials + Makefile.local 設 DEV_ID_APP）。
release: build
	@test -n "$(DEV_ID_APP)" || { echo "✗ 需在 Makefile.local 設 DEV_ID_APP"; exit 1; }
	@rm -rf $(APP_BUNDLE)
	@mkdir -p $(CONTENTS)/MacOS $(CONTENTS)/Helpers $(CONTENTS)/Resources
	@cp $(SWIFT_BIN) $(CONTENTS)/MacOS/$(APP_NAME)
	@cp $(RUST_BIN) $(CONTENTS)/Helpers/quickvm
	@cp Resources/Info.plist $(CONTENTS)/Info.plist
	@cp Resources/AppIcon.icns $(CONTENTS)/Resources/AppIcon.icns
	@cp Resources/BoltMark.png $(CONTENTS)/Resources/BoltMark.png
	@codesign --force --options runtime --timestamp --sign "$(DEV_ID_APP)" $(CONTENTS)/Helpers/quickvm
	@codesign --force --options runtime --timestamp --entitlements Resources/QuicKVM.entitlements --sign "$(DEV_ID_APP)" $(APP_BUNDLE)
	@codesign --verify --strict --verbose=2 $(APP_BUNDLE)
	@ditto -c -k --keepParent $(APP_BUNDLE) build/$(APP_NAME)-notarize.zip
	@xcrun notarytool submit build/$(APP_NAME)-notarize.zip --keychain-profile "$(NOTARY_PROFILE)" --wait
	@xcrun stapler staple $(APP_BUNDLE)
	@rm -f build/$(APP_NAME)-notarize.zip
	@echo "[OK] notarized + stapled：$(APP_BUNDLE)"

clean:
	rm -rf .build build
