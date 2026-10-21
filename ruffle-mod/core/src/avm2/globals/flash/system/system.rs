//! `flash.system.System` native methods

use crate::avm2::Error;
use crate::avm2::activation::Activation;
use crate::avm2::error::{make_error_2017, make_error_2018};
use crate::avm2::function::FunctionArgs;
use crate::avm2::parameters::ParametersExt;
use crate::avm2::value::Value;
use crate::avm2_stub_method;
use ruffle_common::sandbox::SandboxType;
use std::sync::atomic::{AtomicBool, Ordering};

/// Implements `flash.system.System.setClipboard` method
pub fn set_clipboard<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    // The following restrictions only apply to the plugin.
    // TODO: Check the type of event that triggered the function call.
    #[cfg(target_family = "wasm")]
    if false {
        return Err(crate::avm2::error::make_error_2176(activation));
    }

    let new_content = args.get_string_non_null(activation, 0, "text")?;
    activation
        .context
        .ui
        .set_clipboard_content(new_content.to_string());

    Ok(Value::Undefined)
}

/// `System.gc()`：置位请求，由下一次 `Player::tick` 执行完整回收。
///
/// 这里不能直接跑 GC —— native 方法执行时 Player 正被 tick 持有，
/// 再取锁会死锁。
pub fn gc<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    crate::player::GC_REQUESTED.store(true, Ordering::Relaxed);
    Ok(Value::Undefined)
}

/// 是否让 `System.totalMemory` 系列返回真实内存（调试开关）。
///
/// 默认开。关掉后回到上游行为（固定 90MB），用于对比"游戏是否根据这个
/// 数值在做缓存决策"。
static REAL_MEMORY_API: AtomicBool = AtomicBool::new(true);

pub fn set_real_memory_api(on: bool) {
    REAL_MEMORY_API.store(on, Ordering::Relaxed);
}

/// 进程常驻内存（字节）。Android/Linux 读 /proc/self/statm 第二列（页数）。
fn process_rss_bytes() -> Option<u64> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // 页面大小统一按 4KB 估：Android/Linux 常见值，误差 <2%
        Some(pages.saturating_mul(4096))
    }
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    {
        None
    }
}

fn total_memory_number() -> f64 {
    if !REAL_MEMORY_API.load(Ordering::Relaxed) {
        return 90.0 * 1024.0 * 1024.0;
    }
    process_rss_bytes()
        .map(|b| b as f64)
        .unwrap_or(90.0 * 1024.0 * 1024.0)
}

/// `System.totalMemoryNumber`
pub fn get_total_memory_number<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    Ok(Value::Number(total_memory_number()))
}

/// `System.privateMemory`（Flash 语义是进程私有内存，这里用 RSS 近似）
pub fn get_private_memory<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    Ok(Value::Number(total_memory_number()))
}

/// `System.freeMemory`
///
/// Flash 的语义是"已分配给运行时但尚未使用的内存"，不是系统剩余内存。
/// 保持和 total 大约 1:9 的比例（原 stub 是 10MB/90MB），这样游戏看到
/// "已用 3.6GB、空闲 360MB" 时会按自己的策略清理缓存。
pub fn get_free_memory<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    Ok(Value::Number(total_memory_number() / 9.0))
}

pub fn exit<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    avm2_stub_method!(activation, "flash.system.System", "exit");

    if cfg!(target_family = "wasm") {
        return Err(make_error_2018(activation));
    }

    let movie = activation.caller_movie_or_root();
    if !matches!(movie.sandbox_type(), SandboxType::LocalTrusted) {
        return Err(make_error_2017(activation));
    }

    // TODO: Actually quit.
    Err("Unsupported System.exit() was called".into())
}
