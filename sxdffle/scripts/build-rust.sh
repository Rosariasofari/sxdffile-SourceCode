#!/usr/bin/env bash
# ---------------------------------------------------------------
# SXDffle —— Rust 引擎层交叉编译脚本
#
# 说明：CC/AR 等环境变量名中含 '-'（如 CC_aarch64-linux-android），
#       在 zsh 下无法直接 export，因此本脚本必须用 bash 执行：
#           bash scripts/build-rust.sh [目标架构...]
#       默认编译 arm64-v8a（现代手机）与 armeabi-v7a（老机型）。
# ---------------------------------------------------------------
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

# ---- 定位 NDK ----
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    for candidate in \
        "$HOME/Android/Sdk/ndk/"* \
        "/opt/android-ndk/android-ndk-r27c" \
        "/usr/local/lib/android/sdk/ndk/"* ; do
        if [ -d "$candidate/toolchains/llvm/prebuilt/linux-x86_64" ]; then
            export ANDROID_NDK_HOME="$candidate"
            break
        fi
    done
fi

if [ -z "${ANDROID_NDK_HOME:-}" ] || [ ! -d "$ANDROID_NDK_HOME" ]; then
    echo "错误：未找到 Android NDK。请设置 ANDROID_NDK_HOME 环境变量。" >&2
    exit 1
fi

case "$(uname -s)" in
    Linux)  HOST_TAG=linux-x86_64 ;;
    Darwin) HOST_TAG=darwin-x86_64 ;;
    *)      HOST_TAG=linux-x86_64 ;;
esac
NDK_BIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$HOST_TAG/bin"
API="${SXDFFLE_API_LEVEL:-26}"

echo "== SXDffle 构建 =="
echo "NDK   : $ANDROID_NDK_HOME"
echo "API   : $API"

# ---- 配置各架构交叉编译工具链 ----
setup_target() {
    local rust_target="$1" cc_prefix="$2"
    export "CC_${rust_target}=${NDK_BIN}/${cc_prefix}${API}-clang"
    export "CXX_${rust_target}=${NDK_BIN}/${cc_prefix}${API}-clang++"
    export "AR_${rust_target}=${NDK_BIN}/llvm-ar"
    export "RANLIB_${rust_target}=${NDK_BIN}/llvm-ranlib"
}

TARGETS=("$@")
if [ ${#TARGETS[@]} -eq 0 ]; then
    TARGETS=(aarch64-linux-android armv7-linux-androideabi)
fi

for t in "${TARGETS[@]}"; do
    case "$t" in
        aarch64-linux-android)     setup_target aarch64-linux-android     aarch64-linux-android ;;
        armv7-linux-androideabi)   setup_target armv7-linux-androideabi   armv7a-linux-androideabi ;;
        x86_64-linux-android)      setup_target x86_64-linux-android      x86_64-linux-android ;;
        i686-linux-android)        setup_target i686-linux-android        i686-linux-android ;;
        *) echo "跳过未知目标: $t" >&2; continue ;;
    esac

    echo "---- 编译 $t ----"
    if ! rustup target list --installed | grep -qx "$t"; then
        rustup target add "$t"
    fi
    # 交给 rust-toolchain.toml 决定工具链版本（gc-arena 需要 1.99+）
    cargo build --release --target "$t"
done

echo
echo "== 产物 =="
for t in "${TARGETS[@]}"; do
    so="$PROJECT_DIR/target/$t/release/libsxdffle.so"
    if [ -f "$so" ]; then
        printf "  %-26s %s\n" "$t" "$(du -h "$so" | cut -f1)"
    fi
done
echo "完成。"
