#!/bin/bash
# 在手机上启动 MoleRuffle,并把它的日志实时流到电脑、存成文件(需要手机已解锁;数据线最稳)。
# 用户在手机上正常玩,这边就能拿到游戏内各场景的 [perf] 行(通道数/来源/脏矩形/烘焙记忆/足迹)。
#
# 用法:ios/perf-console.sh [设备标识] [输出文件]
#   设备标识默认取第一台已配对的实体 iPhone;输出默认 target/ios-console/<时间>.log
#   结束:在手机上退出 App,或在这边 Ctrl-C。
# 之后汇总:ios/perf-console.sh --summary <日志文件>
set -euo pipefail
cd "$(dirname "$0")/.."

summary() {
  python3 - "$1" <<'PY'
import re, sys, statistics as st
rows = []
for line in open(sys.argv[1], errors="replace"):
    m = re.search(r"\[perf\] FPS\s+(\d+) \| render 均\s*([\d.]+)/峰\s*([\d.]+)ms \| 每帧 通道\s*([\d.]+)\((.*?)\) 拷贝段\s*([\d.]+) 绘制\s*([\d.]+) \| 脏矩形 (.*?) \| 烘焙 (.*?)(?: \||$)", line)
    if m:
        rows.append(m.groups())
if not rows:
    sys.exit("日志里没有 [perf] 行")
print(f"共 {len(rows)} 个统计窗口(约每 2 秒一个)")
for name, idx in (("render 均值 ms", 1), ("单帧峰值 ms", 2), ("每帧通道", 3), ("每帧绘制", 6)):
    vals = [float(r[idx]) for r in rows]
    print(f"  {name:<14} 中位 {st.median(vals):7.2f}  P90 {sorted(vals)[int(len(vals)*0.9)]:7.2f}  最大 {max(vals):7.2f}")
worst = sorted(rows, key=lambda r: -float(r[3]))[:5]
print("通道最多的 5 个窗口:")
for r in worst:
    print(f"  通道 {r[3]}({r[4]}) render {r[1]}/{r[2]}ms | 脏矩形 {r[7]} | 烘焙 {r[8]}")
PY
}

if [[ "${1:-}" == "--summary" ]]; then
  summary "${2:?用法: ios/perf-console.sh --summary <日志文件>}"
  exit 0
fi

# 新系统(iOS 27.x 测试版)需要配套的 Xcode 测试版里的 devicectl 才能连上;构建仍用正式版(见 ios/_env.sh)。
for x in /Applications/Xcode-beta.app /Applications/Xcode.app; do
  [[ -x "$x/Contents/Developer/usr/bin/devicectl" || -d "$x/Contents/Developer" ]] && { export DEVELOPER_DIR="$x/Contents/Developer"; break; }
done

DEV="${1:-$(xcrun devicectl list devices 2>/dev/null | awk '/available \(paired\)/ && /physical/ {for (i=1;i<=NF;i++) if ($i ~ /^[0-9A-F-]{36}$/) {print $i; exit}}')}"
[[ -n "$DEV" ]] || { echo "找不到已配对且在线的 iPhone(解锁手机、连上数据线后再试)"; exit 1; }
OUT="${2:-target/ios-console/$(date +%Y%m%d-%H%M%S).log}"
mkdir -p "$(dirname "$OUT")"
echo "设备 $DEV → 日志 $OUT(手机上退出 App 或 Ctrl-C 结束)"
xcrun devicectl device process launch --console --terminate-existing --device "$DEV" com.moleworld.moleruffle 2>&1 | tee "$OUT"
echo
summary "$OUT" || true
