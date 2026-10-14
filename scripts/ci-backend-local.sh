#!/usr/bin/env bash
# =============================================================================
# ci-backend-local.sh —— 本机跑「与 CI backend job 完全同口径」的后端门禁
# =============================================================================
#
# 为什么需要它（issue #127）：本机惯用命令（`cargo check && cargo test && cargo
# clippy -- -D warnings`，无 feature、macOS 宿主）与 CI 口径（`--features
# marker-helper`、ubuntu 宿主、外加两步 Windows 交叉门禁）**从来是两条不同的
# 命令**——#119 合并时首次 CI 运行即红 5 条（平台相关用例 macOS 本机全绿），
# 且第 4 步失败即中断、Windows 交叉门禁从未轮到（#128 的 E0560 编译破损只能靠
# 人工复刻 CI 命令才炸出来）。**本机全绿推不出 CI 会绿**；合并前请以本脚本为准。
#
# 与 CI 的对应关系（.github/workflows/ci.yml backend job，逐条等价）：
#   ① cargo fmt --check
#   ② cargo check
#   ③ cargo clippy -- -D warnings
#   ④ cargo test --features marker-helper        ← 本机惯用命令缺的 feature
#   ⑤ rustup target add x86_64-pc-windows-gnu    ← 幂等
#   ⑥ cargo check  --target x86_64-pc-windows-gnu --all-targets --features marker-helper
#   ⑦ cargo clippy --target x86_64-pc-windows-gnu --all-targets --features marker-helper -- -D warnings
#
# 宿主差异（如实申报，不算失真）：
#   * **Windows 宿主**：本机 cargo 已原生编译全部 Windows-only 代码（`#![cfg(windows)]`
#     的 m9r_e2e.rs 等在本机 fmt/check/clippy/test 全覆盖），交叉到 gnu 目标反而
#     需要另装工具链——故默认**跳过 ⑤⑥⑦ 并说明**，原生四步已覆盖 CI 交叉门禁
#     要抓的「Windows-only 编译破损」；要强制跑交叉传 `--with-cross`。
#   * **macOS/Linux 宿主**：⑤⑥⑦ 照跑。`cargo check/clippy --target` 不做链接，
#     无需 mingw-w64（CI 装它是因为 apt 侧顺手；本机只要 rustup 有该 target 即可）。
#   * 平台相关用例在「本机全绿、CI 红」的那类（tailscale 平台语义），本脚本无法
#     替代 ubuntu 宿主的运行语义——PR 仍以 CI 运行为准；本脚本是「别让低级红
#     浪费一次 CI 轮次」的前置闸。
#
# 用法：bash scripts/ci-backend-local.sh [--with-cross]
# 退出码：任一步失败即非零（**不吞退出码**——照 check-real-db-untouched.sh 惯例）。
set -uo pipefail

cd "$(dirname "$0")/../src-tauri" || exit 1

step_no=0
step() {
  step_no=$((step_no + 1))
  printf '\n\033[1;36m===== 步骤 %d/%d: %s =====\033[0m\n' "$step_no" "$TOTAL" "$1"
}

fail() {
  printf '\n\033[1;31m✗ 门禁失败于步骤 %d: %s\033[0m\n' "$step_no" "$1" >&2
  exit 1
}

WITH_CROSS=0
[ "${1:-}" = "--with-cross" ] && WITH_CROSS=1

HOST_WINDOWS=0
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT) HOST_WINDOWS=1 ;;
esac

# Windows 宿主的全量 `--all-targets` 并行 rustc 实测有文件锁竞争（E0463 can't find
# crate / E0786 invalid metadata / E0460 混链，错误形态每次不同、单目标重跑即绿
# ——2026-10-14 本机实测）。这不改变测试集（CI ubuntu 全并行无此问题），只是本机
# 的编译调度参数：未显式指定时给 Windows 宿主降并行，显式设置则尊重之。
if [ -z "${CARGO_BUILD_JOBS:-}" ] && [ "$HOST_WINDOWS" -eq 1 ]; then
  export CARGO_BUILD_JOBS=3
fi

RUN_CROSS=0
if [ "$WITH_CROSS" -eq 1 ]; then
  RUN_CROSS=1
elif [ "$HOST_WINDOWS" -eq 0 ]; then
  RUN_CROSS=1
fi

if [ "$RUN_CROSS" -eq 1 ]; then
  TOTAL=7
else
  TOTAL=4
  if [ "$HOST_WINDOWS" -eq 1 ]; then
    printf '\033[1;33m[i] Windows 宿主：原生编译已覆盖 CI 交叉门禁要抓的 Windows-only 破损，跳过 ⑤⑥⑦（强制跑传 --with-cross）\033[0m\n'
  fi
fi

step "cargo fmt --check"
cargo fmt --check || fail "cargo fmt --check（CI ①）"

step "cargo check"
cargo check || fail "cargo check（CI ②）"

step "cargo clippy -- -D warnings"
cargo clippy -- -D warnings || fail "cargo clippy（CI ③）"

step "cargo test --features marker-helper"
cargo test --features marker-helper || fail "cargo test --features marker-helper（CI ④；本机惯用命令缺的正是这个 feature）"

if [ "$RUN_CROSS" -eq 1 ]; then
  step "rustup target add x86_64-pc-windows-gnu（幂等）"
  rustup target add x86_64-pc-windows-gnu || fail "rustup target add（CI ⑤）"

  step "cargo check --target x86_64-pc-windows-gnu --all-targets --features marker-helper"
  cargo check --target x86_64-pc-windows-gnu --all-targets --features marker-helper \
    || fail "Windows 交叉 check（CI ⑥；#128 的 E0560 即这步抓的）"

  step "cargo clippy --target x86_64-pc-windows-gnu --all-targets --features marker-helper -- -D warnings"
  cargo clippy --target x86_64-pc-windows-gnu --all-targets --features marker-helper -- -D warnings \
    || fail "Windows 交叉 clippy（CI ⑦）"
fi

printf '\n\033[1;32m✓ 本机后端门禁全绿（%d 步，与 CI backend job 同口径）\033[0m\n' "$step_no"
