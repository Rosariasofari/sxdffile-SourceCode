#!/usr/bin/env bash
# ---------------------------------------------------------------
# SXDffle —— 打包 APK
#
# 前置：已执行 scripts/fetch-deps.sh 与 scripts/build-rust.sh，
#       原生库位于 app/src/main/jniLibs/<abi>/libsxdffle.so
#
# 用法：bash scripts/build-apk.sh [debug|release]
# ---------------------------------------------------------------
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

VARIANT="${1:-debug}"

# 原生库已预编译，告知 Gradle 跳过关联的 cargo-ndk 任务，避免重复编译
export SXDFFLE_PREBUILT_NATIVE=1

if [ -z "${ANDROID_HOME:-}" ] && [ -z "${ANDROID_SDK_ROOT:-}" ]; then
    for candidate in "$HOME/Android/Sdk" "/opt/android-sdk" "/usr/local/lib/android/sdk"; do
        if [ -d "$candidate" ]; then
            export ANDROID_HOME="$candidate"
            break
        fi
    done
fi

if [ ! -d "$PROJECT_DIR/app/src/main/jniLibs" ]; then
    echo "警告：未找到 jniLibs，将尝试由 Gradle 现场编译原生库（较慢）。" >&2
    unset SXDFFLE_PREBUILT_NATIVE
fi

case "$VARIANT" in
    debug)   TASK=":app:assembleDebug" ;;
    release) TASK=":app:assembleRelease" ;;
    *)       echo "未知变体: $VARIANT（应为 debug 或 release）" >&2; exit 1 ;;
esac

echo "== SXDffle APK 打包 ($VARIANT) =="
./gradlew "$TASK" --no-daemon

echo
echo "== 产物 =="
find app/build/outputs/apk -name "*.apk" -printf "  %p  (%s bytes)\n" 2>/dev/null || \
    find app/build/outputs/apk -name "*.apk" -exec ls -lh {} \; 2>/dev/null || \
    echo "  未找到 APK，请检查上面的构建输出。"
