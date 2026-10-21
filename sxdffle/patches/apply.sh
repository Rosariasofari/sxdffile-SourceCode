#!/usr/bin/env bash
# ---------------------------------------------------------------
# SXDffle —— 应用对 Ruffle 引擎源码的补丁
#
# SXDffle 只在引擎侧做最小必要改动（网络层），其余全部逻辑都在
# host 层（Kotlin）与本项目的 src/ 中实现，便于跟随上游更新。
#
# 当前补丁：
#   0001-navigator-user-agent.patch
#     把 HTTP 客户端默认 User-Agent 从 "Ruffle/x.y.z" 改为可配置的
#     浏览器 UA —— 部分页游服务器会校验 UA，非浏览器 UA 可能被拒绝。
#
# 用法：bash patches/apply.sh
# ---------------------------------------------------------------
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORK_DIR="$(cd "$PROJECT_DIR/.." && pwd)"
RUFFLE_DIR="$WORK_DIR/ruffle"

if [ ! -d "$RUFFLE_DIR" ]; then
    echo "错误：未找到 Ruffle 源码目录 $RUFFLE_DIR" >&2
    echo "请先执行：bash scripts/fetch-deps.sh" >&2
    exit 1
fi

shopt -s nullglob
PATCHES=("$SCRIPT_DIR"/*.patch)

if [ ${#PATCHES[@]} -eq 0 ]; then
    echo "没有找到补丁文件。"
    exit 0
fi

for p in "${PATCHES[@]}"; do
    name="$(basename "$p")"
    if patch -p1 -d "$RUFFLE_DIR" --dry-run --forward -i "$p" >/dev/null 2>&1; then
        patch -p1 -d "$RUFFLE_DIR" --forward -i "$p"
        echo "已应用: $name"
    elif patch -p1 -d "$RUFFLE_DIR" --dry-run --reverse -i "$p" >/dev/null 2>&1; then
        echo "已应用过，跳过: $name"
    else
        echo "警告：$name 无法应用（可能上游已变更），请人工核对。" >&2
    fi
done

echo "完成。"
