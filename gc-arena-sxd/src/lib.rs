#![no_std]

#[cfg(feature = "std")]
extern crate std;

extern crate alloc;

pub mod arena;
pub mod barrier;
pub mod zst_cache;
#[macro_use]
pub mod collect;
mod collect_impl;
mod context;
pub mod dynamic_roots;
mod gc;
mod gc_weak;
pub mod lock;
pub mod metrics;
mod no_drop;
mod static_wrapper;
mod types;

/// SXDffle：GC 累计耗时（微秒）与次数，用来把「解释器慢」和「GC 慢」分开。
pub static GC_TIME_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static GC_RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// SXDffle：GcBox slab 从系统拿到的总字节。
pub fn slab_bytes() -> usize {
    types::slab_bytes()
}

/// (峰值, 已归还)：判断「GC 收完之后到底还给系统多少」。
pub fn slab_reclaim_stats() -> (usize, usize) {
    types::slab_reclaim_stats()
}

/// SXDffle：GC 堆字节统计，(累计分配, 累计释放, 当前存活对象数)。
pub fn heap_byte_stats() -> (usize, usize, usize) {
    types::heap_byte_stats()
}

/// SXDffle：存活对象按分配尺寸分桶（用来判断千万级对象到底是什么）。
pub use types::{SIZE_CLASSES, SIZE_CLASS_LABELS, SLAB_HOOK, live_size_histogram};
mod unsize;

#[cfg(feature = "enum-map")]
mod enum_map;

#[cfg(feature = "hashbrown")]
mod hashbrown;

#[cfg(feature = "indexmap")]
mod indexmap;

#[cfg(feature = "slotmap")]
mod slotmap;

#[cfg(feature = "smallvec")]
mod smallvec;

#[doc(hidden)]
pub use gc_arena_derive::__unelide_lifetimes;

#[doc(hidden)]
pub use self::{arena::__DynRootable, no_drop::__MustNotImplDrop, unsize::__CoercePtrInternal};

pub use self::{
    arena::{Arena, Rootable},
    collect::Collect,
    context::{Finalization, Mutation},
    dynamic_roots::{DynamicRoot, DynamicRootSet},
    gc::Gc,
    gc_weak::GcWeak,
    lock::{GcLock, GcRefLock, Lock, RefLock},
    static_wrapper::Static,
};
