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
pub const TAG_COUNT: usize = 8;

pub const NAMES: [&str; TAG_COUNT] = [
    "其他/未标注",
    "SWF解析",
    "位图解码",
    "AS3执行",
    "渲染",
    "音频",
    "网络",
    "字体",
];

thread_local! {
    static TAG: Cell<u8> = const { Cell::new(0) };
}

#[inline]
pub fn current() -> u8 {
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
