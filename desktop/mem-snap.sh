#!/bin/zsh
# 桌面版内存快照:启动 → 等登录页加载完(常驻位图达到阈值)→ 稳定若干秒 → 记录物理足迹与分类 → 杀进程。
# 用法: desktop/mem-snap.sh [-p release|profiling] [-r 常驻MB] [-w 稳定秒] <标签=环境变量...> ...
# 输出: target/mem-snap/<标签>/{log.txt,footprint.txt,vmmap.txt},并打印足迹与几个大类。
set -u
cd "$(dirname "$0")/.."
PROFILE=release; READY_MB=18; WARM=12
while getopts "p:r:w:" o; do
  case $o in p) PROFILE=$OPTARG ;; r) READY_MB=$OPTARG ;; w) WARM=$OPTARG ;; esac
done
shift $((OPTIND - 1))
BIN="target/$PROFILE/moleruffle"
for spec in "$@"; do
  tag="${spec%%=*}"; envs="${spec#*=}"
  out="target/mem-snap/$tag"; mkdir -p "$out"
  ready=0
  for attempt in 1 2 3 4; do
    env MOLE_SERVER="${MOLE_SERVER:-official}" ${=envs} "$BIN" >"$out/log.txt" 2>&1 &
    pid=$!
    for _ in {1..22}; do
      sleep 2
      kill -0 $pid 2>/dev/null || break
      mb=$(grep -a "\[perf\]" "$out/log.txt" | tail -1 | sed -nE 's/.*常驻 ([0-9]+)MB.*/\1/p')
      [[ -n $mb && $mb -ge $READY_MB ]] && { ready=1; break; }
    done
    [[ $ready == 1 ]] && break
    kill $pid 2>/dev/null; sleep 1; kill -9 $pid 2>/dev/null; sleep 1
  done
  [[ $ready == 0 ]] && { echo "[$tag] 没加载出来,跳过"; continue; }
  sleep "$WARM"
  footprint $pid >"$out/footprint.txt" 2>&1
  vmmap --summary $pid >"$out/vmmap.txt" 2>&1
  vmmap $pid >"$out/vmmap_full.txt" 2>&1
  kill $pid 2>/dev/null; sleep 1; kill -9 $pid 2>/dev/null
  total=$(grep -m1 -E "^ *phys_footprint:|Footprint:" "$out/footprint.txt" | head -1)
  echo "[$tag] $(grep -m1 -iE 'footprint' "$out/footprint.txt" | tr -s ' ')"
  grep -E "Owned physical|Malloc (Small|Large)|IOSurface|TOTAL" "$out/footprint.txt" | awk '{printf "    %s\n", $0}' | head -8
  echo "    显存块尺寸分布(块大小 × 个数,按总量排序):"
  grep "^owned unmapped (graphics)" "$out/vmmap_full.txt" | sed -E 's/.*\[ *([0-9.]+[KMG]) .*/\1/' | sort | uniq -c \
    | awk '{n=$1; s=$2; u=substr(s,length(s)); v=substr(s,1,length(s)-1)+0; if(u=="K")v/=1024; if(u=="G")v*=1024; printf "%9.1f MB  %5d × %s\n", n*v, n, s}' \
    | sort -rn | head -10 | sed 's/^/      /'
done
