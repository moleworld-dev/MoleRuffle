#!/bin/zsh
# 桌面版性能对照:同一个二进制,用不同环境变量各跑一轮登录页,对比 [perf] 行的 render 平均耗时。
#
# 用法:
#   desktop/perf-ab.sh [-p release|profiling] [-r 常驻MB] [-w 稳定秒] [-t 采样秒] [-s] <标签=环境变量...> ...
# 例:
#   desktop/perf-ab.sh "基线=MOLE_METAL_SINGLE_CB=0" "单缓冲=MOLE_METAL_SINGLE_CB=1"
#   desktop/perf-ab.sh -p profiling -s "基线=A=0" "新=A=1 B=1"      # -s:同时用 sample 采主线程
#
# 每组:启动 → 等登录页就绪(日志出现"里程碑:游戏报告首页素材加载成功",即游戏自己的统计打点;
# 不带这行日志的旧二进制退回看 [perf] 行"常驻"位图达到 -r 阈值,默认 18MB —— 惰性形状打开后
# 只解码显示到的位图,常驻只有 6MB 左右,不能再用它判断。网络慢时固定等待不可靠;45 秒没加载完就
# 重启,最多 4 次)→ 再稳定 -w 秒 → 采样窗口 → 杀进程。只启动不点击,停在登录页(稳态 24fps)。
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
  # 官方服网络时好时坏(主 SWF 偶尔几十秒不回甚至解析失败):45 秒没加载完就杀掉重来,最多 4 次。
  ready=0
  for attempt in 1 2 3 4; do
    env MOLE_SERVER="${MOLE_SERVER:-official}" ${=envs} "$BIN" >"$out/log.txt" 2>&1 &
    pid=$!
    for _ in {1..22}; do
      sleep 2
      kill -0 $pid 2>/dev/null || break
      grep -aq "里程碑:游戏报告首页素材加载成功" "$out/log.txt" && { ready=1; break; }
      mb=$(grep -a "\[perf\]" "$out/log.txt" | tail -1 | sed -nE 's/.*常驻 ([0-9]+)MB.*/\1/p')
      [[ -n $mb && $mb -ge $READY_MB ]] && { ready=1; break; }
    done
    [[ $ready == 1 ]] && break
    kill $pid 2>/dev/null; sleep 1; kill -9 $pid 2>/dev/null; sleep 1
  done
  if [[ $ready == 0 ]]; then echo "[$tag] 重试 4 次登录页都没加载完,跳过"; continue; fi
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
avg, peak, fps, gp = [], [], [], []
kinds = dirty = memo = ""
for l in sys.stdin:
    m = re.search(r"\[perf\] FPS\s+(\d+).*?render 均\s*([\d.]+)/峰\s*([\d.]+)ms", l)
    if m:
        fps.append(float(m[1])); avg.append(float(m[2])); peak.append(float(m[3]))
    g = re.search(r"每帧 通道\s*([\d.]+)\((.*?)\) 拷贝段\s*([\d.]+) 绘制\s*([\d.]+)", l)
    if g:
        gp.append((float(g[1]), float(g[3]), float(g[4]))); kinds = g[2]
    d = re.search(r"脏矩形 (.*?) \| 烘焙 (.*?) \| ", l)
    if d:
        dirty = d[1]; memo = d[2]
if not avg:
    print(f"[{tag}] 采样窗口内没有 [perf] 行"); sys.exit()
print(f"[{tag}] {len(avg)} 个窗口 | FPS 中位 {st.median(fps):.0f} | render 均值: 中位 {st.median(avg):.2f}ms 最小 {min(avg):.2f} 最大 {max(avg):.2f} | 单帧峰值中位 {st.median(peak):.2f}ms"
      + (f" | 每帧 通道 {st.median(x[0] for x in gp):.1f} 拷贝段 {st.median(x[1] for x in gp):.1f} 绘制 {st.median(x[2] for x in gp):.1f} | 末窗来源 {kinds} | 末窗脏矩形 {dirty} | 末窗烘焙 {memo}" if gp else ""))
' "$tag"
  errs=$(grep -ciE "wgpu.*(error|validation)|panicked" "$out/log.txt")
  [[ $errs != 0 ]] && { echo "[$tag] ⚠ 日志里有 $errs 行 wgpu 报错/崩溃:"; grep -iE "wgpu.*(error|validation)|panicked" "$out/log.txt" | head -5; }
done
