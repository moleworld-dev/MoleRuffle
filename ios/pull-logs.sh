#!/bin/bash
# 把手机上 App"文稿/logs"里的日志拷回电脑(App 运行时会同时写日志文件,不插线也能录)。
# 用法:ios/pull-logs.sh [设备标识]   → target/ios-logs/,并对最新一份做汇总(同 perf-console.sh --summary)
set -euo pipefail
cd "$(dirname "$0")/.."
for x in /Applications/Xcode-beta.app /Applications/Xcode.app; do
  [[ -d "$x/Contents/Developer" ]] && { export DEVELOPER_DIR="$x/Contents/Developer"; break; }
done
DEV="${1:-$(xcrun devicectl list devices 2>/dev/null | awk '/physical/ && ($0 ~ / connected / || $0 ~ /available \(paired\)/) {for (i=1;i<=NF;i++) if ($i ~ /^[0-9A-F-]{36}$/) {print $i; exit}}')}"
[[ -n "$DEV" ]] || { echo "找不到已连接的 iPhone(解锁手机、连上数据线后再试)"; exit 1; }
OUT=target/ios-logs; mkdir -p "$OUT"
xcrun devicectl device copy from --device "$DEV" \
  --domain-type appDataContainer --domain-identifier com.moleworld.moleruffle \
  --source Documents/logs --destination "$OUT" >/dev/null
latest=$(ls -t "$OUT"/moleruffle-*.log 2>/dev/null | head -1)
[[ -n "$latest" ]] || { echo "手机上没有日志文件"; exit 1; }
ls -la "$OUT"/moleruffle-*.log | awk '{print $5, $9}'
echo "== 最新:$latest"
ios/perf-console.sh --summary "$latest"
