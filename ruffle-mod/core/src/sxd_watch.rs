//! SXDffle：慢帧看门狗。
//!
//! 0.261120 的实测暴露了一类**看不见**的崩溃：主线程卡在一个 8 秒的帧里，
//! 期间分配了 2000 万个对象、RSS 从 2.2GB 涨到 7.1GB，然后被 LMK 杀掉。
//! 因为所有采样（15 秒常规采样、1 秒爆发快照、500ms 冲击检测）都挂在
//! 渲染循环里，而渲染循环正卡在这一帧上 —— 崩溃现场一片空白。
//!
//! 这里补上两件事：
//! 1. `FrameTimer`：帧开始/结束打点，供**独立线程**读取「本帧已经跑了多久」；
//! 2. `tick()`：解释器每 2048 条指令调用一次，超过 1.2 秒就把
//!    「当前方法 + 当前指令 + 指令耗时 TOP」写进一个 Mutex，
//!    独立线程直接读走并打日志。
//!
//! 为什么不在 `run_actions` 入口记录：8 秒的帧往往是**一个方法内部的死循环**，
//! 入口只进一次，等我们发现时它已经跑完了。必须从指令边界采样。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// 当前帧开始的时刻（微秒，0 表示不在帧内）。
pub static FRAME_START_US: AtomicU64 = AtomicU64::new(0);

/// 上一次写慢帧快照的时刻，用于节流。
static LAST_RECORD_US: AtomicU64 = AtomicU64::new(0);

#[inline]
fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// 帧计时守卫：构造时打点，析构时清除（`run_frame` 里任何提前返回都能覆盖）。
pub struct FrameTimer;

impl FrameTimer {
    #[inline]
    pub fn new() -> Self {
        FRAME_START_US.store(now_us(), Ordering::Relaxed);
        FrameTimer
    }
}

impl Drop for FrameTimer {
    #[inline]
    fn drop(&mut self) {
        FRAME_START_US.store(0, Ordering::Relaxed);
    }
}

/// 本帧已运行毫秒数；不在帧内时返回 `None`。
pub fn frame_elapsed_ms() -> Option<u64> {
    let start = FRAME_START_US.load(Ordering::Relaxed);
    if start == 0 {
        return None;
    }
    Some(now_us().saturating_sub(start) / 1000)
}

/// 一次慢帧现场快照。
#[derive(Clone, Debug)]
pub struct SlowFrame {
    /// 采样时本帧已经跑了多少毫秒。
    pub elapsed_ms: u64,
    /// 当前正在执行的 AS3 方法（`method_name()`，通常是 `com::Data/parseSocketData` 这种）。
    pub method: String,
    /// 采样时正在执行的那条指令的变体名。
    pub opcode: String,
    /// 指令耗时 TOP（名称, 采样次数, 累计微秒）。
    pub top_ops: Vec<(String, u64, u64)>,
    /// 写入时刻（微秒），宿主用来判断快照是否还新鲜。
    record_us: u64,
}

static SLOW: Mutex<Option<SlowFrame>> = Mutex::new(None);

/// 解释器指令边界调用：本帧过慢时记录一次现场（内部 400ms 节流）。
///
/// 平时的开销是一次原子 load + 一次比较；只有帧已经超过 1.2 秒才会格式化字符串，
/// 那时这一帧本来就已经失败了，诊断价值远大于开销。
#[inline]
pub fn tick(method: crate::avm2::Method<'_>, opname: &str) {
    let start = FRAME_START_US.load(Ordering::Relaxed);
    if start == 0 {
        return;
    }
    let now = now_us();
    let ms = now.saturating_sub(start) / 1000;
    if ms < 1200 {
        return;
    }
    let last = LAST_RECORD_US.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 400_000 {
        return;
    }
    if LAST_RECORD_US
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let method_name = method
        .method_name()
        .map(|c| c.into_owned())
        .unwrap_or_else(|| "<anonymous>".to_string());
    let top_ops = crate::avm2::opstats::snapshot_top(4);
    if let Ok(mut guard) = SLOW.try_lock() {
        *guard = Some(SlowFrame {
            elapsed_ms: ms,
            method: method_name,
            opcode: opname.to_string(),
            top_ops,
            record_us: now,
        });
    }
}

/// 取一份「还新鲜」的慢帧快照（2 秒内写入的）。
pub fn peek_slow() -> Option<SlowFrame> {
    let now = now_us();
    let guard = SLOW.try_lock().ok()?;
    let snap = guard.as_ref()?;
    if now.saturating_sub(snap.record_us) > 2_000_000 {
        return None;
    }
    Some(snap.clone())
}

/// 清空快照（宿主打印完调用，避免同一现场重复刷屏）。
pub fn clear_slow() {
    if let Ok(mut guard) = SLOW.try_lock() {
        *guard = None;
    }
}
