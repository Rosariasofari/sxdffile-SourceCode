use alloc::alloc;
use core::{
    alloc::{Layout, LayoutError},
    cell::Cell,
    fmt,
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
};

use crate::{collect::Collect, context::Context};

// ---- SXDffle: GC 堆字节统计 ----
//
// 之前只统计"对象个数"，无法回答"8GB 到底是多少活对象、多少是分配器碎片"。
// 这里在分配/释放路径上累加真实申请字节（含 header），
// 用 live = alloc - free 就能精确归因。
use core::sync::atomic::{AtomicUsize, Ordering};

static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static FREE_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_OBJECTS: AtomicUsize = AtomicUsize::new(0);

// ---- SXDffle：存活对象按「单次分配尺寸」分桶 ----
//
// 战报加载时对象数从 168 万暴涨到 1946 万，但 GC 堆只涨 934MB（平均 52B/个），
// 而 RSS 涨了 4.1GB。要么是分配器碎片，要么是这批小对象另有来源。
// 只靠"总数"无法区分，必须知道**每个尺寸档有多少个存活对象**：
// 若集中在 48~64B，说明是 Gc 小包装（Value/RefLock/Cell）；
// 若集中在 200B 以上，说明是 MovieClip/Frame 这类结构。
pub const SIZE_CLASSES: usize = 16;

const fn size_class(n: usize) -> usize {
    match n {
        0..=24 => 0,
        25..=32 => 1,
        33..=40 => 2,
        41..=48 => 3,
        49..=56 => 4,
        57..=64 => 5,
        65..=80 => 6,
        81..=96 => 7,
        97..=128 => 8,
        129..=160 => 9,
        161..=192 => 10,
        193..=256 => 11,
        257..=384 => 12,
        385..=512 => 13,
        513..=1024 => 14,
        _ => 15,
    }
}

pub const SIZE_CLASS_LABELS: [&str; SIZE_CLASSES] = [
    "≤24", "32", "40", "48", "56", "64", "80", "96", "128", "160", "192", "256", "384", "512",
    "1K", ">1K",
];

static LIVE_BY_SIZE: [AtomicUsize; SIZE_CLASSES] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// 返回每个尺寸档的存活对象数（含 header 的 alloc_layout.size()）。
pub fn live_size_histogram() -> [usize; SIZE_CLASSES] {
    let mut out = [0usize; SIZE_CLASSES];
    for (i, v) in out.iter_mut().enumerate() {
        *v = LIVE_BY_SIZE[i].load(Ordering::Relaxed);
    }
    out
}

/// 返回 (累计分配字节, 累计释放字节, 当前存活对象数)。
pub fn heap_byte_stats() -> (usize, usize, usize) {
    (
        ALLOC_BYTES.load(Ordering::Relaxed),
        FREE_BYTES.load(Ordering::Relaxed),
        LIVE_OBJECTS.load(Ordering::Relaxed),
    )
}

// ---- SXDffle: GcBox 专用 slab 分配器 ----
//
// 为什么值得单独写一层：
//
// 战报加载时游戏在一帧内创建 2000 万级对象（GC 堆 1.2GB），而 mimalloc 的映射
// 却涨到 5.9GB —— 4.7 倍放大。原因是 GcBox 尺寸集中在 40~96B，mimalloc 按
// 4MB segment 分配，22M 个碎块散落在几十个 size class 里，每个 segment 都只
// 用了一小部分，剩下的全是内部碎片，而且爆发期间根本没机会归还。
//
// 这里改成「按 16B 对齐的 size class + 1MB slab 顺序 bump 分配 + 每类空闲链」：
//   - 分配：只做一次指针加法，比 malloc 快一个数量级（分配本身也是 CPU 热点）；
//   - 碎片：每类最多浪费不到 1 个 slab，63 个类合计 < 63MB；
//   - 释放：块回收到本类空闲链，不跨线程归还（GC 是单线程的）。
//
// 代价：slab 不会还给内核（进程内复用），所以「峰值过后 RSS 不回落」这件事
// 仍然要靠 GC + mi_collect 之外的手段解决；但峰值本身能低一个数量级。
const SLAB_BYTES: usize = 1 << 20; // 1MB
const CLASS_STEP: usize = 16;
const MAX_SMALL: usize = 1024;
const NUM_CLASSES: usize = MAX_SMALL / CLASS_STEP + 1;

struct SlabClass {
    free: *mut u8,
    bump: *mut u8,
    end: *mut u8,
}

impl SlabClass {
    const fn new() -> Self {
        Self {
            free: core::ptr::null_mut(),
            bump: core::ptr::null_mut(),
            end: core::ptr::null_mut(),
        }
    }
}

struct SlabTable(core::cell::UnsafeCell<[SlabClass; NUM_CLASSES]>);
// SAFETY: 所有访问都在 SLAB_LOCK 之内。
unsafe impl Sync for SlabTable {}

static SLABS: SlabTable =
    SlabTable(core::cell::UnsafeCell::new([const { SlabClass::new() }; NUM_CLASSES]));

static SLAB_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// SXDffle：告诉宿主分配器「这次 malloc 是 GC slab 自己要的」。
//
// 不标的话，slab 会按当时的执行阶段被打上 `AS3执行` 的标签，
// 于是一份 700MB 的 GC 堆会算进「AS3 大块」里，把归因彻底带偏
// （0.261120 那 2GB「AS3执行」里有 700MB 其实是 slab 本身）。
//
// gc-arena 是 no_std，用不了 `thread_local!`；而全局原子标记会让**同窗口内
// 其它线程**的分配被误标（战报期间 SWF 解压正在别的线程跑，一个 36MB 的块
// 被记成 slab 就毁了整张表）。所以这里只留一个函数指针，由宿主注入
// 它自己的线程局部标记。
pub static SLAB_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

#[inline]
fn slab_hook(enter: bool) {
    let f = SLAB_HOOK.load(core::sync::atomic::Ordering::Relaxed);
    if f != 0 {
        // SAFETY: 由宿主用 `install_slab_hook` 写入，签名固定为 `fn(bool)`。
        let f: fn(bool) = unsafe { core::mem::transmute::<usize, fn(bool)>(f) };
        f(enter);
    }
}

#[inline]
fn slab_lock() {
    use core::sync::atomic::Ordering;
    while SLAB_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

#[inline]
fn slab_unlock() {
    SLAB_LOCK.store(false, core::sync::atomic::Ordering::Release);
}

/// 小对象走 slab，大对象/超对齐仍交给系统分配器。
///
/// SXDffle 0.261121：**整块归还**。
///
/// 之前 slab 只从系统要、从不还，于是「884 万个临时对象」那一瞬间要来的
/// 700MB，在 GC 把它们全收走之后仍然挂在进程上 —— 这就是退回宗门后
/// 4.19GB 一分不降里最不可解释的那一块。
///
/// 做法：slab 一律 1MB 对齐，头部 64B 存 `live` 计数；某块被释放后
/// `live` 归零，说明这一整块 1MB 已经没有活对象，把它从空闲链里摘干净
/// 再还给系统。战报的对象是顺序 bump 出来的（先来的常驻、后到的都是
/// 临时数据），所以「整块变空」不是巧合，而是常态。
const SLAB_HDR: usize = 64;
const SLAB_MAGIC: u32 = 0x5344_534C; // "SDSL"

#[repr(C)]
struct SlabHdr {
    magic: u32,
    class: u32,
    live: u32,
    _pad: u32,
    /// 系统分配器给的原始地址（我们为了做地址对齐多要了 1MB）
    raw: usize,
}

#[inline]
fn hdr_of(ptr: *mut u8) -> *mut SlabHdr {
    (ptr as usize & !(SLAB_BYTES - 1)) as *mut SlabHdr
}

/// 多要 1MB 再做手工对齐：不依赖分配器支不支持 1MB 对齐。
/// 多出来的那半段不会被写，所以不占物理页。
#[inline]
unsafe fn slab_layout() -> Layout {
    unsafe { Layout::from_size_align_unchecked(SLAB_BYTES * 2, CLASS_STEP) }
}

#[inline]
unsafe fn gc_alloc(layout: Layout) -> *mut u8 {
    let size = layout.size();
    if size == 0 || size > MAX_SMALL || layout.align() > CLASS_STEP {
        return unsafe { alloc::alloc(layout) };
    }
    let block = (size + CLASS_STEP - 1) & !(CLASS_STEP - 1);
    let idx = block / CLASS_STEP;

    slab_lock();
    let slab = unsafe { &mut (*SLABS.0.get())[idx] };
    if !slab.free.is_null() {
        let p = slab.free;
        unsafe {
            slab.free = *(p as *mut *mut u8);
            (*hdr_of(p)).live += 1;
        }
        slab_unlock();
        return p;
    }
    if slab.bump.is_null() || unsafe { slab.bump.add(block) } > slab.end {
        let sl = unsafe { slab_layout() };
        slab_hook(true);
        let raw = unsafe { alloc::alloc(sl) };
        slab_hook(false);
        if raw.is_null() {
            slab_unlock();
            unsafe { alloc::handle_alloc_error(sl) };
        }
        let base =
            ((raw as usize + SLAB_BYTES - 1) & !(SLAB_BYTES - 1)) as *mut u8;
        unsafe {
            let h = base as *mut SlabHdr;
            (*h).magic = SLAB_MAGIC;
            (*h).class = idx as u32;
            (*h).live = 0;
            (*h)._pad = 0;
            (*h).raw = raw as usize;
        }
        SLAB_TOTAL.fetch_add(SLAB_BYTES, Ordering::Relaxed);
        SLAB_LIVE.fetch_add(SLAB_BYTES, Ordering::Relaxed);
        slab.bump = unsafe { base.add(SLAB_HDR) };
        slab.end = unsafe { base.add(SLAB_BYTES) };
    }
    let p = slab.bump;
    slab.bump = unsafe { slab.bump.add(block) };
    unsafe { (*hdr_of(p)).live += 1 };
    slab_unlock();
    p
}

/// 把一整块已经空了的 slab 从本 class 的空闲链里摘掉并还给系统。
///
/// 必须是「先摘链、后 free」：空闲链的 next 指针就写在块自己的前 8 字节里。
#[inline]
unsafe fn reclaim_slab(slab: &mut SlabClass, hdr: *mut SlabHdr) {
    let lo = hdr as usize;
    let hi = lo + SLAB_BYTES;
    let mut prev: *mut *mut u8 = &mut slab.free;
    let mut cur = slab.free;
    while !cur.is_null() {
        let a = cur as usize;
        let next = unsafe { *(cur as *mut *mut u8) };
        if a >= lo && a < hi {
            unsafe { *prev = next };
        } else {
            prev = cur as *mut *mut u8;
        }
        cur = next;
    }
    // bump 指在这块上且已经用尽，可以顺手复位
    if !slab.bump.is_null() && (slab.bump as usize & !(SLAB_BYTES - 1)) == lo {
        slab.bump = core::ptr::null_mut();
        slab.end = core::ptr::null_mut();
    }
    SLAB_LIVE.fetch_sub(SLAB_BYTES, Ordering::Relaxed);
    SLAB_RECLAIMED.fetch_add(SLAB_BYTES, Ordering::Relaxed);
    let raw = unsafe { (*hdr).raw } as *mut u8;
    unsafe { alloc::dealloc(raw, slab_layout()) };
}

#[inline]
unsafe fn gc_dealloc(ptr: *mut u8, layout: Layout) {
    let size = layout.size();
    if size == 0 || size > MAX_SMALL || layout.align() > CLASS_STEP {
        unsafe { alloc::dealloc(ptr, layout) };
        return;
    }
    let block = (size + CLASS_STEP - 1) & !(CLASS_STEP - 1);
    let idx = block / CLASS_STEP;
    slab_lock();
    let slab = unsafe { &mut (*SLABS.0.get())[idx] };
    unsafe { *(ptr as *mut *mut u8) = slab.free };
    slab.free = ptr;

    let hdr = hdr_of(ptr);
    let h = unsafe { &mut *hdr };
    if unsafe { h.magic } == SLAB_MAGIC {
        h.live = h.live.saturating_sub(1);
        if h.live == 0 {
            // 当前 bump 块如果还有余量，留着继续用，别来回申请
            let in_bump = !slab.bump.is_null()
                && (slab.bump as usize & !(SLAB_BYTES - 1)) == hdr as usize;
            if !in_bump || slab.bump >= slab.end {
                unsafe { reclaim_slab(slab, hdr) };
            }
        }
    }
    slab_unlock();
}

/// SXDffle：slab 从系统拿到过多少字节（只增不减，用于观察峰值）。
static SLAB_TOTAL: AtomicUsize = AtomicUsize::new(0);
/// 当前仍挂在进程上的 slab 字节。
static SLAB_LIVE: AtomicUsize = AtomicUsize::new(0);
/// 累计归还给系统的 slab 字节。
static SLAB_RECLAIMED: AtomicUsize = AtomicUsize::new(0);

/// 当前 slab 占用（活着的 1MB 块合计）。
pub fn slab_bytes() -> usize {
    // SLAB_LIVE 记的是「向系统要了多少字节」，每块是 2MB（含 1MB 对齐余量），
    // 对外一律给出逻辑占用（1MB/块），免得所有报表都要记得除 2。
    SLAB_LIVE.load(Ordering::Relaxed) / 2
}

/// (峰值, 已归还)：用来判断「GC 收完之后到底还给系统多少」。
pub fn slab_reclaim_stats() -> (usize, usize) {
    (
        SLAB_TOTAL.load(Ordering::Relaxed) / 2,
        SLAB_RECLAIMED.load(Ordering::Relaxed) / 2,
    )
}


/// A thin-pointer-sized pointer to a type-erased GC object.
///
/// Pointers to GC objects have the metadata required by the GC algorithm in the same allocation
/// *before* the stored pointer.
pub(crate) struct GcPtr<T: ?Sized = ()>(NonNull<T>);

impl<T: ?Sized> fmt::Debug for GcPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.as_ptr(), f)
    }
}

impl<T: ?Sized> Copy for GcPtr<T> {}

impl<T: ?Sized> Clone for GcPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'gc, T: Collect<'gc>> GcPtr<T> {
    /// Allocate a new GC value with a default header.
    ///
    /// # Panics
    ///
    /// Panics if there is no valid layout we can allocate.
    #[inline(always)]
    pub(crate) fn alloc(value: T) -> Self {
        unsafe {
            let gc_layout = GcLayout::new(Layout::new::<T>()).expect("no layout for GC allocation");
            let gc_header = GcHeader::new::<T>();

            let bytes = gc_alloc(gc_layout.alloc_layout);
            let Some(bytes) = NonNull::new(bytes) else {
                alloc::handle_alloc_error(gc_layout.alloc_layout);
            };

            let value_ptr = gc_layout.value_ptr::<T>(bytes);
            let header_ptr = GcLayout::header_ptr(value_ptr);
            debug_assert!(header_ptr.is_aligned() && value_ptr.is_aligned());

            ptr::write(header_ptr.as_ptr(), gc_header);
            ptr::write(value_ptr.as_ptr(), value);

            ALLOC_BYTES.fetch_add(gc_layout.alloc_layout.size(), Ordering::Relaxed);
            LIVE_OBJECTS.fetch_add(1, Ordering::Relaxed);
            LIVE_BY_SIZE[size_class(gc_layout.alloc_layout.size())].fetch_add(1, Ordering::Relaxed);

            GcPtr(value_ptr)
        }
    }
}

impl<T: ?Sized> GcPtr<T> {
    #[inline(always)]
    pub(crate) fn header(&self) -> &GcHeader {
        unsafe { GcLayout::header_ptr(self.0).as_ref() }
    }

    #[inline(always)]
    pub(crate) fn as_ptr(self) -> *const T {
        self.0.as_ptr()
    }

    /// # Safety
    ///
    /// The provided ptr must have come from `GcPtr::as_ptr`.
    #[inline(always)]
    pub(crate) unsafe fn from_ptr(p: *const T) -> Self {
        Self(unsafe { NonNull::new_unchecked(p as *mut T) })
    }

    #[inline(always)]
    pub(crate) fn cast<U>(self) -> GcPtr<U> {
        GcPtr(self.0.cast())
    }

    /// A convenience method to cast to `()`.
    #[inline(always)]
    pub(crate) fn erase(self) -> GcPtr {
        self.cast()
    }

    /// Returns true if two `GcPtr`s point to the same allocation.
    ///
    /// This function ignores the metadata of `dyn` pointers.
    #[inline(always)]
    pub(crate) fn addr_eq(self, other: GcPtr<T>) -> bool {
        ptr::addr_eq(self.as_ptr(), other.as_ptr())
    }

    /// Return a shared reference to the value.
    ///
    /// # Safety
    ///
    /// You must ensure that this pointer was not cast to a type incompatible with its allocated
    /// type and has not been dropped or deallocated.
    ///
    /// Additionally, the returned reference has an unbound lifetime so the returned reference must
    /// not be live when the value is dropped or the pointer deallocated.
    #[inline(always)]
    pub(crate) unsafe fn as_ref<'a>(self) -> &'a T {
        unsafe { self.0.as_ref() }
    }

    /// Traces the stored value.
    ///
    /// # Safety
    ///
    /// The value must not have been dropped and the pointer must not have been deallocated.
    #[inline(always)]
    pub(crate) unsafe fn trace_value(self, cc: &mut Context) {
        unsafe { (self.header().vtable().trace_value)(self.0.cast::<()>().as_ptr(), cc) }
    }

    /// Drops the stored value.
    ///
    /// # Safety:
    ///
    /// The value must not have been previously dropped and the pointer must not have been
    /// deallocated.
    #[inline(always)]
    pub(crate) unsafe fn drop_in_place(&mut self) {
        unsafe { (self.header().vtable().drop_value)(self.0.cast::<()>().as_ptr()) }
    }

    /// Deallocates the allocation this pointer points to.
    ///
    /// Failing to call `Self::drop_in_place` beforehand will cause the stored value to be leaked.
    ///
    /// # Safety:
    ///
    /// The pointer must not already be deallocated.
    #[inline(always)]
    pub(crate) unsafe fn dealloc(self) {
        let gc_layout = GcLayout::new(self.header().vtable().value_layout).unwrap();
        unsafe {
            let ptr = gc_layout.alloc_ptr(self.0).as_ptr();
            // SXDffle: 统计真实释放字节
            FREE_BYTES.fetch_add(gc_layout.alloc_layout.size(), Ordering::Relaxed);
            LIVE_OBJECTS.fetch_sub(1, Ordering::Relaxed);
            let _ = LIVE_BY_SIZE[size_class(gc_layout.alloc_layout.size())]
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(1))
                });
            // SAFETY: the pointer was allocated with this layout.
            gc_dealloc(ptr, gc_layout.alloc_layout);
        }
    }
}

pub(crate) struct GcHeader {
    /// The next element in the global linked list of allocated objects.
    next: Cell<Option<GcPtr>>,
    /// A custom virtual function table for handling type-specific operations.
    ///
    /// The lower bits of the pointer are used to store GC flags:
    /// - bits 0 & 1 for the current `GcColor`;
    /// - bit 2 for the `needs_trace` flag;
    /// - bit 3 for the `is_live` flag.
    tagged_vtable: Cell<*const CollectVtable>,
}

impl GcHeader {
    /// Create a new `GcHeader` with:
    /// 1) color set to `White`
    /// 2) `needs_trace` set to `false`
    /// 3) `is_live` set to `false`
    #[inline(always)]
    fn new<'gc, T: Collect<'gc>>() -> Self {
        // Helper trait to materialize vtables in static memory.
        trait HasCollectVtable {
            const VTABLE: CollectVtable;
        }

        impl<'gc, T: Collect<'gc>> HasCollectVtable for T {
            const VTABLE: CollectVtable = CollectVtable::vtable_for::<T>();
        }

        let vtable: &'static _ = &<T as HasCollectVtable>::VTABLE;

        Self {
            next: Cell::new(None),
            tagged_vtable: Cell::new(vtable as *const _),
        }
    }

    #[inline(always)]
    fn vtable(&self) -> &'static CollectVtable {
        let ptr = tagged_ptr::untag(self.tagged_vtable.get());
        // SAFETY:
        // - the pointer was properly untagged.
        // - the vtable is stored in static memory.
        unsafe { &*ptr }
    }

    /// Gets the next element in the global linked list of allocated objects.
    #[inline(always)]
    pub(crate) fn next(&self) -> Option<GcPtr> {
        self.next.get()
    }

    /// Sets the next element in the global linked list of allocated objects.
    #[inline(always)]
    pub(crate) fn set_next(&self, next: Option<GcPtr>) {
        self.next.set(next)
    }

    #[inline]
    pub(crate) fn color(&self) -> GcColor {
        match tagged_ptr::get::<0x3, _>(self.tagged_vtable.get()) {
            0x0 => GcColor::White,
            0x1 => GcColor::WhiteWeak,
            0x2 => GcColor::Gray,
            _ => GcColor::Black,
        }
    }

    #[inline]
    pub(crate) fn set_color(&self, color: GcColor) {
        self.tagged_vtable.update(|p| {
            tagged_ptr::set::<0x3, _>(
                p,
                match color {
                    GcColor::White => 0x0,
                    GcColor::WhiteWeak => 0x1,
                    GcColor::Gray => 0x2,
                    GcColor::Black => 0x3,
                },
            )
        });
    }

    #[inline]
    pub(crate) fn needs_trace(&self) -> bool {
        tagged_ptr::get_bool::<0x4, _>(self.tagged_vtable.get())
    }

    #[inline]
    pub(crate) fn set_needs_trace(&self, needs_trace: bool) {
        self.tagged_vtable
            .update(|p| tagged_ptr::set_bool::<0x4, _>(p, needs_trace));
    }

    /// Determines whether or not we've dropped the `dyn Collect` value stored in `GcPtr.value`
    ///
    /// When we garbage-collect a `GcPtr` that still has outstanding weak pointers, we set `alive`
    /// to false. When there are no more weak pointers remaining, we will deallocate the `GcPtr`,
    /// but skip dropping the `dyn Collect` value (since we've already done it).
    #[inline]
    pub(crate) fn is_live(&self) -> bool {
        tagged_ptr::get_bool::<0x8, _>(self.tagged_vtable.get())
    }

    #[inline]
    pub(crate) fn set_live(&self, alive: bool) {
        self.tagged_vtable
            .update(|p| tagged_ptr::set_bool::<0x8, _>(p, alive));
    }
}

/// Type-specific operations for GC'd values.
///
/// We use a custom vtable instead of `dyn Collect` for extra flexibility. The type is over-aligned
/// so that `GcHeader` can store flags into the LSBs of the vtable pointer.
#[repr(align(16))]
struct CollectVtable {
    /// Traces the value at the given pointer.
    trace_value: unsafe fn(*const (), &mut Context),
    /// Drops the value at the given pointer.
    drop_value: unsafe fn(*mut ()),
    /// The layout of the value stored in this `GcPtr`.
    value_layout: Layout,
}

impl CollectVtable {
    /// Makes a vtable for a known, `Sized` type.
    /// Because `T: Sized`, we can recover a typed pointer
    /// directly from the erased `GcPtr`.
    #[inline(always)]
    const fn vtable_for<'gc, T: Collect<'gc>>() -> Self {
        Self {
            trace_value: |ptr, cc| unsafe {
                ptr.cast::<T>().as_ref_unchecked().trace(cc);
            },
            drop_value: |ptr| unsafe {
                ptr::drop_in_place(ptr.cast::<T>());
            },
            value_layout: Layout::new::<T>(),
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum GcColor {
    /// An object that has not yet been reached by tracing (if we're in a tracing phase).
    ///
    /// During `Phase::Sweep`, we will free all white objects that existed *before* the start of the
    /// current `Phase::Sweep`. Objects allocated during `Phase::Sweep` will be white, but will not
    /// be freed.
    White,
    /// Like White, but for objects weakly reachable from a Black object.
    ///
    /// These objects may drop their contents during `Phase::Sweep`, but must stay allocated so that
    /// weak references can check the alive status.
    WhiteWeak,
    /// An object reachable from a Black object, but that has not yet been traced using
    /// `Collect::trace`. We also mark black objects as gray during `Phase::Mark` in response to
    /// a write barrier, so that we re-trace and find any objects newly reachable from the mutated
    /// object.
    Gray,
    /// An object that was reached during tracing. It will not be freed during `Phase::Sweep`. At
    /// the end of `Phase::Sweep`, all black objects will be reset to white.
    Black,
}

// Phantom type that holds a lifetime and ensures that it is invariant.
pub(crate) type Invariant<'a> = PhantomData<Cell<&'a ()>>;

/// The layout of an allocated buffer backing a `Gc` pointer.
#[derive(Debug, Copy, Clone)]
struct GcLayout {
    alloc_layout: Layout,
    value_offset: usize,
}

impl GcLayout {
    #[inline(always)]
    const fn new(value_layout: Layout) -> Result<Self, LayoutError> {
        let header_layout = Layout::new::<GcHeader>();
        match header_layout.extend(value_layout) {
            Ok((alloc_layout, value_offset)) => {
                // The header and value positions should be properly aligned.
                //
                // Value is obviuosly aligned to itself because the implementation of
                // `Layout::extend` ensures that it is.
                //
                // The position of the header (value_ptr - header_size) is also aligned to the
                // header because:
                // 1) If the header alignment is greater than the value alignment, then the value
                //    will fit right next to the header since the header's size is a multiple of its
                //    alignment and thus also the value alignment. Thus (value_ptr - header_size) is
                //    the beginning of the allocation which is already aligned to the header.
                // 2) If the value alignment is greater than the header alignment, then the value is
                //    also aligned to the header so (value_ptr - header_size) must be aligned to the
                //    header since the header size is a multiple of its alignment.

                // Assert that the above logic is true.
                debug_assert!(value_offset.is_multiple_of(value_layout.align()));
                debug_assert!(
                    (value_offset - header_layout.size()).is_multiple_of(header_layout.align())
                );

                Ok(Self {
                    alloc_layout,
                    value_offset,
                })
            }
            Err(err) => Err(err),
        }
    }

    /// Compute the pointer to the GC value from the pointer to the GC allocation.
    ///
    /// # Safety
    ///
    /// Allocation must have been allocated according to `alloc_layout`.
    #[inline(always)]
    unsafe fn value_ptr<T>(&self, alloc_ptr: NonNull<u8>) -> NonNull<T> {
        let value = unsafe { alloc_ptr.add(self.value_offset).cast() };
        debug_assert!(value.is_aligned());
        value
    }

    /// Compute the pointer to the GC header from a GC value pointer.
    ///
    /// # Safety
    ///
    /// Must be a valid GC value pointer.
    #[inline(always)]
    unsafe fn header_ptr<T: ?Sized>(value_ptr: NonNull<T>) -> NonNull<GcHeader> {
        let header = unsafe {
            value_ptr
                .cast::<GcHeader>()
                .byte_sub(mem::size_of::<GcHeader>())
        };
        debug_assert!(header.is_aligned());
        header
    }

    /// Compute the pointer to the beginning of the GC allocation from a GC value pointer.
    ///
    /// # Safety
    ///
    /// Must be a valid GC value pointer.
    #[inline(always)]
    unsafe fn alloc_ptr<T: ?Sized>(&self, value_ptr: NonNull<T>) -> NonNull<u8> {
        unsafe { value_ptr.cast::<u8>().byte_sub(self.value_offset) }
    }
}

/// Utility functions for tagging and untagging pointers.
mod tagged_ptr {
    /// Checks that `mask` can be used to tag a pointer to `T`.
    const fn is_valid_mask<T>(mask: usize) -> bool {
        mask < core::mem::align_of::<T>()
    }

    /// Checks that `mask` is exactly 1 bit wide.
    const fn is_boolean_mask(mask: usize) -> bool {
        mask.is_power_of_two()
    }

    trait ValidMask<const MASK: usize> {
        const CHECK: ();
    }

    impl<T, const MASK: usize> ValidMask<MASK> for T {
        const CHECK: () = assert!(is_valid_mask::<T>(MASK));
    }

    trait BooleanMask<const MASK: usize> {
        const CHECK: ();
    }

    impl<T, const MASK: usize> BooleanMask<MASK> for T {
        const CHECK: () = assert!(is_boolean_mask(MASK));
    }

    /// Checks that `$mask` can be used to tag a pointer to `$type`.
    ///
    /// If this isn't true, this macro will cause a post-monomorphization error.
    macro_rules! check_mask {
        ($type:ty, $mask:expr) => {
            let _ = <$type as ValidMask<$mask>>::CHECK;
        };
    }

    macro_rules! check_bool_mask {
        ($type:ty, $mask:expr) => {
            let _ = <$type as ValidMask<$mask>>::CHECK;
            let _ = <$type as BooleanMask<$mask>>::CHECK;
        };
    }

    #[inline(always)]
    pub(super) fn untag<T>(tagged_ptr: *const T) -> *const T {
        let mask = core::mem::align_of::<T>() - 1;
        tagged_ptr.map_addr(|addr| addr & !mask)
    }

    #[inline(always)]
    pub(super) fn get<const MASK: usize, T>(tagged_ptr: *const T) -> usize {
        check_mask!(T, MASK);
        tagged_ptr.addr() & MASK
    }

    pub(super) fn set<const MASK: usize, T>(tagged_ptr: *const T, tag: usize) -> *const T {
        check_mask!(T, MASK);
        tagged_ptr.map_addr(|addr| (addr & !MASK) | (tag & MASK))
    }

    #[inline(always)]
    pub(super) fn get_bool<const MASK: usize, T>(tagged_ptr: *const T) -> bool {
        check_bool_mask!(T, MASK);
        tagged_ptr.addr() & MASK != 0x0
    }

    #[inline(always)]
    pub(super) fn set_bool<const MASK: usize, T>(tagged_ptr: *const T, value: bool) -> *const T {
        check_bool_mask!(T, MASK);
        tagged_ptr.map_addr(|addr| (addr & !MASK) | if value { MASK } else { 0 })
    }
}
