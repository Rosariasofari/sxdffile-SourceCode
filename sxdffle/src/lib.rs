mod audio;
mod crash;
mod custom_event;
// 实验性：嵌入式宿主（网页原位播放 Flash），见 embed.rs 顶部说明
mod embed;
mod embed_jni;
mod java;
mod keycodes;
mod navigator;
mod trace;
mod ui;

/// 全局分配器：mimalloc。
///
/// 实测（0.261062 日志）：GC 堆存活 167MB，而进程 RSS 2043MB。
/// 也就是说 1.8GB 是「已释放但没还给系统」的空闲页 —— bionic 的
/// dlmalloc 在页游这种「几百万个 32~200B 小对象反复分配释放」的模式下
/// 会保留大量空洞。mimalloc 按 size-class 分配、整页归还，
/// 并且每个线程独立堆，避免多线程下的锁竞争与跨线程碎片。
// ---- 内测魔改版：全局分配器 + 按大小档位的精确记账 ----
//
// 之前所有"是谁吃了 1.2GB"的推断都失败了：
// GC 堆 190MB、SWF 解压 209MB、slots 3MB、数组 34MB、字符串 14MB ——
// 加起来不到 450MB，而 mimalloc 里躺着 2797MB。
//
// 与其继续猜结构，不如直接在分配入口记账：每次 alloc/dealloc 按 size class
// 累加，current = alloc - free 就是"此刻真实活着的字节"，再按档位一摊，
// 立刻能看出是"海量小对象"还是"少数巨块"。
//
// 这是纯 Rust 实现，不依赖任何外部符号（上一版调 mimalloc 的
// mi_heap_visit_blocks 在真机上启动即崩）。
const ALLOC_BUCKETS: usize = 10;

static BUCKET_BYTES: [std::sync::atomic::AtomicUsize; ALLOC_BUCKETS] = [
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
];
static BUCKET_COUNTS: [std::sync::atomic::AtomicUsize; ALLOC_BUCKETS] = [
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
];

// ---- 大块存活归因（≥64KB）----
//
// 按阶段的「累计分配」不能说明谁**占着**内存：AS3 里分配的对象是在 GC 阶段
// 释放的，减不回去。所以这里只对 ≥64KB 的块建一张定长表：
// 分配时登记 (ptr, size, tag)，释放时按 ptr 清除。查询时按 tag 求和，
// 得到的就是「此刻每个阶段还活着的大块字节」。
//
// 表操作是线性扫描（8192 项），但大块分配次数很少（一次运行几千次），
// 而且全程不分配、不加锁，放在 allocator 里是安全的。
const TAG_COUNT: usize = 12;

/// 每个阶段「此刻存活的 ≥64KB 字节」。由块头里的 tag 精确增减（见 BigHeader）。
static TAG_BYTES: [std::sync::atomic::AtomicUsize; TAG_COUNT] = [
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
];
// SXDffle：8192 槽 / 16 次探测撑不住战报的规模。
//
// 实测 0.261110：分配器自己统计 ≥64KB 存活 1878MB，而这张表只认出 103MB
// —— 探测 16 次就放弃，剩下的块静默没登记，"大块归属"那行也就失去意义。
// 32768 槽（约 800KB 静态内存）+ 96 次探测，命中率足以覆盖几万个并发大块。
const BIG_SLOTS: usize = 32768;

struct BigSlot {
    ptr: std::sync::atomic::AtomicUsize,
    size: std::sync::atomic::AtomicUsize,
    tag: std::sync::atomic::AtomicU8,
}

impl BigSlot {
    const fn new() -> Self {
        Self {
            ptr: std::sync::atomic::AtomicUsize::new(0),
            size: std::sync::atomic::AtomicUsize::new(0),
            tag: std::sync::atomic::AtomicU8::new(0),
        }
    }
}

static BIG_TABLE: [BigSlot; BIG_SLOTS] = [const { BigSlot::new() }; BIG_SLOTS];

/// 开放寻址哈希：从 `ptr` 的地址位取初始槽，线性探测。
///
/// **0.261094–0.261097 这里是「从头线性扫描 8192 项」**，每次 ≥64KB 分配
/// 都要做最多 8192 次原子 load —— 战报期间一帧几十万次大块分配，
/// 单这一项就能吃掉上百毫秒，而且它被算进了「AVM2 脚本耗时」，
/// 直接导致了 235ns/条 和 1753ns/条 那个 7 倍差距。
/// 换成哈希探测后平均 1~2 次就能命中，开销降到可忽略。
#[inline(always)]
fn big_slot_of(ptr: usize) -> usize {
    // malloc 返回的指针至少 16 字节对齐，右移 4 位做散列，取低位当索引。
    (ptr >> 4) & (BIG_SLOTS - 1)
}

#[inline]
fn big_register(ptr: usize, size: usize, tag: u8) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut i = big_slot_of(ptr);
    for _ in 0..96 {
        let cur = BIG_TABLE[i].ptr.load(Relaxed);
        if cur == 0 || cur == ptr {
            BIG_TABLE[i].size.store(size, Relaxed);
            BIG_TABLE[i].tag.store(tag, Relaxed);
            BIG_TABLE[i].ptr.store(ptr, Relaxed);
            return;
        }
        i = (i + 1) & (BIG_SLOTS - 1);
    }
}

#[inline]
fn big_unregister(ptr: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut i = big_slot_of(ptr);
    for _ in 0..96 {
        let cur = BIG_TABLE[i].ptr.load(Relaxed);
        if cur == ptr {
            BIG_TABLE[i].ptr.store(0, Relaxed);
            return;
        }
        if cur == 0 {
            return;
        }
        i = (i + 1) & (BIG_SLOTS - 1);
    }
}

/// 返回 (每个阶段的存活大块字节, 大块总字节)。
pub fn alloc_tag_stats() -> ([usize; TAG_COUNT], usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut out = [0usize; TAG_COUNT];
    let mut total = 0usize;
    for (i, v) in TAG_BYTES.iter().enumerate() {
        let b = v.load(Relaxed);
        out[i] = b;
        total += b;
    }
    (out, total)
}

pub const TAG_LABELS: [&str; TAG_COUNT] = [
    "其他",
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
];

pub const ALLOC_BUCKET_LABELS: [&str; ALLOC_BUCKETS] = [
    "≤64B", "65-256B", "257-1K", "1K-4K", "4K-16K", "16K-64K", "64K-256K", "256K-1M", "1M-8M",
    ">8M",
];

#[inline]
fn alloc_bucket(size: usize) -> usize {
    match size {
        0..=64 => 0,
        65..=256 => 1,
        257..=1024 => 2,
        1025..=4096 => 3,
        4097..=16384 => 4,
        16385..=65536 => 5,
        65537..=262144 => 6,
        262145..=1048576 => 7,
        1048577..=8388608 => 8,
        _ => 9,
    }
}

/// 返回每个档位的 (存活字节, 存活块数)。
pub fn alloc_bucket_stats() -> [(usize, usize); ALLOC_BUCKETS] {
    use std::sync::atomic::Ordering::Relaxed;
    let mut out = [(0usize, 0usize); ALLOC_BUCKETS];
    for i in 0..ALLOC_BUCKETS {
        out[i] = (
            BUCKET_BYTES[i].load(Relaxed),
            BUCKET_COUNTS[i].load(Relaxed),
        );
    }
    out
}

struct TrackingAlloc {
    // SXDffle：回到 mimalloc。
    //
    // 0.261080 时代做过一次 A/B，结论是「换成系统分配器 RSS 没明显下降」，
    // 但那是在**稳态**（160 万对象）测的。战报加载是另一回事：
    // 一次 preload 会 malloc 出 1800 万个独立小块（gc-arena 每个 Gc 一次 alloc），
    // 期间 RSS 涨 4.1GB，而 GC 堆只涨 934MB —— 4.4 倍放大全在分配器里。
    // bionic 的 scudo 在这种「每秒几十万次小块分配」下会留下大量半空 region；
    // mimalloc 的 size-class 页（64KB 一块、每页只放同尺寸）碎片率低一个量级。
    //
    // 注意：绝不能在 preload 期间调 mi_collect(true)，它会锁住所有线程堆，
    // 0.261075 的「加载卡 96%」就是这么来的。只在 onTrimMemory 里用。
    inner: mimalloc::MiMalloc,
}

// ---- 大块分配直方图 ----
//
// 64K 以上的分配只有约 1900 次，但合计 688MB。它们的「尺寸重复度」
// 能直接指认来源：若某个尺寸（如 2.6MB）出现几十次，那就是同一类结构。
static BIG_BLOCKS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

#[inline]
fn note_big(size: usize) {
    // 必须 try_lock：加锁/扩容本身可能触发分配，递归进来要能安全跳过。
    if let Ok(mut v) = BIG_BLOCKS.try_lock() {
        if let Some(e) = v.iter_mut().find(|e| e.0 == size) {
            e.1 += 1;
        } else if v.len() < 512 {
            v.push((size, 1));
        }
    }
}

/// 释放时扣减，保证 TOP 表是「此刻存活的大块」，而不是累计分配次数。
///
/// 0.261083 之前这里没有扣减，日志里的「64K+ 共 688MB」其实是累计值，
/// 会把人误导到「有 688MB 大块永不释放」的结论上。
#[inline]
fn note_big_free(size: usize) {
    if let Ok(mut v) = BIG_BLOCKS.try_lock() {
        if let Some(e) = v.iter_mut().find(|e| e.0 == size) {
            e.1 = e.1.saturating_sub(1);
            if e.1 == 0 {
                v.retain(|e| e.0 != size);
            }
        }
    }
}

/// 返回按字节排序的大块尺寸 TOP N：(尺寸, 存活块数)。
pub fn big_alloc_top(n: usize) -> Vec<(usize, usize)> {
    let mut v = match BIG_BLOCKS.try_lock() {
        Ok(g) => g.clone(),
        Err(_) => return Vec::new(),
    };
    v.sort_by(|a, b| (b.0 * b.1).cmp(&(a.0 * a.1)));
    v.truncate(n);
    v
}

// ---- SXDffle：≥64KB 的块用 16 字节头部携带「阶段标签」 ----
//
// 之前用固定表 (ptr → tag) 记录，但开放寻址在删除后会把探测链打断：
// 后面本该被移除的条目变成永久垃圾，表被撑满后新块再也登记不进去。
// 结果是 `大块归属` 长期停在 ~800MB，而分配器里实际有 2.8GB 的 ≥64KB 块 ——
// 归因表本身成了盲区。
//
// 改成「分配时多要 16 字节、把 tag 写在块头」：
//   - 释放时读块头就能精确扣减，不再依赖任何表；
//   - 只对 size ≥ 64KB 且 align ≤ 16 的块生效（其余走原路径）；
//   - realloc 时若跨过 64KB 边界，改成 alloc+copy+free，语义等价。
const BIG_HEADER: usize = 16;

#[inline]
fn big_header_ok(size: usize, align: usize) -> bool {
    size >= 65536 && align <= BIG_HEADER
}

fn note_alloc(ptr: *mut u8, size: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let b = alloc_bucket(size);
    BUCKET_BYTES[b].fetch_add(size, Relaxed);
    BUCKET_COUNTS[b].fetch_add(1, Relaxed);
    if size >= 65536 {
        big_register(ptr as usize, size, ruffle_core::alloc_tag::current());
        // 直方图（Mutex + 512 项线性查找）只对 ≥1MB 的块维护。
        // 64KB~1MB 这一档一帧能有几万次，为了一张 TOP 表付这个代价不值。
        if size >= 1048576 {
            note_big(size);
        }
    }
}

#[inline]
fn note_free(ptr: *mut u8, size: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let b = alloc_bucket(size);
    if size >= 65536 {
        big_unregister(ptr as usize);
    }
    if size >= 1048576 {
        note_big_free(size);
    }
    // saturating：dealloc 的 layout 理论上与 alloc 相同，但 realloc 边界情况
    // 可能让计数对不齐，这里宁可少算也不要下溢。
    let _ = BUCKET_BYTES[b].fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(size)));
    let _ = BUCKET_COUNTS[b].fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1)));

}

unsafe impl std::alloc::GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if big_header_ok(layout.size(), layout.align()) {
            let with = unsafe {
                std::alloc::Layout::from_size_align_unchecked(
                    layout.size() + BIG_HEADER,
                    layout.align(),
                )
            };
            let base = unsafe { self.inner.alloc(with) };
            if base.is_null() {
                return base;
            }
            let tag = ruffle_core::alloc_tag::current() as usize;
            unsafe { *base = tag as u8 };
            if tag < TAG_COUNT {
                TAG_BYTES[tag].fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
            }
            let p = unsafe { base.add(BIG_HEADER) };
            note_alloc(p, layout.size());
            return p;
        }
        let p = unsafe { self.inner.alloc(layout) };
        if !p.is_null() {
            note_alloc(p, layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        if big_header_ok(layout.size(), layout.align()) {
            // 注意：不能直接复用 alloc —— 它拿到的是未初始化内存，
            // alloc_zeroed 的契约要求用户区全零，这里必须自己清零。
            let p = unsafe { self.alloc(layout) };
            if !p.is_null() {
                unsafe { std::ptr::write_bytes(p, 0, layout.size()) };
            }
            return p;
        }
        let p = unsafe { self.inner.alloc_zeroed(layout) };
        if !p.is_null() {
            note_alloc(p, layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        note_free(ptr, layout.size());
        if big_header_ok(layout.size(), layout.align()) {
            let base = unsafe { ptr.sub(BIG_HEADER) };
            let tag = unsafe { *base } as usize;
            if tag < TAG_COUNT {
                let _ = TAG_BYTES[tag].fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |v| Some(v.saturating_sub(layout.size())),
                );
            }
            let with = unsafe {
                std::alloc::Layout::from_size_align_unchecked(
                    layout.size() + BIG_HEADER,
                    layout.align(),
                )
            };
            unsafe { self.inner.dealloc(base, with) };
            return;
        }
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    unsafe fn realloc(
        &self,
        ptr: *mut u8,
        layout: std::alloc::Layout,
        new_size: usize,
    ) -> *mut u8 {
        let old_big = big_header_ok(layout.size(), layout.align());
        let new_big = big_header_ok(new_size, layout.align());
        if !old_big && !new_big {
            let p = unsafe { self.inner.realloc(ptr, layout, new_size) };
            if !p.is_null() {
                note_free(ptr, layout.size());
                note_alloc(p, new_size);
            }
            return p;
        }
        // 跨过 64KB 边界（或一直在两侧）时，走 alloc+copy+free，保证头部语义一致。
        let new_layout = unsafe {
            std::alloc::Layout::from_size_align_unchecked(new_size, layout.align())
        };
        let np = unsafe { self.alloc(new_layout) };
        if np.is_null() {
            return np;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(ptr, np, layout.size().min(new_size));
        }
        unsafe { self.dealloc(ptr, layout) };
        np
    }
}

#[global_allocator]
static GLOBAL_ALLOC: TrackingAlloc = TrackingAlloc {
    inner: mimalloc::MiMalloc,
};

use custom_event::RuffleEvent;

use jni::{
    errors::ThrowRuntimeExAndDefault,
    jni_sig, jni_str,
    objects::{JObject, JString},
    sys::{self, jint, jobject},
    Env, EnvUnowned, JavaVM,
};
use keycodes::{android_key_event_to_ruffle_key_descriptor, key_tag_to_key_descriptor};
use std::any::Any;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{mpsc, MutexGuard};
use std::time::Duration;
use std::{
    panic,
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};
use wgpu::rwh::{AndroidDisplayHandle, HasWindowHandle, RawDisplayHandle};

use android_activity::input::{InputEvent, KeyAction, MotionAction};
use android_activity::{AndroidApp, AndroidAppWaker, InputStatus, MainEvent, PollEvent};
use backtrace::Backtrace;
use jni::objects::JClass;

use audio::AAudioAudioBackend;
use url::Url;

use ruffle_common::duration::FloatDuration;
use ruffle_core::font::DefaultFont;
use ruffle_core::{
    backend::navigator::OwnedFuture,
    events::{LogicalKey, MouseButton, PlayerEvent},
    tag_utils::SwfMovieData,
    Player, PlayerBuilder, ViewportDimensions,
};
use ruffle_frontend_utils::backends::storage::DiskStorageBackend;
use ruffle_frontend_utils::content::PlayingContent;
use ruffle_frontend_utils::{
    backends::navigator::{ExternalNavigatorBackend, FutureSpawner},
    content::ContentDescriptor,
};

use crate::navigator::AndroidNavigatorInterface;
use crate::trace::FileLogBackend;
use crate::ui::AndroidUiBackend;
use java::JavaInterface;
use ruffle_render_wgpu::{backend::WgpuRenderBackend, target::SwapChainTarget};

/// A unique identifier for a given `Player` instance.
/// Used to track which player any currently executing future is bound to.
#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) struct PlayerId(pub(crate) i64);

impl PlayerId {
    pub(crate) fn new() -> Self {
        use std::sync::atomic::{AtomicI64, Ordering};

        static NEXT: AtomicI64 = AtomicI64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(id >= 0, "PlayerId overflowed!");
        Self(id)
    }
}

/// A `Player`-bound future that is currently running.
pub struct PlayerRunnable(async_task::Runnable<PlayerId>);

impl PlayerRunnable {
    /// 把裸 `Runnable` 包成 `PlayerRunnable`（供嵌入式宿主自定义调度器使用）。
    pub(crate) fn from_runnable(runnable: async_task::Runnable<PlayerId>) -> Self {
        Self(runnable)
    }

    /// 在宿主线程上轮询这个 future。
    ///
    /// 全屏版直接访问 `.0.run()`（同模块），嵌入式宿主在另一个模块里，
    /// 这里补一个方法避免把字段可见性放开。
    pub(crate) fn run(self) {
        self.0.run();
    }
}

/// Represents a current Player and any associated state with that player,
/// which may be lost when this Player is closed (dropped)
struct ActivePlayer {
    id: PlayerId,
    player: Arc<Mutex<Player>>,
}

#[derive(Clone)]
pub struct EventSender {
    sender: Sender<RuffleEvent>,
    waker: AndroidAppWaker,
}

impl EventSender {
    pub fn send(&self, event: RuffleEvent) {
        if self.sender.send(event).is_ok() {
            self.waker.wake();
        }
    }
}

/// A bare-bones executor that schedules tasks on the winit event loop.
struct AndroidExecutor {
    event_loop: EventSender,
    player_id: PlayerId,
}

impl<E: std::error::Error + 'static> FutureSpawner<E> for AndroidExecutor {
    fn spawn(&self, future: OwnedFuture<(), E>) {
        // Discard any errors.
        let future = async {
            if let Err(e) = future.await {
                tracing::error!("Async error: {}", e);
            }
        };

        let event_loop = self.event_loop.clone();
        let scheduler = move |task| {
            let event = RuffleEvent::TaskPoll(PlayerRunnable(task));
            event_loop.send(event)
        };

        let (runnable, task) = async_task::Builder::new()
            .metadata(self.player_id)
            .spawn_local(|_| future, scheduler);

        // The future should run in the background.
        task.detach();
        // Immediately schedule the future to be polled for the first time.
        runnable.schedule();
    }
}

#[tokio::main]
async fn run(app: AndroidApp) {
    let mut last_frame_time = Instant::now();
    let mut next_frame_time = Some(Instant::now());
    let mut quit = false;
    let (sender, receiver) = mpsc::channel::<RuffleEvent>();
    let mut native_window: Option<ndk::native_window::NativeWindow> = None;
    let mut playerbox: Option<ActivePlayer> = None;
    let sender = EventSender {
        sender,
        waker: app.create_waker(),
    };

    log::info!("Starting event loop...");
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut sys::JavaVM) };
    // This is a global reference, so it can only be borrowed, not wrapped as a local one.
    let activity_raw = app.activity_as_ptr() as jobject;

    let (trace_output, android_storage_dir) = vm
        .attach_current_thread(|env| -> jni::errors::Result<_> {
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw) }?;
            let trace_output = JavaInterface::get_trace_output(env, &activity);
            let android_storage_dir = JavaInterface::get_android_data_storage_dir(env, &activity);
            let _ = unsafe {
                env.set_rust_field(&*activity, jni_str!("eventLoopHandle"), sender.clone())
            };
            // Lets reqwest verify server certificates using Android's trust store.
            let context = env.new_local_ref(&*activity)?;
            rustls_platform_verifier::android::init_with_env(env, context)?;
            Ok((trace_output, android_storage_dir))
        })
        .expect("JNI calls on the main thread must succeed");

    while !quit {
        let mut needs_redraw = false;
        app.poll_events(
            Some(
                next_frame_time
                    .and_then(|next| next.checked_duration_since(last_frame_time))
                    .unwrap_or_else(|| Duration::from_millis(100)),
            ),
            |event| {
                match event {
                    PollEvent::Main(event) => match event {
                        MainEvent::Destroy => {
                            if let Some(player) = playerbox.as_ref() {
                                let mut player_lock = player.player.lock().unwrap();
                                player_lock.flush_shared_objects();
                            }
                            quit = true;
                        }
                        MainEvent::WindowResized { .. } => {
                            if let Some(player) = playerbox.as_ref() {
                                let mut player_lock = player.player.lock().unwrap();
                                let window = native_window
                                    .as_ref()
                                    .expect("native_window should be Some for a WindowResized");
                                log::info!(
                                    "WindowResized: {} x {}",
                                    window.width(),
                                    window.height()
                                );
                                let viewport_scale_factor = app
                                    .config()
                                    .density()
                                    .map(|dpi| dpi as f64 / 160.0)
                                    .unwrap_or(1.0);
                                let dimensions = ViewportDimensions {
                                    width: window.width() as u32,
                                    height: window.height() as u32,
                                    scale_factor: viewport_scale_factor,
                                };
                                player_lock.set_viewport_dimensions(dimensions);
                                needs_redraw = true;
                            }
                        }
                        MainEvent::Resume { .. } => {
                            if let Some(player) = playerbox.as_ref() {
                                if let Some(window) = native_window.as_ref() {
                                    // [NA] For some reason we can get negative sizes during a resume...
                                    if window.width() > 0 && window.height() > 0 {
                                        unsafe {
                                            let mut player = player
                                                .player
                                                .lock()
                                                .unwrap();

                                            let renderer = <dyn Any>::downcast_mut::<WgpuRenderBackend<SwapChainTarget>>(
                                                player.renderer_mut(),
                                            )
                                            .unwrap();

                                            renderer.recreate_surface_unsafe(
                                                wgpu::SurfaceTargetUnsafe::RawHandle {
                                                    raw_display_handle:
                                                        Some(RawDisplayHandle::Android(
                                                            AndroidDisplayHandle::new(),
                                                        )),
                                                    raw_window_handle: window
                                                        .window_handle()
                                                        .unwrap()
                                                        .into(),
                                                },
                                                (window.width() as u32, window.height() as u32),
                                            )
                                            .unwrap();
                                        }
                                    }
                                }
                            }
                        }
                        MainEvent::InitWindow { .. } => {
                            native_window = app.native_window();
                            let window = native_window
                                .as_ref()
                                .expect("native_window should be Some after InitWindow");
                            let viewport_scale_factor = app
                                .config()
                                .density()
                                .map(|dpi| dpi as f64 / 160.0)
                                .unwrap_or(1.0);
                            let dimensions = ViewportDimensions {
                                width: window.width() as u32,
                                height: window.height() as u32,
                                scale_factor: viewport_scale_factor,
                            };
                            log::info!(
                                "Init window: {} x {} (is existing: {})",
                                window.width(),
                                window.height(),
                                playerbox.is_some()
                            );

                            if let Some(activeplayer) = &playerbox {
                                let mut player_lock = activeplayer.player.lock().unwrap();
                                unsafe {
                                    let renderer = <dyn Any>::downcast_mut::<WgpuRenderBackend<SwapChainTarget>>(
                                        player_lock.renderer_mut(),
                                    )
                                    .unwrap();

                                    renderer.recreate_surface_unsafe(
                                        wgpu::SurfaceTargetUnsafe::RawHandle {
                                            raw_display_handle: Some(RawDisplayHandle::Android(
                                                AndroidDisplayHandle::new(),
                                            )),
                                            raw_window_handle: window
                                                .window_handle()
                                                .unwrap()
                                                .into(),
                                        },
                                        (window.width() as u32, window.height() as u32),
                                    )
                                    .unwrap();
                                }
                                player_lock.set_is_playing(true);
                            } else {
                                let renderer = unsafe {
                                    // TODO: make this take an Arc<Window> instead?
                                    WgpuRenderBackend::for_window_unsafe(
                                        wgpu::SurfaceTargetUnsafe::RawHandle {
                                            raw_display_handle: Some(RawDisplayHandle::Android(
                                                AndroidDisplayHandle::new(),
                                            )),
                                            raw_window_handle: window
                                                .window_handle()
                                                .unwrap()
                                                .into(),
                                        },
                                        (dimensions.width, dimensions.height),
                                        wgpu::Backends::GL,
                                        wgpu::PowerPreference::HighPerformance,
                                        None,
                                    )
                                    .unwrap()
                                };
                                let movie_url = Url::parse("file://movie.swf").unwrap();
                                let player_id = PlayerId::new();

                                let future_spawner = AndroidExecutor {
                                    event_loop: sender.clone(),
                                    player_id,
                                };

                                // ---- SXDffle 定制：把浏览器侧的登录态与防盗链信息注入 Flash 网络栈 ----
                                // referer：页游服务器普遍用它做防盗链校验，缺失会导致资源 403。
                                // cookie ：把 WebView 里"网页登录"得到的会话带入 Flash 的请求，
                                //          这是「先在网页登录、再进游戏」这条链路能打通的关键。
                                let (referer, cookie) = with_activity(&app, |env, activity| {
                                    (
                                        JavaInterface::get_referer(env, activity),
                                        JavaInterface::get_cookie(env, activity),
                                    )
                                })
                                .unwrap_or((None, None));
                                let referer_url =
                                    referer.as_deref().and_then(|r| Url::parse(r).ok());
                                log::info!(
                                    "SXDffle net: referer={:?}, cookie_len={}",
                                    referer_url,
                                    cookie.as_ref().map(|c| c.len()).unwrap_or(0)
                                );

                                let navigator = ExternalNavigatorBackend::new(
                                    movie_url.clone(),
                                    referer_url,
                                    cookie,
                                    future_spawner,
                                    None,
                                    true,
                                    Default::default(),
                                    ruffle_core::backend::navigator::SocketMode::Allow,
                                    Rc::new(PlayingContent::DirectFile(ContentDescriptor::new_remote(movie_url))),
                                    AndroidNavigatorInterface,
                                );

                                playerbox = Some(ActivePlayer {
                                    id: player_id,
                                    player: PlayerBuilder::new()
                                            // SXDffle：接入 Android 设备字体后端，
                                            // 否则中文页游的设备字体无法加载，中文会变成方块
                                            .with_ui(AndroidUiBackend::new())
                                            .with_renderer(renderer)
                                            .with_audio(AAudioAudioBackend::new().unwrap())
                                            .with_storage(Box::new(DiskStorageBackend::new(android_storage_dir.clone())))
                                            .with_navigator(navigator)
                                            .with_log(FileLogBackend::new(trace_output.as_deref()))
                                            .with_video(
                                                ruffle_video_software::backend::SoftwareVideoBackend::new(),
                                            )
                                        .build(),
                                    }
                                );

                                let player = &playerbox.as_ref().unwrap().player;
                                let mut player_lock = player.lock().unwrap();

                                // ---- SXDffle 定制：配置默认字体链 ----
                                // SWF 里的 `_sans` / `_serif` / `_typewriter` 是通用字体别名，
                                // 引擎会用这里的列表去系统里找实际字体。
                                // 上游未配置，落回内置的拉丁字体，中文（CJK）没有字形 → 显示为方块。
                                // 这里优先指定 Android 上常见的中文字体族。
                                player_lock.set_default_font(
                                    DefaultFont::Sans,
                                    vec![
                                        "Noto Sans CJK SC".into(),
                                        "Source Han Sans SC".into(),
                                        "Source Han Sans CN".into(),
                                        "Noto Sans SC".into(),
                                        "Droid Sans Fallback".into(),
                                        "Noto Sans".into(),
                                        "Roboto".into(),
                                    ],
                                );
                                player_lock.set_default_font(
                                    DefaultFont::Serif,
                                    vec![
                                        "Noto Serif CJK SC".into(),
                                        "Source Han Serif SC".into(),
                                        "Noto Serif".into(),
                                    ],
                                );
                                player_lock.set_default_font(
                                    DefaultFont::Typewriter,
                                    vec!["Droid Sans Mono".into(), "Noto Sans Mono".into()],
                                );
                                let (url, bytes, flash_vars) = with_activity(&app, |env, activity| {
                                    (
                                        JavaInterface::get_swf_uri(env, activity),
                                        JavaInterface::get_swf_bytes(env, activity),
                                        JavaInterface::get_flash_vars(env, activity),
                                    )
                                })
                                .unwrap();

                                // ---- SXDffle 定制：把 flashVars 交给引擎 ----
                                // 页游的启动参数全部走 flashVars（《神仙道》有 170+ 项，
                                // 含 ip/port/hash_code 等），缺失会导致游戏初始化失败。
                                //
                                // 这里用 `form_urlencoded::parse` 解析：它按 '&' 切分并对
                                // name/value 做 URL 解码（'+' → 空格，'%XX' → 字符），
                                // 与 Flash Player 处理 flashvars 的语义一致。
                                // 若不解码，游戏会拿到含 '%' 的原始串并在 JSON.parse 处报错。
                                let parameters: Vec<(String, String)> = flash_vars
                                    .as_deref()
                                    .map(|s| {
                                        url::form_urlencoded::parse(s.as_bytes())
                                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                log::info!("SXDffle flashVars: 解析出 {} 项", parameters.len());

                                if let Some(bytes) = bytes {
                                    let movie = SwfMovieData::from_data(&bytes, url, None, None).unwrap();
                                    player_lock.mutate_with_update_context(|context| {
                                        context.set_root_movie(movie);
                                    });
                                } else {
                                    player_lock.fetch_root_movie(url, parameters, Box::new(|_| {}))
                                }
                                player_lock.set_is_playing(true); // Desktop player will auto-play.

                                player_lock.set_letterbox(ruffle_core::config::Letterbox::On);

                                player_lock.set_viewport_dimensions(dimensions);

                                last_frame_time = Instant::now();
                                next_frame_time = Some(Instant::now());

                                log::info!("MOVIE STARTED");
                            }
                        }
                        MainEvent::TerminateWindow { .. }  => {
                            let player = &playerbox.as_ref().unwrap().player;
                            let mut player_lock = player.lock().unwrap();
                            player_lock.set_is_playing(false);
                        }
                        MainEvent::InputAvailable => {
                            if let Ok(mut inputs) = app.input_events_iter() {
                                while inputs.next(|input| match input {
                                    InputEvent::MotionEvent(event) => {
                                        let window = native_window.as_ref().unwrap();
                                        let pointer = event.pointer_index();
                                        let pointer = event.pointer_at_index(pointer);
                                        let coords: (i32, i32) = get_loc_in_window(&app);
                                        let mut x = pointer.x() as f64 - coords.0 as f64;
                                        let mut y = pointer.y() as f64 - coords.1 as f64;
                                        let view_size = get_view_size(&app).unwrap();
                                        x = x * window.width() as f64 / view_size.0 as f64;
                                        y = y * window.height() as f64 / view_size.1 as f64;
                                        let ruffle_event = match event.action() {
                                            MotionAction::Down | MotionAction::PointerDown | MotionAction::ButtonPress => {
                                                PlayerEvent::MouseDown {
                                                    x,
                                                    y,
                                                    button: MouseButton::Left, // TODO
                                                    index: None, // TODO
                                                }
                                            }
                                            MotionAction::Up | MotionAction::PointerUp | MotionAction::ButtonRelease => {
                                                PlayerEvent::MouseUp {
                                                    x,
                                                    y,
                                                    button: MouseButton::Left, // TODO
                                                }
                                            }
                                            MotionAction::Move => PlayerEvent::MouseMove { x, y },
                                            _ => return InputStatus::Unhandled,
                                        };

                                        if let Some(player) = playerbox.as_ref() {
                                            player
                                                .player
                                                .lock()
                                                .unwrap()
                                                .handle_event(ruffle_event);
                                        }

                                        InputStatus::Handled
                                    }
                                    InputEvent::KeyEvent(event) => {
                                        if let Some(player) = playerbox.as_ref() {
                                            let Some(key_descriptor) =
                                                android_key_event_to_ruffle_key_descriptor(event)
                                            else {
                                                return InputStatus::Unhandled;
                                            };
                                            let down;
                                            let ruffle_event = match event.action() {
                                                KeyAction::Down => {
                                                    down = true;
                                                    PlayerEvent::KeyDown {
                                                        key: key_descriptor,
                                                    }
                                                }
                                                KeyAction::Up => {
                                                    down = false;
                                                    PlayerEvent::KeyUp { key: key_descriptor }
                                                }
                                                _ => return InputStatus::Unhandled,
                                            };
                                            player
                                                .player
                                                .lock()
                                                .unwrap()
                                                .handle_event(ruffle_event);

                                            // TODO: Use `KeyEvent.unicode_char` when it's available:
                                            // https://github.com/rust-mobile/android-activity/issues/183
                                            if down {
                                                if let LogicalKey::Character(c) = key_descriptor.logical_key {
                                                    let event = PlayerEvent::TextInput { codepoint: c };
                                                    player.player.lock().unwrap().handle_event(event);
                                                }
                                            };

                                            needs_redraw = true;
                                        }

                                        InputStatus::Handled
                                    }
                                    _ => InputStatus::Unhandled,
                                }) {}
                            }
                        }
                        _ => {} // Something else happened but it's probably not important for now.
                    },
                    PollEvent::Wake => {} // A task tried to wake us, we'll recv it below
                    PollEvent::Timeout => {} // No events happened, we'll tick as normal below
                    _ => {}               // Unknown future event
                }
            },
        );

        match receiver.try_recv() {
            Err(_) => {}
            Ok(RuffleEvent::TaskPoll(task)) => {
                // Only run the task if it matches our current player;
                // otherwise it is stale, and should be cancelled (which
                // happens implicitly on drop).
                if let Some(player) = playerbox.as_ref() {
                    if *task.0.metadata() == player.id {
                        task.0.run();
                    }
                }
            }
            Ok(RuffleEvent::VirtualKeyEvent {
                down,
                key_descriptor,
            }) => {
                if let Some(player) = playerbox.as_ref() {
                    let event = if down {
                        PlayerEvent::KeyDown {
                            key: key_descriptor,
                        }
                    } else {
                        PlayerEvent::KeyUp {
                            key: key_descriptor,
                        }
                    };
                    player.player.lock().unwrap().handle_event(event);

                    if down {
                        // TODO: Add shift/capslock and pass in uppercase characters accordingly
                        if let LogicalKey::Character(c) = key_descriptor.logical_key {
                            let event = PlayerEvent::TextInput { codepoint: c };
                            player.player.lock().unwrap().handle_event(event);
                        }
                    }
                }
            }
            Ok(RuffleEvent::RunContextMenuCallback(index)) => {
                if let Some(player) = playerbox.as_ref() {
                    player
                        .player
                        .lock()
                        .unwrap()
                        .run_context_menu_callback(index);
                }
            }
            Ok(RuffleEvent::ClearContextMenu) => {
                if let Some(player) = playerbox.as_ref() {
                    player.player.lock().unwrap().clear_custom_menu_items();
                }
            }
            Ok(RuffleEvent::RequestContextMenu) => {
                if let Some(player) = playerbox.as_ref() {
                    log::warn!("preparing context menu!");
                    let items = player.player.lock().unwrap().prepare_context_menu();
                    with_activity(&app, |env, activity| {
                        JavaInterface::show_context_menu(env, activity, &items)
                    })
                    .unwrap();
                }
            }
        }

        let new_time = Instant::now();
        let dt = new_time.duration_since(last_frame_time).as_micros();
        if dt > 0 {
            last_frame_time = new_time;
            if let Some(player) = playerbox.as_ref() {
                if let Ok(mut player) = player.player.lock() {
                    player.tick(FloatDuration::from_millis(dt as f64 / 1000.0));
                    next_frame_time = Some(new_time + player.time_til_next_frame());
                    needs_redraw = player.needs_render();
                    let audio =
                        <dyn Any>::downcast_mut::<AAudioAudioBackend>(player.audio_mut()).unwrap();
                    audio.recreate_stream_if_needed();
                }
            } else {
                next_frame_time = None;
            }
        }

        if needs_redraw {
            if let Some(player) = playerbox.as_ref() {
                if let Ok(mut player) = player.player.lock() {
                    player.render();
                }
            }
        }
    }

    let _ = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw) }?;
        // Ensure that we take the EventSender back, or we'll leak it
        let _: jni::errors::Result<EventSender> =
            unsafe { env.take_rust_field(&*activity, jni_str!("eventLoopHandle")) };
        Ok(())
    });
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_keydown<'local>(
    mut unowned_env: EnvUnowned<'local>,
    this: JObject<'local>,
    key_tag: JString<'local>,
) {
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let tag = key_tag.try_to_string(env)?;

            let event_loop: MutexGuard<Sender<RuffleEvent>> =
                unsafe { env.get_rust_field(&this, jni_str!("eventLoopHandle")) }?;
            if let Some(desc) = key_tag_to_key_descriptor(&tag) {
                let _ = event_loop.send(RuffleEvent::VirtualKeyEvent {
                    down: true,
                    key_descriptor: desc,
                });
            }
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_keyup<'local>(
    mut unowned_env: EnvUnowned<'local>,
    this: JObject<'local>,
    key_tag: JString<'local>,
) {
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let tag = key_tag.try_to_string(env)?;

            let event_loop: MutexGuard<Sender<RuffleEvent>> =
                unsafe { env.get_rust_field(&this, jni_str!("eventLoopHandle")) }?;
            if let Some(desc) = key_tag_to_key_descriptor(&tag) {
                let _ = event_loop.send(RuffleEvent::VirtualKeyEvent {
                    down: false,
                    key_descriptor: desc,
                });
            }
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Attaches the current thread to the JVM and runs `f` with the JNI environment and the activity.
///
/// Note: The activity can't be taken from `ndk_context`, because since android-activity 0.6.1,
/// that holds the `Application` instead.
pub fn with_activity<T>(
    app: &AndroidApp,
    f: impl FnOnce(&mut Env, &JObject) -> T,
) -> jni::errors::Result<T> {
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };

    // This is a global reference, so it can only be borrowed, not wrapped as a local one.
    let activity_raw = app.activity_as_ptr() as jobject;

    vm.attach_current_thread(|env| {
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw) }?;
        Ok(f(env, &activity))
    })
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_requestContextMenu<'local>(
    mut unowned_env: EnvUnowned<'local>,
    this: JObject<'local>,
) {
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let event_loop: MutexGuard<Sender<RuffleEvent>> =
                unsafe { env.get_rust_field(&this, jni_str!("eventLoopHandle")) }?;
            let _ = event_loop.send(RuffleEvent::RequestContextMenu);
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_runContextMenuCallback<'local>(
    mut unowned_env: EnvUnowned<'local>,
    this: JObject<'local>,
    index: jint,
) {
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let event_loop: MutexGuard<Sender<RuffleEvent>> =
                unsafe { env.get_rust_field(&this, jni_str!("eventLoopHandle")) }?;
            let _ = event_loop.send(RuffleEvent::RunContextMenuCallback(index as usize));
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_clearContextMenu<'local>(
    mut unowned_env: EnvUnowned<'local>,
    this: JObject<'local>,
) {
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let event_loop: MutexGuard<Sender<RuffleEvent>> =
                unsafe { env.get_rust_field(&this, jni_str!("eventLoopHandle")) }?;
            let _ = event_loop.send(RuffleEvent::ClearContextMenu);
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn Java_rs_ruffle_PlayerActivity_nativeInit<'local>(
    mut unowned_env: EnvUnowned<'local>,
    class: JClass<'local>,
    crash_callback: JObject<'local>,
    log_path: JString<'local>,
) {
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            native_init(env, &class, &crash_callback, &log_path)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

fn native_init(
    env: &mut Env,
    class: &JClass,
    crash_callback: &JObject,
    log_path: &JString,
) -> jni::errors::Result<()> {
    let crash_callback = env.new_global_ref(crash_callback)?;
    let jvm = env.get_java_vm()?;

    // ---- SXDffle：初始化应用内日志文件（与 logcat 双写）----
    // 路径由 Kotlin 侧传入（getExternalFilesDir(null)/sxdffle.log），
    // 便于用户不接电脑也能导出日志。
    match log_path.try_to_string(env) {
        Ok(path) if !path.is_empty() => {
            if let Err(e) = crate::trace::init_app_log(std::path::Path::new(&path)) {
                // 失败不影响运行，只是退化为仅输出 logcat
                eprintln!("SXDffle: 无法创建日志文件 {path}: {e}");
            }
        }
        _ => {
            eprintln!("SXDffle: 未提供日志文件路径，仅输出 logcat");
        }
    }

    crate::trace::TeeLogger::install(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("ruffle")
            .with_filter(
                // SXDffle：必须显式放行 `sxdffle` 模块的 info 日志。
                // 上游只写了 "warn,ruffle=info"，而本项目的关键埋点
                // （字体加载、Referer/Cookie/flashVars 传递、socket 目标）
                // 都在 sxdffle crate 下，默认会被 warn 级过滤掉，导致真机排查时
                // 抓不到任何有用信息。
                android_logger::FilterBuilder::new()
                    .parse("warn,ruffle=info,sxdffle=info")
                    .build(),
            ),
    );

    panic::set_hook(Box::new(move |info| {
        let backtrace = Backtrace::new();
        let thread = thread::current();
        let thread = thread.name().unwrap_or("<unnamed>");
        let message = match info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &**s,
                None => "Box<Any>",
            },
        };

        let full = match info.location() {
            Some(location) => format!(
                "thread '{}' panicked at '{}': {}:{}\n{:?}",
                thread,
                message,
                location.file(),
                location.line(),
                backtrace
            ),
            None => format!(
                "thread '{}' panicked at '{}'\n{:?}",
                thread, message, backtrace
            ),
        };
        log::error!(target: "panic","{}", full);

        // SXDffle：内嵌宿主的 panic 不弹全屏崩溃页。
        //
        // 内嵌是实验特性，它自己失败只应让「网页里的那块画面」黑掉，
        // 而不该让整个 App 跳到 PanicActivity —— 用户会以为全屏功能也坏了。
        // 这里只记录日志，由 EmbedSession 的 status 反馈给 Kotlin 侧。
        if thread.starts_with("sxdffle-embed") || thread.starts_with("sxdffle-net") {
            log::error!(target: "panic", "SXDffle 非主线程 panic，已隔离（线程 {}）", thread);
            return;
        }

        // Any exception already pending on this thread is stashed by the attachment while the
        // callback runs, and re-thrown afterwards, so java will still discover it on their own.
        let result = jvm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let java_message = env.new_string(full)?;
            env.call_method(
                &*crash_callback,
                jni_str!("onCrash"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[(&java_message).into()],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::error!(target: "panic", "Failed to report crash to java: {}", e);
        }
    }));

    JavaInterface::init(env, class);
    Ok(())
}

fn get_loc_in_window(app: &AndroidApp) -> (i32, i32) {
    // no worky :(
    //ndk_glue::native_activity().show_soft_input(true);

    with_activity(app, JavaInterface::get_loc_in_window).unwrap()
}

fn get_view_size(app: &AndroidApp) -> Result<(i32, i32), Box<dyn std::error::Error>> {
    let size = with_activity(app, |env, activity| {
        let width = JavaInterface::get_surface_width(env, activity);
        let height = JavaInterface::get_surface_height(env, activity);
        (width, height)
    })?;

    Ok(size)
}

#[no_mangle]
fn android_main(app: AndroidApp) {
    log::info!("Starting android_main...");
    // Must happen before any reqwest client is built (e.g. by the navigator backend).
    let _ = rustls::crypto::ring::default_provider().install_default();
    run(app);
}
