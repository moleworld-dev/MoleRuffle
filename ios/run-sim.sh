#!/bin/bash
# 把 MoleRuffle 打包成 .app 并装到 iOS 模拟器运行(开发自验,无需签名)。
# 用法:ios/run-sim.sh [模拟器名,默认 "iPhone 17 Pro"]
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

SIM="${1:-iPhone 17 Pro}"
TARGET="aarch64-apple-ios-sim"
BIN="target/$TARGET/debug/moleruffle"
APP="target/$TARGET/MoleRuffle.app"
BUNDLE_ID="com.moleworld.moleruffle"

echo "== 1. 构建 ($TARGET) =="
cargo +stable build --target "$TARGET" -p moleruffle-desktop

echo "== 2. 组装 .app =="
rm -rf "$APP"; mkdir -p "$APP"
cp "$BIN" "$APP/moleruffle"
cp ios/Info.plist "$APP/Info.plist"

echo "== 3. 启动模拟器 '$SIM' =="
xcrun simctl boot "$SIM" 2>/dev/null || true
open -a Simulator
sleep 3

echo "== 4. 安装 + 启动 =="
xcrun simctl install booted "$APP"
xcrun simctl launch --console-pty booted "$BUNDLE_ID"
