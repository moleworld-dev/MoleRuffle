#!/bin/bash
# 把 MoleRuffle 打包成 .app 并装到 iOS 模拟器运行(开发自验,无需签名)。
# 用法:ios/run-sim.sh [模拟器名,默认 "iPhone 17 Pro"]
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

SIM="${1:-iPhone 17 Pro}"
TARGET="aarch64-apple-ios-sim"
# release 构建:Ruffle 是 AVM 解释器,debug 比 release 慢一个数量级,帧率差异巨大,默认走 release。
BIN="target/$TARGET/release/moleruffle"
APP="target/$TARGET/MoleRuffle.app"
BUNDLE_ID="com.moleworld.moleruffle"

echo "== 1. 构建 ($TARGET, release) =="
cargo +stable build --release --target "$TARGET" -p moleruffle-desktop

echo "== 2. 组装 .app =="
rm -rf "$APP"; mkdir -p "$APP"
cp "$BIN" "$APP/moleruffle"
cp ios/Info.plist "$APP/Info.plist"
# ★ 关键:把 LaunchScreen.storyboard 用 ibtool 编进包(Info.plist 里 UILaunchStoryboardName 指它)。
#   没有它,iOS 找不到有效 launch screen → 进"缩放兼容模式" → view.contentScaleFactor 被降级
#   → winit inner_size/scale 拿到偏小逻辑分辨率被系统放大 → 画面模糊/分辨率错(就是之前的病根)。
#   真机路径(xcodebuild)会自动编译 storyboard,这里手工拼 .app 必须补上,两条路才一致。
ibtool --errors --warnings --notices --output-format human-readable-text \
  --target-device iphone --target-device ipad --minimum-deployment-target 15.0 \
  --compile "$APP/LaunchScreen.storyboardc" ios/LaunchScreen.storyboard

echo "== 3. 启动模拟器 '$SIM' =="
xcrun simctl boot "$SIM" 2>/dev/null || true
open -a Simulator
sleep 3

echo "== 4. 安装 + 启动 =="
xcrun simctl install booted "$APP"
xcrun simctl launch --console-pty booted "$BUNDLE_ID"
