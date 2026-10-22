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

/// AVM2 解释器「等待」的累计微秒（等 socket / 等帧 / 等 IO）。
/// 热点百分比要把这段剔掉才有意义，见 `opstats::IDLE_GAP_US`。
#[inline]
pub fn avm_idle_us() -> u64 {
    crate::avm2::opstats::idle_us()
}

/// 供同 crate 其他模块（socket）打时间戳用。
#[inline]
pub(crate) fn now_us_pub() -> u64 {
    now_us()
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

/// 一次取全 preload 现场：`(tag, done, total_ms, depth)`。
///
/// **只要这一轮加载还在进行就返回 `Some`**，即使采样恰好落在两次分片之间（此时 `tag == 0`）。
/// 0.261135 的教训：判据写成「`PRELOAD_TAG != 0`」，而 preload 被切成 16ms 一片，
/// 独立采样线程 99% 的概率落在片与片之间，于是整轮加载一条日志都没打出来。
pub fn peek_preload() -> Option<(u16, u32, u64, u64)> {
    let now = now_us();
    let last = PRELOAD_LAST_US.load(Ordering::Relaxed);
    let running = PRELOAD_TAG.load(Ordering::Relaxed) != 0;
    // 这一轮加载是否还「活着」：要么正在解析，要么上一次离开不超过 2 秒
    let alive = running || (last != 0 && now.saturating_sub(last) <= PRELOAD_BURST_GAP_US);
    if !alive {
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

// ---- CoerceS 归因（0.261136）----
//
// 0.261135 的真机日志里 `AVM2 热点` 反复出现 `CoerceS 1%~2% / 96%~99%`：
// 采样次数占比极低，耗时占比却是全部。opstats 是「每 4096 条指令采一次、
// 把两次采样之间的时间记给当前指令」，所以这代表**少数几次 CoerceS 单次就吃掉了整段时间**。
//
// 而 `Value::Object` 的 `coerce_to_string` 会先调 `coerce_to_primitive(Hint::String)`，
// 也就是走 AS3 的 `toString`；如果那个 toString 是 native 实现（Array/Object/Vector），
// 它整段耗时都会算在这条 CoerceS 上，在解释器里完全看不见。
//
// 这里直接量：**谁家的对象、转出来多大、花了多久**。
use std::sync::atomic::AtomicU32;

static COERCE_N: AtomicU64 = AtomicU64::new(0);
static COERCE_US: AtomicU64 = AtomicU64::new(0);
static COERCE_SLOW_N: AtomicU64 = AtomicU64::new(0);
static COERCE_WORST_US: AtomicU64 = AtomicU64::new(0);
static COERCE_LAST_LOG_US: AtomicU64 = AtomicU64::new(0);
static COERCE_LOG_COUNT: AtomicU32 = AtomicU32::new(0);

/// (类名, 次数, 累计耗时微秒)
static COERCE_TOP: Mutex<Vec<(String, u64, u64)>> = Mutex::new(Vec::new());

/// 单次对象转字符串的耗时门槛：低于它的不记（正常也就几微秒）。
const COERCE_SLOW_US: u64 = 1500;

/// 每完成一次 `Value::Object` → String 就调一次。
pub fn note_coerce(class: &str, out_len: usize, us: u64) {
    COERCE_N.fetch_add(1, Ordering::Relaxed);
    COERCE_US.fetch_add(us, Ordering::Relaxed);
    if us < COERCE_SLOW_US {
        return;
    }
    COERCE_SLOW_N.fetch_add(1, Ordering::Relaxed);
    COERCE_WORST_US.fetch_max(us, Ordering::Relaxed);

    if let Ok(mut top) = COERCE_TOP.try_lock() {
        if let Some(e) = top.iter_mut().find(|(n, _, _)| n == class) {
            e.1 += 1;
            e.2 += us;
        } else if top.len() < 8 {
            top.push((class.to_string(), 1, us));
        }
    }

    // 现场日志：最多 30 条、每秒 1 条，避免把日志刷爆。
    if COERCE_LOG_COUNT.load(Ordering::Relaxed) >= 30 {
        return;
    }
    let now = now_us();
    let last = COERCE_LAST_LOG_US.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 1_000_000 {
        return;
    }
    COERCE_LAST_LOG_US.store(now, Ordering::Relaxed);
    COERCE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        "SXDffle CoerceS 慢: 类={} · 输出 {}KB · 单次 {}ms · 累计慢 {} 次",
        class,
        out_len / 1024,
        us / 1000,
        COERCE_SLOW_N.load(Ordering::Relaxed)
    );
}

/// `(对象转换次数, 累计微秒, 慢次数, 单次最坏微秒)`。
pub fn coerce_stats() -> (u64, u64, u64, u64) {
    (
        COERCE_N.load(Ordering::Relaxed),
        COERCE_US.load(Ordering::Relaxed),
        COERCE_SLOW_N.load(Ordering::Relaxed),
        COERCE_WORST_US.load(Ordering::Relaxed),
    )
}

/// 按累计耗时降序取 CoerceS 慢点 TOP（取完即清）。
pub fn take_coerce_top() -> Vec<(String, u64, u64)> {
    let mut out = match COERCE_TOP.try_lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => Vec::new(),
    };
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out
}


// ---- Loader 生命周期（0.261138）----
//
// 卡在 96% 那几分钟里，`加载/卸载 调用` 涨了 4 次，而 `SWF 载入` 只涨了 1 次 ——
// 差的 3 次如果一直不回来，就是「进度条永远差最后几个百分点」的直接原因。
// 这里只数「启动」和「init 事件触发」，两者之差 = 还挂着的加载。
static LOADER_STARTED: AtomicU64 = AtomicU64::new(0);
static LOADER_INITED: AtomicU64 = AtomicU64::new(0);
static LOADER_LAST_START_US: AtomicU64 = AtomicU64::new(0);
static LOADER_LAST_URL: Mutex<String> = Mutex::new(String::new());

pub fn note_loader_start(url: &str) {
    LOADER_STARTED.fetch_add(1, Ordering::Relaxed);
    LOADER_LAST_START_US.store(now_us(), Ordering::Relaxed);
    if let Ok(mut g) = LOADER_LAST_URL.try_lock() {
        g.clear();
        g.push_str(url);
        if g.len() > 120 {
            g.truncate(120);
        }
    }
}

pub fn note_loader_init() {
    LOADER_INITED.fetch_add(1, Ordering::Relaxed);
}

/// `(启动数, 完成数, 最近启动距今毫秒, 最近启动的 URL)`
pub fn loader_stats() -> (u64, u64, u64, String) {
    let last = LOADER_LAST_START_US.load(Ordering::Relaxed);
    let idle = if last == 0 {
        0
    } else {
        now_us().saturating_sub(last) / 1000
    };
    let url = LOADER_LAST_URL
        .try_lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    (
        LOADER_STARTED.load(Ordering::Relaxed),
        LOADER_INITED.load(Ordering::Relaxed),
        idle,
        url,
    )
}
