#!/usr/bin/env bash
# ---------------------------------------------------------------
# SXDffle —— 拉取引擎源码与需要本地化的依赖
#
# 上游 ruffle-android 通过 git 依赖 6 个 GitHub 仓库；在无法访问
# github.com 的环境（内网 / 部分 CI）下会直接失败。本脚本改用
# codeload.github.com 拉取 tarball，并放到 Cargo.toml 约定的位置：
#
#   <项目上级目录>/
#     ├── ruffle/          Ruffle 引擎源码（锁定 commit）
#     └── vendor-crates/   被本地化的依赖
#
# 用法：bash scripts/fetch-deps.sh
# ---------------------------------------------------------------
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORK_DIR="$(cd "$PROJECT_DIR/.." && pwd)"

# 与上游 ruffle-android 的 Cargo.lock 保持一致的 Ruffle commit
RUFFLE_REPO="ruffle-rs/ruffle"
RUFFLE_COMMIT="163b7d71ed9a3b14b6880e356ba2dc7376c5eb3f"

# 需要本地化的依赖：目录名|仓库|commit
DEPS=(
    "regras3|ruffle-rs/regras3|6d48179d6bd448f2cc66b76352c9f048dde968d1"
    "jpegxr|ruffle-rs/jpegxr|86cc1749d493288f53d7290ac3eb7082c475b5d9"
    "nellymoser|ruffle-rs/nellymoser|073eb48d907201f46dea0c8feb4e8d9a1d92208c"
    "rust-flash-lso|ruffle-rs/rust-flash-lso|61b717248aae853a4f5d8a103eba704268d286c8"
    "h263-rs|ruffle-rs/h263-rs|27ee833ca97aadd2a14cf0b3b4ec8b53f31ff6bd"
    "nihav-vp6|ruffle-rs/nihav-vp6|12d0bdf10a85641dc2de3d6900c747894431ed77"
)

CACHE_DIR="${SXDFFLE_CACHE:-$WORK_DIR/.sxdffle-cache}"
mkdir -p "$CACHE_DIR"

fetch_tar() {
    # $1=repo  $2=commit  $3=目标目录
    local repo="$1" commit="$2" dest="$3"
    local name
    name="$(basename "$repo")"
    local tarball="$CACHE_DIR/${name}-${commit}.tar.gz"

    if [ ! -f "$tarball" ] || [ ! -s "$tarball" ]; then
        echo "  下载 $repo@${commit:0:8} ..."
        curl -fL --retry 3 --retry-delay 2 -m 900 \
            -o "$tarball" \
            "https://codeload.github.com/$repo/tar.gz/$commit"
    fi

    rm -rf "$dest"
    mkdir -p "$dest"
    tar xzf "$tarball" -C "$dest" --strip-components=1
}

echo "== SXDffle 依赖拉取 =="
echo "工作目录: $WORK_DIR"

# ---- 1. Ruffle 引擎源码 ----
if [ -f "$WORK_DIR/ruffle/Cargo.toml" ] && [ -d "$WORK_DIR/ruffle/core" ]; then
    echo "Ruffle 源码已存在，跳过。"
else
    echo "-- Ruffle 引擎源码 ($RUFFLE_REPO) --"
    fetch_tar "$RUFFLE_REPO" "$RUFFLE_COMMIT" "$WORK_DIR/ruffle"
fi

# ---- 2. 本地化依赖 ----
echo "-- 本地化依赖 --"
for entry in "${DEPS[@]}"; do
    IFS='|' read -r dir repo commit <<< "$entry"
    dest="$WORK_DIR/vendor-crates/$dir"
    if [ -f "$dest/Cargo.toml" ] || \
       [ -f "$dest/flash-lso/Cargo.toml" ] || \
       [ -f "$dest/nihav-core/Cargo.toml" ] || \
       [ -f "$dest/h263/Cargo.toml" ]; then
        echo "  $dir 已存在，跳过。"
        continue
    fi
    echo "  $dir"
    fetch_tar "$repo" "$commit" "$dest"
done

# ---- 3. rustls-platform-verifier 的 Kotlin 组件（Gradle 子模块）----
PV_DIR="$PROJECT_DIR/vendor/rustls-platform-verifier"
if [ -d "$PV_DIR/android" ]; then
    echo "rustls-platform-verifier 已存在，跳过。"
else
    echo "-- rustls-platform-verifier (TLS 证书校验组件) --"
    fetch_tar "rustls/rustls-platform-verifier" "main" "$PV_DIR"
fi

echo
echo "依赖就绪。Ruffle 源码需打上 SXDffle 补丁（若尚未打）:"
echo "  patches/apply.sh"
echo "下一步：bash scripts/build-rust.sh"
