#!/bin/zsh
# 启动时间对照:交替启动若干轮,记录"进程启动 → 主 SWF 解析完成(Loaded SWF)"与"→ 登录页加载完"。
# 登录页加载完 = 日志"里程碑:游戏报告首页素材加载成功"(游戏自己的统计打点,发出请求时记);
# 不带这行日志的旧二进制退回看"常驻 ≥18MB"(惰性形状打开后常驻只有 6MB 左右,不能再用它)。
# 用法:desktop/startup-ab.sh <轮数> "标签=环境变量" "标签=环境变量" ...
set -u
cd "$(dirname "$0")/.."
N=$1; shift
OUT=target/startup-ab; mkdir -p $OUT
for i in $(seq 1 $N); do
  for spec in "$@"; do
    tag="${spec%%=*}"; envs="${spec#*=}"
    log=$OUT/${tag}_$i.log
    t0=$(python3 -c 'import time;print(time.time())')
    env MOLE_SERVER=official ${=envs} target/release/moleruffle >$log 2>&1 &
    pid=$!
    for _ in {1..40}; do
      sleep 1
      grep -aq "里程碑:游戏报告首页素材加载成功\|常驻 1[89]MB\|常驻 [2-9][0-9]MB" $log && break
    done
    kill $pid 2>/dev/null; sleep 1; kill -9 $pid 2>/dev/null
    python3 - "$log" "$t0" "$tag" <<'PY'
import re,sys,datetime
log,t0,tag=sys.argv[1],float(sys.argv[2]),sys.argv[3]
lines=[re.sub(r"\x1b\[[0-9;]*m","",l) for l in open(log,errors="replace")]
def ts(pat):
    for l in lines:
        if re.search(pat,l):
            m=re.match(r"(\d{4}-\d\d-\d\dT[\d:.]+)Z",l)
            if m: return datetime.datetime.fromisoformat(m[1]).replace(tzinfo=datetime.timezone.utc).timestamp()-t0
    return None
first=ts(r"MoleRuffle 启动"); pre=ts(r"预取:"); swf=ts(r"Loaded SWF")
ready=ts(r"里程碑:游戏报告首页素材加载成功") or ts(r"常驻 (1[89]|[2-9]\d)MB")
f=lambda v: f"{v:5.2f}" if v is not None else "  -  "
print(f"[{tag}] 启动日志 {f(first)}s  发出预取 {f(pre)}s  主SWF {f(swf)}s  登录页完成 {f(ready)}s")
PY
  done
done
