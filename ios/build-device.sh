#!/bin/bash
# 构建并把 MoleRuffle 装到真机 iPhone(开发签名,自动 provisioning)。
# 用法:ios/build-device.sh <设备UDID>
#   设备 UDID 用 `xcrun devicectl list devices` 查(physical / available 那台)。
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

DEV="${1:?用法: ios/build-device.sh <设备UDID>}"
DD=/tmp/mole_dd

echo "== 1. 编译真机二进制 (aarch64-apple-ios) =="
cargo +stable build --target aarch64-apple-ios -p moleruffle-desktop

echo "== 2. 生成 Xcode 工程 =="
( cd ios && xcodegen generate )

echo "== 3. xcodebuild 自动签名 =="
xcodebuild -project ios/MoleRuffle.xcodeproj -scheme MoleRuffle \
  -configuration Debug -destination 'generic/platform=iOS' \
  -allowProvisioningUpdates -derivedDataPath "$DD" build

APP="$DD/Build/Products/Debug-iphoneos/moleruffle.app"

echo "== 4. 安装到真机 =="
xcrun devicectl device install app --device "$DEV" "$APP"

echo "== 5. 启动(需先解锁手机)=="
xcrun devicectl device process launch --device "$DEV" --terminate-existing com.moleworld.moleruffle
