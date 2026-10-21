//! AVM2 对象分配统计（SXDffle 诊断用）。
//!
//! 页游看战报时短时间会 new 出上千万个对象。只看总数没法定位是谁，
//! 这里按**类名**做 1/256 采样：开销可以忽略（每 256 次分配才记一次），
//! 但足够看出"哪一类对象在被疯狂创建"。
//!
//! 注意：统计的是**创建次数**（累计），不是当前存活数。它回答的是
//! "谁在制造垃圾"，配合 total_gc_count 就能判断是"分配太快"还是"对象被引用住"。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 采样间隔：每 N 次分配记一次。
const SAMPLE: usize = 256;

static COUNTER: AtomicUsize = AtomicUsize::new(0);
/// ScriptObject 创建总数（精确，不做采样）。
static CREATED: AtomicUsize = AtomicUsize::new(0);
static STATS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

/// 记录一次对象创建，并返回本次是否命中采样。
///
/// 关键：调用方必须**先**调用本函数，命中采样后才去取类名并 `to_string()`。
/// 早期版本在调用前就构造了 String，导致每次 `new` 都分配一次字符串 ——
/// 战报一局上千万对象就是上千万次无谓分配，既慢又推高内存峰值。
#[inline]
pub fn tick() -> bool {
    CREATED.fetch_add(1, Ordering::Relaxed);
    COUNTER.fetch_add(1, Ordering::Relaxed) % SAMPLE == 0
}

/// 记录一次采样到的对象类名。
pub fn record(name: String) {
    let map = STATS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = map.lock() {
        *guard.entry(name).or_insert(0) += 1;
    }
}

/// ScriptObject 累计创建数（精确值）。
pub fn created_total() -> usize {
    CREATED.load(Ordering::Relaxed)
}

/// 取创建次数最多的前 `n` 个类（已按采样率放大估算）。
pub fn snapshot_top(n: usize) -> Vec<(String, u64)> {
    let Some(map) = STATS.get() else {
        return Vec::new();
    };
    let Ok(guard) = map.lock() else {
        return Vec::new();
    };
    let mut v: Vec<(String, u64)> = guard
        .iter()
        .map(|(k, v)| (k.clone(), v.saturating_mul(SAMPLE as u64)))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(n);
    v
}
