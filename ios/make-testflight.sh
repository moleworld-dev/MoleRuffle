#!/bin/bash
# 打 TestFlight 发布签名 IPA 并(可选)上传。
# 发布签名走 Apple Distribution 证书 + App Store 描述文件(用 ASC API key 自动生成/下载)。
# 注入预编译 Rust release 二进制(同 build-device.sh),archive→exportArchive 产出 IPA。
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

KEY="$HOME/.appstoreconnect/private_keys/AuthKey_6N5DAM7RXC.p8"
KEYID="6N5DAM7RXC"
ISSUER="0f1cb134-9497-45fb-959c-09fb3a7cf633"
TEAM="JV2TTWR28G"
DD="/tmp/mole_tf"
AUTH=(-authenticationKeyPath "$KEY" -authenticationKeyID "$KEYID" -authenticationKeyIssuerID "$ISSUER")

echo "== 1. 编译真机 release 二进制 =="
cargo +stable build --release --target aarch64-apple-ios -p moleruffle-desktop

echo "== 2. 生成 Xcode 工程 =="
( cd ios && xcodegen generate )

echo "== 3. archive(Release,分发签名,API key 自动配描述文件)=="
rm -rf "$DD"
xcodebuild -project ios/MoleRuffle.xcodeproj -scheme MoleRuffle \
  -configuration Release -destination 'generic/platform=iOS' \
  -archivePath "$DD/MoleRuffle.xcarchive" \
  -allowProvisioningUpdates "${AUTH[@]}" \
  archive

echo "== 4. 导出 App Store IPA =="
cat > "$DD/ExportOptions.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>method</key><string>app-store-connect</string>
  <key>teamID</key><string>$TEAM</string>
  <key>uploadSymbols</key><false/>
  <key>destination</key><string>export</string>
</dict></plist>
PLIST
xcodebuild -exportArchive -archivePath "$DD/MoleRuffle.xcarchive" \
  -exportOptionsPlist "$DD/ExportOptions.plist" \
  -exportPath "$DD/export" \
  -allowProvisioningUpdates "${AUTH[@]}"

echo "== 完成 =="
ls -la "$DD/export/"*.ipa
