#!/bin/zsh
# 用 ruffle-fork 自带的图像回归(tests 的 visual 目录)验证 MoleRuffle 的渲染改动。
#
# fork 自己的测试用的是官方 wgpu-core,覆盖不到 vendor/wgpu-core 的补丁;这里用 cargo 的 --config
# 临时把 fork 的 wgpu-core / wgpu-hal 换成 MoleRuffle/vendor 里的补丁版(不改任何文件,Cargo.lock 结束后还原),
# 并用 MOLE_FAST_PATHS=1 打开 fork 里全部渲染快路径,分四轮跑:
#   基线    :官方 wgpu + 快路径全关(上游行为)
#   快路径  :官方 wgpu + 快路径全开(fork 快路径自身的差异)
#   补丁关  :补丁版 wgpu + 快路径全开 + MOLE_METAL_SINGLE_CB=0(vendor 补丁除单命令缓冲外的差异)
#   补丁开  :补丁版 wgpu + 快路径全开 + MOLE_METAL_SINGLE_CB=1(并确认快路径确实被走到)
# 四轮的失败集合必须相同;补丁轮里 cargo 若报 "was not used in the crate graph"(补丁没被用上)直接判失败。
# 每轮先清掉调用者环境里的全部 MOLE_* 变量。改 vendor、升级 wgpu、改 fork 渲染代码后都应跑一遍。
# 注意:运行期间会临时改写 ruffle-fork 的 Cargo.lock(退出时还原);被强杀时请手动 git checkout 它。
#
# 用法:desktop/fastcb-regress.sh [测试过滤词,默认 visual]
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
FORK="$(cd "$HERE/../ruffle-fork" && pwd)"
FILTER="${1:-visual}"
OUT="$HERE/target/fastcb-regress"
mkdir -p "$OUT"
export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home}"
export CARGO_TARGET_DIR="$OUT/target"
cd "$FORK"
cp Cargo.lock "$OUT/Cargo.lock.bak"
trap 'cp "$OUT/Cargo.lock.bak" "$FORK/Cargo.lock"' EXIT

run() {
  local tag=$1; shift
  local patch=$1; shift
  local args=(test -p tests --features imgtests --test tests)
  [[ $patch == 1 ]] && args=(--config "patch.crates-io.wgpu-core.path=\"$HERE/vendor/wgpu-core\"" \
    --config "patch.crates-io.wgpu-hal.path=\"$HERE/vendor/wgpu-hal\"" "${args[@]}")
  echo "== $tag"
  local clean=()
  for v in ${(k)parameters[(I)MOLE_*]}; do clean+=(-u "$v"); done
  env "${clean[@]}" "$@" cargo "${args[@]}" -- "$FILTER" >"$OUT/$tag.log" 2>&1
  if [[ $patch == 1 ]] && grep -q "was not used in the crate graph" "$OUT/$tag.log"; then
    echo "✗ $tag:补丁没被用上(was not used in the crate graph)"; PATCH_UNUSED=1
  fi
  grep -E "^test result" "$OUT/$tag.log" | tail -1
  grep -E "\.\.\. FAILED$" "$OUT/$tag.log" | awk '{print $(NF-2)}' | sort >"$OUT/$tag.failed"
  local stats=$(grep -a "^\[mole_stats\]" "$OUT/$tag.log" | tail -1)
  [[ -n $stats ]] && echo "   $stats"
}

PATCH_UNUSED=0
run 基线 0 MOLE_STATS_DUMP=1
run 快路径 0 MOLE_FAST_PATHS=1 MOLE_STATS_DUMP=1
run 补丁关 1 MOLE_FAST_PATHS=1 MOLE_METAL_SINGLE_CB=0 MOLE_STATS_DUMP=1
run 补丁开 1 MOLE_FAST_PATHS=1 MOLE_METAL_SINGLE_CB=1 MOLE_STATS_DUMP=1

echo "== 失败集合对比"
ok=1
[[ $PATCH_UNUSED == 1 ]] && ok=0
for t in 快路径 补丁关 补丁开; do
  if ! diff -q "$OUT/基线.failed" "$OUT/$t.failed" >/dev/null; then
    ok=0; echo "✗ $t 与基线不同:"; diff "$OUT/基线.failed" "$OUT/$t.failed" | sed 's/^/   /'
  fi
done
fast=$(grep -a "^\[mole_stats\]" "$OUT/补丁开.log" | tail -1 | sed -nE 's/.*其中快路径 ([0-9]+).*/\1/p')
if [[ -z $fast || $fast == 0 ]]; then
  ok=0; echo "✗ 补丁开那一轮没有走到快路径(补丁没生效?)"
fi
[[ $ok == 1 ]] && echo "✓ 四轮失败集合相同($(wc -l <"$OUT/基线.failed" | tr -d ' ') 项),快路径走了 $fast 个通道"
exit $((1 - ok))
