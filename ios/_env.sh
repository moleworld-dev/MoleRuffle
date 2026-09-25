# iOS 构建公共环境(被 build-device.sh / make-testflight.sh 用 `source` 引入,不单独执行)。
#
# ★为什么必须用正式版 Xcode(SDK 26.x)★
# iOS 27 强制 app 采用 UIScene 生命周期,而 winit 0.30 没有适配。凡链接 SDK ≥ 27 的二进制,
# 在 iOS 27 上启动就会被系统终止(装得上、一点就弹回桌面、没有崩溃日志)。门槛在 27 不在 26
# (touchHLE 那边 2026-09-06 真机对照实测)。iPhone Duo 只跑 iOS 27,所以这也是它能启动的前提。
# 另外 App Store Connect 拒收 SDK < 26 的包,并且外部测试不接受 Beta 版 Xcode 打出的包
# ⇒ 唯一可行窗口是【正式版 Xcode、SDK 26.x】。
#
# 可覆盖:DEVELOPER_DIR=... 指定别的 Xcode;MOLE_ALLOW_SDK27=1 放行 SDK 27(仅供 winit 适配 UIScene 之后)。

export PATH="$HOME/.cargo/bin:$PATH"
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
if [ ! -d "$DEVELOPER_DIR" ]; then
  echo "✗ 找不到 Xcode:$DEVELOPER_DIR(需要正式版 Xcode 26.x)" >&2
  exit 1
fi

IOS_SDK_VER=$(xcrun --sdk iphoneos --show-sdk-version)
IOS_SDK_MAJOR=${IOS_SDK_VER%%.*}
if [ "$IOS_SDK_MAJOR" -ge 27 ] && [ "${MOLE_ALLOW_SDK27:-0}" != "1" ]; then
  echo "✗ 当前 Xcode 的 iOS SDK 是 $IOS_SDK_VER(≥27):编出的包在 iOS 27 上会启动即被终止。" >&2
  echo "  请用正式版 Xcode 26.x:DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer" >&2
  exit 1
fi
if [ "$IOS_SDK_MAJOR" -lt 26 ]; then
  echo "⚠ iOS SDK $IOS_SDK_VER < 26:可以侧载,但 App Store Connect 会拒收上传。" >&2
fi

# cargo 链接 iOS 二进制时必须显式给 iOS SDK,否则 clang 用 macOS sysroot → "framework 'UIKit' not found"。
export SDKROOT=$(xcrun --sdk iphoneos --show-sdk-path)
# 与 Info.plist 的 MinimumOSVersion 保持一致(rustc 默认 10.0,会写出过时的 LC_VERSION_MIN 载入命令)。
export IPHONEOS_DEPLOYMENT_TARGET=15.0

# ruffle_core 的构建脚本要调 Java 编译 AS3 的 playerglobal。系统 /usr/bin/java 只是个桩,
# Homebrew 的 JDK 默认不链接到系统路径 → "Unable to locate a Java Runtime"。自动找一个。
if ! java -version >/dev/null 2>&1; then
  for v in openjdk@21 openjdk@17 openjdk; do
    J="/opt/homebrew/opt/$v/libexec/openjdk.jdk/Contents/Home"
    if [ -x "$J/bin/java" ]; then
      export JAVA_HOME="$J"
      export PATH="$J/bin:$PATH"
      break
    fi
  done
  java -version >/dev/null 2>&1 || { echo "✗ 找不到 Java(ruffle_core 构建需要):brew install openjdk@21" >&2; exit 1; }
fi

echo "构建环境:$(xcodebuild -version | head -1)(iOS SDK $IOS_SDK_VER)| $(java -version 2>&1 | head -1)"

# 校验产物二进制声明的 SDK 版本(防止哪个环节偷偷换回了 Beta 版 Xcode)。
check_binary_sdk() {
  local bin="$1"
  local sdk
  sdk=$(xcrun vtool -show-build "$bin" 2>/dev/null | awk '/ sdk /{print $2; exit}')
  if [ -z "$sdk" ]; then
    echo "⚠ 读不到 $bin 的 SDK 版本声明,跳过校验" >&2
    return 0
  fi
  if [ "${sdk%%.*}" -ge 27 ] && [ "${MOLE_ALLOW_SDK27:-0}" != "1" ]; then
    echo "✗ $bin 声明的 SDK 是 $sdk(≥27),在 iOS 27 上会启动即被终止" >&2
    exit 1
  fi
  echo "✓ 二进制 SDK 声明 $sdk($(basename "$bin"))"
}
