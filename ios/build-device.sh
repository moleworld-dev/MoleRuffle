#!/bin/bash
# 构建并把 MoleRuffle 装到真机 iPhone(开发签名,自动 provisioning)。
# 用法:ios/build-device.sh <设备UDID>
#   设备 UDID 用 `xcrun devicectl list devices` 查(physical / available 那台)。
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

DEV="${1:?用法: ios/build-device.sh <设备UDID>}"
DD=/tmp/mole_dd

echo "== 1. 编译真机二进制 (aarch64-apple-ios, release) =="
# release:AVM 解释器 debug 慢一个数量级,真机帧率/流畅度全靠 release(project.yml 注入 release 二进制)
cargo +stable build --release --target aarch64-apple-ios -p moleruffle-desktop

echo "== 2. 生成 Xcode 工程 =="
( cd ios && xcodegen generate )

echo "== 3. xcodebuild 自动签名 =="
xcodebuild -project ios/MoleRuffle.xcodeproj -scheme MoleRuffle \
  -configuration Debug -destination 'generic/platform=iOS' \
  -allowProvisioningUpdates -derivedDataPath "$DD" build

APP="$DD/Build/Products/Debug-iphoneos/moleruffle.app"

# 强制重签名:postCompile 注入 Rust 二进制后,增量构建时 xcodebuild 可能跳过 codesign
# (stub 没变,以为已签),导致最终可执行体是未签名的注入二进制 → 装机报 "No code signature found"。
# 这里无条件用开发证书重签一次,稳妥。
echo "== 3.5 强制重签名(注入后)=="
IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null | grep "Apple Development" | head -1 | awk '{print $2}')
XCENT=$(find "$DD" -name "moleruffle.app.xcent" 2>/dev/null | head -1)
codesign --force --sign "$IDENTITY" ${XCENT:+--entitlements "$XCENT"} --generate-entitlement-der "$APP"
codesign --verify --verbose "$APP" || { echo "签名校验失败"; exit 1; }

echo "== 4. 安装到真机 =="
xcrun devicectl device install app --device "$DEV" "$APP"

echo "== 5. 启动(需先解锁手机)=="
xcrun devicectl device process launch --device "$DEV" --terminate-existing com.moleworld.moleruffle
