//! SXDffle：分配归属标签。
//!
//! 目的：把「分配器里那 800MB 到底是谁的」这件事量化。
//! 做法：在几个粗粒度的执行阶段设置线程局部标签，宿主的全局分配器在
//! `alloc/dealloc` 时读一下标签，就能得到「每个阶段的存活字节」。
//!
//! 为什么不用栈回溯：在 malloc 钩子里 unwind 有崩溃风险（真机验证成本高），
//! 而我们的可疑面只有几个（SWF 解析、位图解码、渲染、脚本、音频），
//! 打标签足够定位，且零风险。
//!
//! 用 `const` 初始化的 `Cell<u8>` 不会在 TLS 首次访问时分配，
//! 因此在分配器里调用是安全的。

use std::cell::Cell;

/// 标签数量（与宿主 `lib.rs` 的 TAG_BYTES 保持一致）。
///
/// 0.261121：从 8 扩到 13，新增 AS3 数组/位图/字节流/字串 四个细分标签
/// 和一个 **GC slab** 标签。后者是关键：slab 是在脚本执行中按需向系统
/// 要的 1MB 块，过去会被记成「AS3执行」，导致 0.261120 那 2GB 里
/// 有 700MB 其实是 GC 堆本身。
pub const TAG_COUNT: usize = 13;

/// GC slab 的专用标签。
pub const TAG_GC_SLAB: u8 = 12;

pub const NAMES: [&str; TAG_COUNT] = [
    "其他/未标注",
    "SWF解析",
    "位图解码",
    "AS3执行",
    "渲染",
    "音频",
    "网络",
    "字体",
    "AS3数组",
    "AS3位图",
    "AS3字节流",
    "AS3字串",
    "GC slab",
];

thread_local! {
    static TAG: Cell<u8> = const { Cell::new(0) };
    /// 本线程正在为 GC slab 向系统要内存。
    static IN_SLAB: Cell<bool> = const { Cell::new(false) };
}

fn slab_hook(enter: bool) {
    IN_SLAB.with(|c| c.set(enter));
}

/// 把「slab 分配中」的标记注入 gc-arena。
///
/// 必须是 gc-arena 在 `alloc` 前后回调我们，而不是我们自己去猜：
/// 全局开关会把**同一时刻别的线程**的分配也标成 slab（战报期间 SWF
/// 正在另一线程解压 36MB，被误标就毁了整张归因表）。
pub fn install_slab_hook() {
    use std::sync::atomic::Ordering;
    let _ = gc_arena::SLAB_HOOK.compare_exchange(
        0,
        slab_hook as usize,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
}

#[inline]
pub fn current() -> u8 {
    // slab 自己的 1MB 块单独成类：它由 GC 在任意阶段按需申请，
    // 若按当前阶段记账，会把「GC 堆有多大」混进「脚本分配了多少」。
    if IN_SLAB.with(|c| c.get()) {
        return TAG_GC_SLAB;
    }
    TAG.with(|c| c.get())
}

#[inline]
fn set(t: u8) -> u8 {
    TAG.with(|c| {
        let old = c.get();
        c.set(t);
        old
    })
}

/// 作用域守卫：构造时切换标签，析构时恢复。
pub struct Guard(u8);

impl Guard {
    #[inline]
    pub fn new(tag: u8) -> Self {
        Guard(set(tag))
    }
}

impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        let old = self.0;
        TAG.with(|c| c.set(old));
    }
}
