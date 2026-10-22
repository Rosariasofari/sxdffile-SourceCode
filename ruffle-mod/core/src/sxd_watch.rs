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

/// 上一次「有任何 MovieClip 真的推进了时间轴」的时刻（微秒）。
pub static LAST_ADVANCE_US: AtomicU64 = AtomicU64::new(0);

/// 由 frame_lifecycle 在时间轴真正推进时调用。
#[inline]
pub fn note_advance() {
    LAST_ADVANCE_US.store(now_us(), Ordering::Relaxed);
}

/// 距离上一次时间轴推进过了多少秒（全场静止时长）。
pub fn idle_secs() -> u64 {
    let last = LAST_ADVANCE_US.load(Ordering::Relaxed);
    if last == 0 {
        return 0;
    }
    now_us().saturating_sub(last) / 1_000_000
}

/// 本帧已运行毫秒数；不在帧内时返回 `None`。
pub fn frame_elapsed_ms() -> Option<u64> {
    let start = FRAME_START_US.load(Ordering::Relaxed);
    if start == 0 {
        return None;
    }
    Some(now_us().saturating_sub(start) / 1000)
}

/// 当前 AS3 调用深度（run_actions 嵌套层数）。
///
/// 阶段 A 的回收只在 depth==1（最外层脚本回调）启用：那一层没有父级
/// Activation 压在 Rust 栈上，根集可控；之前的 SIGSEGV 正是父级帧没被根覆盖。
static CALL_DEPTH: AtomicU64 = AtomicU64::new(0);
static MAX_DEPTH: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn call_depth() -> u64 {
    CALL_DEPTH.load(Ordering::Relaxed)
}

#[inline]
pub fn max_depth() -> u64 {
    MAX_DEPTH.load(Ordering::Relaxed)
}

pub struct CallDepthGuard;

impl CallDepthGuard {
    #[inline]
    pub fn new() -> Self {
        let d = CALL_DEPTH.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = MAX_DEPTH.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |m| {
            Some(if d > m { d } else { m })
        });
        CallDepthGuard
    }
}

impl Drop for CallDepthGuard {
    #[inline]
    fn drop(&mut self) {
        CALL_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

/// preload 阶段的插桩（0.261135）。
///
/// 为什么单开一套而不是复用 `SlowFrame`：preload 不进 AVM2 解释器，
/// `tick()` 一次都打不到；更糟的是如果主线程卡在**加载器/网络**上、
/// `run_frame` 根本没被调用，`FRAME_START_US` 是 0，`frame_elapsed_ms()`
/// 返回 `None`，宿主那条「慢帧」分支整段被跳过 —— 这就是 6.7GB 却 0 条现场的原因。
///
/// 所以这里用一套**不依赖帧计时**的原子变量：只要进了 preload 就有值，
/// 无论 preload 在不在帧内、主线程卡在哪一层。
static PRELOAD_TAG: AtomicU64 = AtomicU64::new(0); // 0 = 不在 preload
static PRELOAD_DONE: AtomicU64 = AtomicU64::new(0); // 本次 preload 调用已解析的 tag 数
static PRELOAD_START_US: AtomicU64 = AtomicU64::new(0); // 最外层 preload 进入时刻
static PRELOAD_NEST: AtomicU64 = AtomicU64::new(0); // preload 嵌套层数（DefineSprite）
static PRELOAD_TOTAL_US: AtomicU64 = AtomicU64::new(0); // 本轮连续加载累计耗时
static PRELOAD_LAST_US: AtomicU64 = AtomicU64::new(0); // 上一次离开 preload 的时刻

/// 连续加载的判定间隔：两次 preload 间隔超过这个值就算「新的一轮」。
///
/// 为什么要累计而不是只记单次：`ExecutionLimit` 把 preload 切成 2000 ops / 16ms 一片，
/// 单次调用永远只有十几毫秒 —— 卡 20 秒在**单次**上根本看不出来。
/// 真正要回答的是「这一轮加载已经连续跑了多久」。
const PRELOAD_BURST_GAP_US: u64 = 2_000_000;

/// preload 进入守卫：**所有**返回路径（早退、`?`、panic 展开）都会经 `Drop` 归零。
///
/// 0.261134 之前踩过的坑：手动在每个 `return` 前 `clear`，漏一个就会让
/// 后续的渲染慢帧被误标成 preload。
pub struct PreloadGuard;

impl PreloadGuard {
    #[inline]
    pub fn new() -> Self {
        let now = now_us();
        if PRELOAD_NEST.fetch_add(1, Ordering::Relaxed) == 0 {
            // 距上一次离开 preload 超过 2 秒 → 新的一轮加载，累计重新开始。
            let last = PRELOAD_LAST_US.load(Ordering::Relaxed);
            if last == 0 || now.saturating_sub(last) > PRELOAD_BURST_GAP_US {
                PRELOAD_TOTAL_US.store(0, Ordering::Relaxed);
            }
            PRELOAD_TAG.store(0, Ordering::Relaxed);
            PRELOAD_DONE.store(0, Ordering::Relaxed);
            PRELOAD_START_US.store(now, Ordering::Relaxed);
        }
        PreloadGuard
    }
}

impl Drop for PreloadGuard {
    #[inline]
    fn drop(&mut self) {
        if PRELOAD_NEST.fetch_sub(1, Ordering::Relaxed) == 1 {
            let start = PRELOAD_START_US.load(Ordering::Relaxed);
            if start != 0 {
                let spent = now_us().saturating_sub(start);
                PRELOAD_TOTAL_US.fetch_add(spent, Ordering::Relaxed);
            }
            PRELOAD_TAG.store(0, Ordering::Relaxed);
            PRELOAD_START_US.store(0, Ordering::Relaxed);
            PRELOAD_LAST_US.store(now_us(), Ordering::Relaxed);
        }
    }
}

/// 每解析一个 tag 调一次（preload 热路径：2 次 relaxed store，无分配）。
#[inline]
pub fn note_preload(tag: u16, done: u32) {
    PRELOAD_TAG.store(tag as u64, Ordering::Relaxed);
    PRELOAD_DONE.store(done as u64, Ordering::Relaxed);
}

/// 结束/出错时归零；正常路径由 `PreloadGuard` 覆盖，这个给显式收尾用。
#[inline]
pub fn clear_preload() {
    PRELOAD_TAG.store(0, Ordering::Relaxed);
}

/// 当前 preload 的 tag 码；0 表示不在 preload。
#[inline]
pub fn preload_tag() -> u16 {
    PRELOAD_TAG.load(Ordering::Relaxed) as u16
}

/// 本次 preload 调用已解析的 tag 数。
#[inline]
pub fn preload_done() -> u32 {
    PRELOAD_DONE.load(Ordering::Relaxed) as u32
}

/// 本轮连续加载累计耗时（毫秒）：已结束的分片 + 正在跑的这一片。
///
/// 单次 preload 只有 16ms，这个数才是「卡了 20 秒」的那个 20 秒。
pub fn preload_total_ms() -> u64 {
    let done = PRELOAD_TOTAL_US.load(Ordering::Relaxed);
    let start = PRELOAD_START_US.load(Ordering::Relaxed);
    let cur = if start == 0 {
        0
    } else {
        now_us().saturating_sub(start)
    };
    done.saturating_add(cur) / 1000
}

/// preload 嵌套层数：>1 说明正在展开 `DefineSprite`。
#[inline]
pub fn preload_depth() -> u64 {
    PRELOAD_NEST.load(Ordering::Relaxed)
}

/// tag 码转名字（只在慢路径调用，允许分配）。
pub fn preload_tag_name(tag: u16) -> String {
    crate::swf::TagCode::format(tag)
}

/// 一次取全 preload 现场：`(tag, done, total_ms, depth)`。不在 preload 时返回 `None`。
pub fn peek_preload() -> Option<(u16, u32, u64, u64)> {
    if PRELOAD_TAG.load(Ordering::Relaxed) == 0 {
        return None;
    }
    Some((
        preload_tag(),
        preload_done(),
        preload_total_ms(),
        preload_depth(),
    ))
}

/// 一次慢帧现场快照。
#[derive(Clone, Debug)]
pub struct SlowFrame {
    /// 采样时本帧已经跑了多少毫秒。
    pub elapsed_ms: u64,
    /// 现场所在阶段：`"exec"`（解释器）或 `"preload"`（SWF 解析）。
    pub stage: &'static str,
    /// preload 阶段正在解析的 tag 码（解释器阶段为 0）。
    pub tag: u16,
    /// preload 阶段本次调用已解析的 tag 数（解释器阶段为 0）。
    pub done: u32,
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

    let method_name = format!(
        "{} (深度{})",
        method
            .method_name()
            .map(|c| c.into_owned())
            .unwrap_or_else(|| "<anonymous>".to_string()),
        call_depth()
    );
    let top_ops = crate::avm2::opstats::snapshot_top(4);
    if let Ok(mut guard) = SLOW.try_lock() {
        *guard = Some(SlowFrame {
            elapsed_ms: ms,
            stage: "exec",
            tag: 0,
            done: 0,
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
