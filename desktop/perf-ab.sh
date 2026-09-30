#!/bin/zsh
# 桌面版性能对照:同一个二进制,用不同环境变量各跑一轮登录页,对比 [perf] 行的 render 平均耗时。
#
# 用法:
#   desktop/perf-ab.sh [-p release|profiling] [-r 常驻MB] [-w 稳定秒] [-t 采样秒] [-s] <标签=环境变量...> ...
# 例:
#   desktop/perf-ab.sh "基线=MOLE_METAL_SINGLE_CB=0" "单缓冲=MOLE_METAL_SINGLE_CB=1"
#   desktop/perf-ab.sh -p profiling -s "基线=A=0" "新=A=1 B=1"      # -s:同时用 sample 采主线程
#
# 每组:启动 → 等 [perf] 行的"常驻"位图达到 -r 阈值(默认 18MB = 登录页全部加载完;网络慢时固定等待
# 不可靠,最多等 120 秒)→ 再稳定 -w 秒 → 采样窗口 → 杀进程。只启动不点击,停在登录页(稳态 24fps)。
# 结果写到 target/perf-ab/<标签>/{log.txt,sample.txt},并打印每组 render 均值的中位数。
set -u
cd "$(dirname "$0")/.."
PROFILE=release; WARM=6; SECS=20; DO_SAMPLE=0; READY_MB=18
while getopts "p:w:t:r:s" o; do
  case $o in
    p) PROFILE=$OPTARG ;; r) READY_MB=$OPTARG ;; w) WARM=$OPTARG ;; t) SECS=$OPTARG ;; s) DO_SAMPLE=1 ;;
  esac
done
shift $((OPTIND - 1))
BIN="target/$PROFILE/moleruffle"
[[ -x $BIN ]] || { echo "没有 $BIN,先 cargo build"; exit 1; }
[[ $# -ge 1 ]] || { echo "至少给一组 标签=环境变量"; exit 1; }

for spec in "$@"; do
  tag="${spec%%=*}"; envs="${spec#*=}"
  out="target/perf-ab/$tag"; mkdir -p "$out"
  if pgrep -f "target/$PROFILE/moleruffle" >/dev/null; then
    echo "已有 $BIN 实例在跑,跳过 $tag"; continue
  fi
  env MOLE_SERVER="${MOLE_SERVER:-official}" ${=envs} "$BIN" >"$out/log.txt" 2>&1 &
  pid=$!
  ready=0
  for _ in {1..60}; do
    sleep 2
    kill -0 $pid 2>/dev/null || break
    mb=$(grep -a "\[perf\]" "$out/log.txt" | tail -1 | sed -nE 's/.*常驻 ([0-9]+)MB.*/\1/p')
    [[ -n $mb && $mb -ge $READY_MB ]] && { ready=1; break; }
  done
  if ! kill -0 $pid 2>/dev/null; then echo "[$tag] 进程已退出"; tail -5 "$out/log.txt"; continue; fi
  if [[ $ready == 0 ]]; then echo "[$tag] 120 秒内没加载到常驻 ${READY_MB}MB,跳过"; kill $pid; continue; fi
  sleep "$WARM"
  start_line=$(wc -l <"$out/log.txt")
  if [[ $DO_SAMPLE == 1 ]]; then
    sample $pid "$SECS" 1 -f "$out/sample.txt" >/dev/null 2>&1
  else
    sleep "$SECS"
  fi
  kill $pid 2>/dev/null; sleep 1; kill -9 $pid 2>/dev/null
  tail -n +$((start_line + 1)) "$out/log.txt" | python3 -c '
import re, sys, statistics as st
tag = sys.argv[1]
avg, peak, fps = [], [], []
for l in sys.stdin:
    m = re.search(r"\[perf\] FPS\s+(\d+).*?render 均\s*([\d.]+)/峰\s*([\d.]+)ms", l)
    if m:
        fps.append(float(m[1])); avg.append(float(m[2])); peak.append(float(m[3]))
if not avg:
    print(f"[{tag}] 采样窗口内没有 [perf] 行"); sys.exit()
print(f"[{tag}] {len(avg)} 个窗口 | FPS 中位 {st.median(fps):.0f} | render 均值: 中位 {st.median(avg):.2f}ms 最小 {min(avg):.2f} 最大 {max(avg):.2f} | 单帧峰值中位 {st.median(peak):.2f}ms")
' "$tag"
  errs=$(grep -ciE "wgpu.*(error|validation)|panicked" "$out/log.txt")
  [[ $errs != 0 ]] && { echo "[$tag] ⚠ 日志里有 $errs 行 wgpu 报错/崩溃:"; grep -iE "wgpu.*(error|validation)|panicked" "$out/log.txt" | head -5; }
done
