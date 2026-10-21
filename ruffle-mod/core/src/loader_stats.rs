//! SXDffle：资源加载/卸载行为统计。
//!
//! 目的只有一个：回答「游戏自己会不会卸载资源」。
//! 如果 `load` 有几百次而 `unload` 只有个位数，那内存只增不减就是游戏侧行为，
//! 引擎必须兜底（LRU / 场景切换回收）；反之则要查引擎的释放路径。
use std::sync::atomic::{AtomicUsize, Ordering};

static LOADS: AtomicUsize = AtomicUsize::new(0);
static LOAD_BYTES: AtomicUsize = AtomicUsize::new(0);
static UNLOADS: AtomicUsize = AtomicUsize::new(0);
static UNLOADS_WITH_CONTENT: AtomicUsize = AtomicUsize::new(0);
static UNLOAD_BYTES: AtomicUsize = AtomicUsize::new(0);
/// 卸载后真正回收掉的 MovieLibrary 个数。
static LIBRARIES_PRUNED: AtomicUsize = AtomicUsize::new(0);

pub fn note_library_pruned() {
    LIBRARIES_PRUNED.fetch_add(1, Ordering::Relaxed);
}

pub fn libraries_pruned() -> usize {
    LIBRARIES_PRUNED.load(Ordering::Relaxed)
}

pub fn note_load(bytes: usize) {
    LOADS.fetch_add(1, Ordering::Relaxed);
    LOAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub fn note_unload() {
    UNLOADS.fetch_add(1, Ordering::Relaxed);
}

/// 卸载时若确实有内容被移除。
pub fn note_unload_content(bytes: usize) {
    UNLOADS_WITH_CONTENT.fetch_add(1, Ordering::Relaxed);
    UNLOAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// 返回 (加载次数, 加载字节, 卸载次数, 有内容的卸载次数, 释放字节)。
pub fn stats() -> (usize, usize, usize, usize, usize) {
    (
        LOADS.load(Ordering::Relaxed),
        LOAD_BYTES.load(Ordering::Relaxed),
        UNLOADS.load(Ordering::Relaxed),
        UNLOADS_WITH_CONTENT.load(Ordering::Relaxed),
        UNLOAD_BYTES.load(Ordering::Relaxed),
    )
}
