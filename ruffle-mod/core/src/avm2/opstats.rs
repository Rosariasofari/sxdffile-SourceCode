//! SXDffle：AVM2 字节码热点采样。
//!
//! 战报加载时 `update` 段单帧 1.4~2.2 秒（PC 上 JIT 只要 0.3 秒），
//! 优化解释器之前必须先知道**慢在哪种指令**上。
//!
//! 采样策略：每 4096 条指令记一次（约每秒万级样本，足够看分布），
//! 用 `Debug` 取指令名并截断到第一个字段，开销可忽略。
//! 解释器单线程，所以线程局部的 `RefCell<Vec>` 就够。

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    // (指令名, 采样次数, 累计耗时微秒)
    static COUNTS: RefCell<Vec<(String, u64, u64)>> = const { RefCell::new(Vec::new()) };
    static LAST: RefCell<Option<std::time::Instant>> = const { RefCell::new(None) };
}

/// 累计执行指令数（每采样一次 +4096，即采样步长）。
///
/// 光有「占比」不够：战报加载 10 分钟到底是**指令太多**还是**单条太慢**，
/// 必须用「指令数 ÷ 耗时」来判断。正常解释器 20~50ns/条，
/// 若实测 500ns/条，说明是 dispatch 或某个 API 的实现问题；
/// 若只是条目多到几十亿，那说明游戏本身的计算量就是这么大，得换思路。
static TOTAL_OPS: AtomicU64 = AtomicU64::new(0);

/// 采样间隔超过这个值就认为「解释器在等」（等 socket、等帧、等 IO），
/// 这段墙钟时间不属于任何一条指令，不能算进热点占比。
///
/// 0.261137 的日志里，卡在 96% 时 `CoerceS 1%/99%` 连续出现 —— 那是假象：
/// 应用 15fps 空转，每隔几十毫秒才执行几条指令，两次采样之间的空档
/// 全被记到了碰巧被采到的那条指令头上。
const IDLE_GAP_US: u64 = 200_000;

/// 被判定为「等」的累计时间（微秒）。
static IDLE_US: AtomicU64 = AtomicU64::new(0);

/// 空闲（等待）累计时间，宿主用来给热点百分比「去掉水分」。
pub fn idle_us() -> u64 {
    IDLE_US.load(Ordering::Relaxed)
}

/// 记录一条指令（已截断的变体名，如 `GetPropertyFast`）。
///
/// 同时记录「距上一次采样过了多久」，把它归给当前这条指令。
/// 这是找 `1897 ns/条` 那种异常值的唯一办法：次数占比说明不了问题
/// （`GetLocal` 占 25% 但每条只要几纳秒），**耗时占比**才能指出
/// 是哪条指令背后挂着几百毫秒的 native 工作（`Array.sort`、`BitmapData` 构造、
/// 字符串 O(n²) 拼接……）。
pub fn note(name: &str) {
    TOTAL_OPS.fetch_add(4096, Ordering::Relaxed);
    let now = std::time::Instant::now();
    let mut dt = LAST.with(|l| {
        let mut slot = l.borrow_mut();
        let dt = slot.map(|t| now.duration_since(t).as_micros() as u64).unwrap_or(0);
        *slot = Some(now);
        dt
    });
    if dt > IDLE_GAP_US {
        IDLE_US.fetch_add(dt, Ordering::Relaxed);
        dt = 0;
    }
    COUNTS.with(|c| {
        let mut v = c.borrow_mut();
        if let Some(e) = v.iter_mut().find(|(n, _, _)| n == name) {
            e.1 += 1;
            e.2 += dt;
        } else if v.len() < 96 {
            v.push((name.to_string(), 1, dt));
        }
    });
}

/// 累计执行的指令数。
pub fn total_ops() -> u64 {
    TOTAL_OPS.load(Ordering::Relaxed)
}

/// 取出计数并清零，按耗时降序返回 (指令名, 采样次数, 耗时微秒)。
pub fn take() -> Vec<(String, u64, u64)> {
    COUNTS.with(|c| {
        let mut v = std::mem::take(&mut *c.borrow_mut());
        v.sort_by(|a, b| b.2.cmp(&a.2));
        v
    })
}

/// 从 `Op` 的 Debug 输出里截出变体名。
#[inline]
pub fn variant_name(debug: &str) -> &str {
    let end = debug
        .find(|c: char| c == ' ' || c == '{' || c == '(' || c == ',')
        .unwrap_or(debug.len());
    &debug[..end]
}

/// 只读快照：按累计耗时降序取前 `n` 项，**不清零**。
///
/// 慢帧看门狗要的是「卡住的这一秒里，时间花在哪条指令上」，
/// 而 15 秒的常规统计是累计值，会把这些细节平均掉。
pub fn snapshot_top(n: usize) -> Vec<(String, u64, u64)> {
    COUNTS.with(|c| {
        let v = c.borrow();
        let mut out: Vec<(String, u64, u64)> = v.clone();
        out.sort_by(|a, b| b.2.cmp(&a.2));
        out.truncate(n);
        out
    })
}
