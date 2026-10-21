//! SXDffle 嵌入式 Flash 宿主（实验性分支 `experiment/embedded-loader`）
//!
//! # 为什么需要这一层
//!
//! 上游 `ruffle-android` 的宿主是 `GameActivity`：它独占一个 Android Activity、
//! 独占整个窗口，Flash 画面必然铺满全屏。要让 Flash **原位嵌在网页里**，
//! 就必须绕开 Activity 的窗口管理，自己接管渲染目标。
//!
//! 本模块因此**完全不复用** `run()` 里的 android-activity 事件循环，改为：
//!
//! ```text
//! Kotlin (SurfaceView)                  Rust (本模块)
//!   │                                     │
//!   ├─ surfaceCreated ──JNI──────────────► nativeEmbedStart
//!   │   传 Surface + 宽高 + SWF 参数       │  ① ANativeWindow_fromSurface 取窗口
//!   │                                     │  ② spawn 渲染线程, 立即返回 handle
//!   │                                     │  ③ 线程内建 wgpu Surface + Player
//!   │                                     │  ④ tick/render 自循环
//!   │                                     │
//!   ├─ onTouch ────────JNI──────────────► nativeEmbedTouch  → mpsc
//!   ├─ onSizeChanged ──JNI──────────────► nativeEmbedResize → recreate surface
//!   └─ surfaceDestroyed JNI─────────────► nativeEmbedStop   → 退出循环 + release
//! ```
//!
//! # 引擎无损
//!
//! 引擎侧（`ruffle_core` / `ruffle_render_wgpu` / navigator / 字体 / 音频）**一行未改**，
//! 仍走本地 path 依赖的同一份 Ruffle 源码。差别只在于「谁把 ANativeWindow 递给 wgpu、
//! 谁驱动 tick」。因此渲染能力、AS1/AS2/AS3 支持度、TCP socket 行为与全屏版完全一致。
//!
//! # 与全屏版的关键差异
//!
//! | | 全屏版 `run()` | 本模块 |
//! |---|---|---|
//! | 窗口来源 | `AndroidApp::native_window()` | Kotlin 传入的 `Surface` |
//! | 事件循环 | android-activity `PollEvent` | 自建 mpsc + `recv_timeout` |
//! | 输入 | `InputEvent::MotionEvent` | JNI 直传屏幕坐标 |
//! | 缩放 | 无 | `scale_factor × zoom` |
//! | 旋转 | 无 | Kotlin 侧 View 旋转 + 尺寸互换 |

use std::any::Any;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ruffle_common::duration::FloatDuration;
use ruffle_core::backend::navigator::{OwnedFuture, SocketMode};
use ruffle_core::config::Letterbox;
use ruffle_core::context::UpdateContext;
use ruffle_core::external::{ExternalInterfaceProvider, Value as ExternalValue};
use ruffle_core::events::{MouseButton, MouseWheelDelta, PlayerEvent};
use ruffle_core::font::DefaultFont;
use ruffle_core::{Player, PlayerBuilder, ViewportDimensions};
use ruffle_frontend_utils::backends::navigator::{ExternalNavigatorBackend, FutureSpawner};
use ruffle_frontend_utils::backends::storage::DiskStorageBackend;
use ruffle_frontend_utils::content::{ContentDescriptor, PlayingContent};
use ruffle_render_wgpu::backend::WgpuRenderBackend;
use ruffle_render_wgpu::target::SwapChainTarget;
use url::Url;
use wgpu::rwh::{AndroidDisplayHandle, AndroidNdkWindowHandle, RawDisplayHandle, RawWindowHandle};

use crate::audio::AAudioAudioBackend;
use crate::navigator::AndroidNavigatorInterface;
use crate::trace::FileLogBackend;
use crate::ui::AndroidUiBackend;
use crate::{PlayerId, PlayerRunnable};

// ---------------------------------------------------------------------------
// 状态码（Kotlin 侧轮询，用于把「黑屏」和「初始化失败」区分开）
// ---------------------------------------------------------------------------

pub const STATUS_INIT: i32 = 0;
pub const STATUS_RUNNING: i32 = 1;
pub const STATUS_FAILED: i32 = 2;
pub const STATUS_STOPPED: i32 = 3;

// ---------------------------------------------------------------------------
// ANativeWindow 裸绑定
// ---------------------------------------------------------------------------
//
// 不用 `ndk::native_window::NativeWindow`，因为它依赖 jni-sys 0.3，
// 而本项目用的是 jni 0.22（jni-sys 0.4）—— 两个 `JNIEnv` 类型不兼容。
// 这里直接声明需要的三个 C 函数，用 `*mut c_void` 传 env，规避版本错配。

extern "C" {
    fn ANativeWindow_fromSurface(env: *mut c_void, surface: *mut c_void) -> *mut c_void;
    fn ANativeWindow_release(window: *mut c_void);
    fn ANativeWindow_getWidth(window: *mut c_void) -> i32;
    fn ANativeWindow_getHeight(window: *mut c_void) -> i32;
}

/// 对 `ANativeWindow*` 的 RAII 包装。
///
/// `ANativeWindow_fromSurface` 会把引用计数 +1，必须配对 `ANativeWindow_release`，
/// 否则每次打开游戏都会泄漏一块图形缓冲（表现为反复进出后 OOM）。
/// 当前进程 RSS（MB）。`/proc/self/statm` 第二列是常驻页数（页 4KB）。
fn rss_mb() -> u64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
        if let Some(pages) = s.split_whitespace().nth(1) {
            if let Ok(p) = pages.parse::<u64>() {
                return p * 4 / 1024;
            }
        }
    }
    0
}

extern "C" {
    /// 运行时查符号：`malloc_trim` 在老系统上不一定有，
    /// 直接 extern 会让整个 .so 加载失败，所以走 dlsym。
    fn dlsym(handle: *mut std::ffi::c_void, symbol: *const u8) -> *mut std::ffi::c_void;
}

/// 独立内存监控线程。
///
/// 关键：之前的采样全都挂在渲染循环上，而战报解析期间渲染完全停摆
/// （25 秒只渲染 64 帧），所以日志里从来没有战报期间的真实曲线——
/// 这正是我们反复误判峰值来源的原因。
/// 这个线程只读 `/proc/self/*`，不打锁、不碰引擎，脚本跑多久都能采样。
fn ensure_mem_monitor() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("sxdffle-mem".into())
            .spawn(|| {
                let mut last_mb = 0u64;
                let mut tick = 0u64;
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    let mb = rss_mb();
                    if mb.abs_diff(last_mb) >= 30 {
                        // GC 堆字节是 atomic 累加，不需要 Player 锁 ——
                        // 战报解析期间渲染线程被占死时，这里依然读得到。
                        let (alloc, freed, objs) = ruffle_core::heap_byte_stats_global();
                        let live = alloc.saturating_sub(freed) / (1024 * 1024);
                        log::warn!(
                            "SXDffle mem: RSS {}MB · GC堆 {}MB / {} 个对象（独立采样）",
                            mb,
                            live,
                            objs
                        );
                        last_mb = mb;
                    }
                    // 每 10 秒打一次映射排行：战报期间也能看到"谁在涨"
                    tick += 1;
                    if tick % 5 == 0 && mb > 800 {
                        let top = mem_top_mappings(6);
                        let line: Vec<String> = top
                            .iter()
                            .map(|(name, m)| format!("{}={}MB", name, m))
                            .collect();
                        log::warn!(
                            "SXDffle mem: RSS {}MB · 映射 TOP {}",
                            mb,
                            line.join(" · ")
                        );
                    }
                }
            });
    });
}

/// 按映射名聚合 `/proc/self/smaps` 的 Rss，返回占用最大的前 N 项（MB）。
///
/// 这是定位"1.8GB 到底是谁"最直接的证据：
/// 能区分 dalvik 堆、gralloc/kgsl 图形缓冲、匿名 malloc 堆、
/// 以及各个 .so 的文件映射。
fn mem_top_mappings(n: usize) -> Vec<(String, u64)> {
    use std::collections::HashMap;
    let mut map: HashMap<String, u64> = HashMap::new();
    let text = match std::fs::read_to_string("/proc/self/smaps") {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut cur: String = String::from("(anon ----)");
    for line in text.lines() {
        // 映射头判定：第一个字段是 "起-止" 的十六进制地址，第二个字段是 4 位权限。
        //
        // 上一版用「第 9 个字节是不是 '-'」判断，而 arm64 的地址是 12 位十六进制
        // （7f1234560000-...），第 9 个字节是 '0' 而不是 '-'，条件几乎永不成立——
        // 结果所有 Rss 被累加到同一个 key 上，得出的 "dalvik-Sentinel=5.4GB"
        // 比进程总 RSS 还大。这里改成结构化判断。
        let fields: Vec<&str> = line.split_whitespace().collect();
        let is_header = fields.len() >= 3 && {
            let range = fields[0];
            let perms = fields[1];
            range.contains('-')
                && range
                    .split('-')
                    .next()
                    .map(|a| a.len() >= 8 && a.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false)
                && perms.len() == 4
                && perms.chars().all(|c| "rwxps-".contains(c))
        };
        if is_header {
            let name = if fields.len() > 5 {
                fields[5..].join(" ")
            } else {
                format!("[anon {}]", fields[1])
            };
            cur = name;
        } else if let Some(rest) = line.strip_prefix("Rss:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            *map.entry(cur.clone()).or_insert(0) += kb;
        }
    }
    let mut v: Vec<(String, u64)> = map
        .into_iter()
        .map(|(k, kb)| (k, kb / 1024))
        .filter(|(_, mb)| *mb >= 8)
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(n);
    v
}

/// mimalloc 自己的账：(当前 rss, 峰值 rss, 当前 commit, 缺页数)，单位 MB。
///
/// commit 是 mimalloc 认为是"自己已保留、可读写"的内存；RSS 是实际驻留。
/// **commit ≈ RSS ⇒ 没有空闲页可归还**（mi_collect 自然无效）；
/// commit 远小于 RSS ⇒ 大头不在 mimalloc 的账上，得另找来源。
fn mi_proc_info() -> (u64, u64, u64, u64) {
    let mut elapsed = 0usize;
    let mut user = 0usize;
    let mut sys = 0usize;
    let mut cur_rss = 0usize;
    let mut peak_rss = 0usize;
    let mut cur_commit = 0usize;
    let mut peak_commit = 0usize;
    let mut faults = 0usize;
    unsafe {
        libmimalloc_sys::mi_process_info(
            &mut elapsed,
            &mut user,
            &mut sys,
            &mut cur_rss,
            &mut peak_rss,
            &mut cur_commit,
            &mut peak_commit,
            &mut faults,
        );
    }
    (
        cur_rss as u64 / (1024 * 1024),
        peak_rss as u64 / (1024 * 1024),
        cur_commit as u64 / (1024 * 1024),
        faults as u64,
    )
}

// ---- mimalloc 堆块遍历 ----
//
// `mi_heap_visit_blocks` 能枚举本线程堆里**每一个已分配块**。
// 按块大小分桶统计，就能一次性回答"这 1.3GB 是海量小对象还是少数巨块"，
// 不需要再逐个猜测是数组、字符串还是字节码。
static BLK_BYTES: [std::sync::atomic::AtomicU64; 8] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
static BLK_COUNT: [std::sync::atomic::AtomicU64; 8] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
const BLK_LABELS: [&str; 8] = [
    "≤64B", "65-256B", "257-1K", "1K-4K", "4K-16K", "16K-64K", "64K-1M", ">1M",
];

unsafe extern "C" {
    fn mi_heap_get_default() -> *mut core::ffi::c_void;
    fn mi_heap_visit_blocks(
        heap: *mut core::ffi::c_void,
        visit_all_blocks: bool,
        visitor: Option<
            unsafe extern "C" fn(
                *const core::ffi::c_void,
                *const core::ffi::c_void,
                *mut core::ffi::c_void,
                usize,
                *mut core::ffi::c_void,
            ) -> bool,
        >,
        arg: *mut core::ffi::c_void,
    ) -> bool;
}

unsafe extern "C" fn blk_visitor(
    _heap: *const core::ffi::c_void,
    _area: *const core::ffi::c_void,
    block: *mut core::ffi::c_void,
    size: usize,
    _arg: *mut core::ffi::c_void,
) -> bool {
    if block.is_null() {
        return true;
    }
    let b = match size {
        0..=64 => 0,
        65..=256 => 1,
        257..=1024 => 2,
        1025..=4096 => 3,
        4097..=16384 => 4,
        16385..=65536 => 5,
        65537..=1048576 => 6,
        _ => 7,
    };
    BLK_BYTES[b].fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
    BLK_COUNT[b].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}

/// 遍历渲染线程的 mimalloc 堆，按块大小分桶打印。
fn log_heap_blocks() {
    for i in 0..8 {
        BLK_BYTES[i].store(0, std::sync::atomic::Ordering::Relaxed);
        BLK_COUNT[i].store(0, std::sync::atomic::Ordering::Relaxed);
    }
    unsafe {
        let heap = mi_heap_get_default();
        if !heap.is_null() {
            mi_heap_visit_blocks(heap, true, Some(blk_visitor), core::ptr::null_mut());
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for i in 0..8 {
        let mb = BLK_BYTES[i].load(std::sync::atomic::Ordering::Relaxed) / (1024 * 1024);
        let n = BLK_COUNT[i].load(std::sync::atomic::Ordering::Relaxed);
        if n > 0 {
            parts.push(format!("{}:{}个/{}MB", BLK_LABELS[i], n, mb));
        }
    }
    log::info!("SXDffle heap 块分布 {}", parts.join(" · "));
}

static MI_STATS_BUF: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

unsafe extern "C" fn mi_stats_out(
    msg: *const std::os::raw::c_char,
    _arg: *mut std::os::raw::c_void,
) {
    if msg.is_null() {
        return;
    }
    // SAFETY: mimalloc 传入的是以 NUL 结尾的 C 字符串
    let text = unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy();
    if let Ok(mut buf) = MI_STATS_BUF.lock() {
        buf.push_str(&text);
    }
}

/// 让 mimalloc 把自己的分配分布打进日志。
///
/// 这是"1.4GB 到底由什么构成"的兜底手段：它会按 size class 列出
/// heap / normal / large / huge 各段的 current 字节数，
/// 一次就能看出是"海量小对象"还是"少数巨块"。
fn log_mi_stats() {
    if let Ok(mut buf) = MI_STATS_BUF.lock() {
        buf.clear();
    }
    unsafe {
        libmimalloc_sys::mi_stats_print_out(Some(mi_stats_out), std::ptr::null_mut());
    }
    if let Ok(buf) = MI_STATS_BUF.lock() {
        for line in buf.lines() {
            let t = line.trim();
            if !t.is_empty() {
                log::info!("SXDffle mimalloc: {}", t);
            }
        }
    }
}

/// 从 `/proc/self/smaps_rollup` 读内存分类（单位 MB）。
///
/// 返回 (Rss, Anonymous, Swap)。这三者能把 RSS 拆成
/// 「匿名内存（堆/栈/图形缓冲）」与「文件映射（so/资源）」，
/// 从而回答"167MB 的 GC 堆之外，那 1.8GB 到底是什么"。
fn mem_smaps_mb() -> (u64, u64, u64) {
    let mut rss = 0u64;
    let mut anon = 0u64;
    let mut swap = 0u64;
    if let Ok(s) = std::fs::read_to_string("/proc/self/smaps_rollup") {
        for line in s.lines() {
            let mut it = line.split_whitespace();
            let key = match it.next() {
                Some(k) => k,
                None => continue,
            };
            let val = it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            match key {
                "Rss:" => rss = val,
                "Anonymous:" => anon = val,
                "Swap:" => swap = val,
                _ => {}
            }
        }
    }
    (rss / 1024, anon / 1024, swap / 1024)
}

type TrimFn = unsafe extern "C" fn(usize) -> i32;
type MalloptFn = unsafe extern "C" fn(i32, i32) -> i32;

fn sym<T: Copy>(name: &[u8]) -> Option<T> {
    unsafe {
        let p = dlsym(std::ptr::null_mut(), name.as_ptr());
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute_copy::<*mut std::ffi::c_void, T>(&p))
        }
    }
}

fn trim_fn() -> Option<TrimFn> {
    static TRIM: std::sync::OnceLock<Option<TrimFn>> = std::sync::OnceLock::new();
    *TRIM.get_or_init(|| sym::<TrimFn>(b"malloc_trim\0"))
}

fn mallopt_fn() -> Option<MalloptFn> {
    static OPT: std::sync::OnceLock<Option<MalloptFn>> = std::sync::OnceLock::new();
    *OPT.get_or_init(|| sym::<MalloptFn>(b"mallopt\0"))
}

/// 会话结束后主动归还内存。
///
/// 一局页游能吃掉近 1GB（纹理、位图、音频），释放后 allocator 往往只是
/// 把它们留在自己的空闲链表里，RSS 不降。连续压测几十个平台就会一路涨到
/// 1.5GB，最后 C++ 侧 bad_alloc → SIGABRT（日志里那两次 Thread-39/21 就是这样）。
/// 每关一局就 trim 一次，把水位压回去。
/// SXDffle：把匿名页显式「压」进 zram（`madvise(MADV_PAGEOUT)`）。
///
/// 背景：RSS 里绝大部分是**匿名页**（GC 堆、mimalloc segment、wgpu 缓冲）。
/// 这些页内核只有在内存紧张时才回收，而且优先杀掉我们这种大进程。
/// `MADV_PAGEOUT` 是主动告诉内核「这些页现在不用，先压走」——
/// 语义完全安全（再访问会缺页换回），但 RSS 会立刻下降，
/// 从而避开 LMK（0.261111 战斗那次就是被 LMK 杀掉的：
/// 日志里 `系统内存紧张(level=20)` + `swap_buffers failed`，RSS 3.7GB 而系统已无余量）。
///
/// 只处理**匿名且可写**的映射；`/dev/kgsl-3d0` 这类设备映射、so 文件页、
/// `[stack]` 都跳过 —— 前者压不动，后两者丢了代价大。
fn pageout_anon_pages(min_len: usize) -> (usize, usize, usize) {
    extern "C" {
        fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: i32) -> i32;
    }
    // Linux 5.4+ / Android 11+：把页换出到 swap（Android 是 zram）
    const MADV_PAGEOUT: i32 = 21;

    // 一旦确认内核不支持（全部失败且成功 0 字节），就永久关闭，
    // 免得每 20 秒白跑一遍 maps 解析。本机（4.19）就是这种情况。
    static AVAILABLE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(true);
    if !AVAILABLE.load(std::sync::atomic::Ordering::Relaxed) {
        return (0, 0, 0);
    }

    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return (0, 0, 0);
    };
    let mut regions = 0usize;
    let mut ok_bytes = 0usize;
    let mut failed = 0usize;
    let mut last_err = 0i32;
    for line in maps.lines() {
        let mut it = line.split_whitespace();
        let Some(range) = it.next() else { continue };
        let Some(perms) = it.next() else { continue };
        // 只要匿名可写（path 为空）
        if !perms.starts_with("rw") {
            continue;
        }
        let mut parts = line.splitn(6, ' ');
        let _ = parts.next();
        let _ = parts.next();
        let _ = parts.next();
        let _ = parts.next();
        let _ = parts.next();
        let path = parts.next().unwrap_or("").trim();
        // Android 的匿名映射**不是**空名字，而是 `[anon:mimalloc]`、`[anon:libc_malloc]`、
        // `[anon:dalvik-main space]` 这种形式。0.261113 里我按「path 为空」筛选，
        // 结果一段都没匹配上（日志：`后台压页 0 段 / 0MB`），压页完全没有发生。
        // 正确判据：空路径 或 以 `[anon:` 开头；`[stack]`/`[vdso]`/`[anon:.bss]` 之外的
        // 具名区一律跳过。
        let is_anon = path.is_empty() || path.starts_with("[anon:");
        if !is_anon || path.starts_with("[anon:.bss") || path.starts_with("[anon:dalvik-zygote") {
            continue;
        }
        let Some((a, b)) = range.split_once('-') else { continue };
        let (Ok(start), Ok(end)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16))
        else {
            continue;
        };
        if end <= start || end - start < min_len {
            continue;
        }
        regions += 1;
        let r = unsafe {
            madvise(
                start as *mut core::ffi::c_void,
                end - start,
                MADV_PAGEOUT,
            )
        };
        if r == 0 {
            ok_bytes += end - start;
        } else {
            failed += 1;
            if last_err == 0 {
                // 记下第一个错误码：EINVAL(22) = 内核不支持这个 advice，
                // ENOMEM/EAGAIN = 页被锁住或暂时不可换出。
                last_err = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
            }
        }
    }
    if failed > 0 && ok_bytes == 0 {
        AVAILABLE.store(false, std::sync::atomic::Ordering::Relaxed);
        log::warn!(
            "SXDffle embed: madvise(PAGEOUT) 全部失败 errno={}（22=EINVAL 内核不支持）→ 永久关闭压页",
            last_err
        );
    } else if failed > 0 {
        log::warn!(
            "SXDffle embed: madvise(PAGEOUT) 部分失败 errno={}（12=ENOMEM，11=EAGAIN）",
            last_err
        );
    }
    (regions, ok_bytes, failed)
}

/// 递归统计目录占用：(字节数, 文件数)。用于回答「用户数据几百 MB 到底是什么」。
fn dir_usage(path: &std::path::Path) -> (u64, u64) {
    let mut total = 0u64;
    let mut count = 0u64;
    let Ok(rd) = std::fs::read_dir(path) else {
        return (0, 0);
    };
    for e in rd.flatten() {
        let p = e.path();
        // symlink 不跟（App 私有目录一般没有，但防环）
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            let (b, c) = dir_usage(&p);
            total += b;
            count += c;
        } else {
            total += md.len();
            count += 1;
        }
    }
    (total, count)
}

fn trim_memory(tag: &str) {
    let before = rss_mb();
    let trim_ret = trim_fn().map(|f| unsafe { f(0) });
    // bionic 的 scudo 还认 mallopt(M_PURGE)：把空闲页立刻还内核。
    // M_PURGE = -101（android/bionic malloc.h）
    let purge_ret = mallopt_fn().map(|f| unsafe { f(-101, 0) });
    // 注意：不要在这里调 mi_collect(true)。
    //
    // 0.261068 真机验证：它会在「所有线程的堆」上加锁并遍历，单次可能耗时数百毫秒
    // 到数秒；而这段代码每 5 秒在渲染线程跑一次，网络线程被反复阻塞，
    // 游戏加载就卡在等资源上（日志表现为 unloadAndStop 之后脚本再无进展）。
    // mimalloc 自身有延迟归还策略，足够用；真要强制归还，只应放在
    // 「内存已接近上限、且不在加载期」的时刻，而不是周期性调用。
    log::info!(
        "SXDffle embed: 内存归还({}) RSS {}MB → {}MB · trim={:?} purge={:?}",
        tag,
        before,
        rss_mb(),
        trim_ret,
        purge_ret
    );
}

pub(crate) struct AndroidSurfaceWindow(NonNull<c_void>);

// SAFETY: ANativeWindow 的文档保证它可以在不同线程间传递（内部引用计数是原子的）。
unsafe impl Send for AndroidSurfaceWindow {}

impl AndroidSurfaceWindow {
    /// # Safety
    /// `env` 必须是当前线程有效的 JNIEnv，`surface` 必须是有效的 `android.view.Surface`。
    pub(crate) unsafe fn from_surface(env: *mut c_void, surface: *mut c_void) -> Option<Self> {
        let ptr = ANativeWindow_fromSurface(env, surface);
        NonNull::new(ptr).map(Self)
    }

    fn handle(&self) -> RawWindowHandle {
        RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(self.0))
    }

    fn width(&self) -> i32 {
        unsafe { ANativeWindow_getWidth(self.0.as_ptr()) }
    }

    fn height(&self) -> i32 {
        unsafe { ANativeWindow_getHeight(self.0.as_ptr()) }
    }
}

impl Drop for AndroidSurfaceWindow {
    fn drop(&mut self) {
        unsafe { ANativeWindow_release(self.0.as_ptr()) }
    }
}

// ---------------------------------------------------------------------------
// 网络运行时
// ---------------------------------------------------------------------------

/// reqwest/hyper 依赖 tokio reactor；没有 runtime context 时，
/// 任何 `tokio::spawn` / IO 注册都会 panic：
/// `there is no reactor running, must be called from the context of a Tokio 1.x runtime`
///
/// 全屏版靠 `#[tokio::main]` 天然拥有 runtime，而本宿主是自建线程，
/// 因此必须自己准备一个。这里用后台 multi-thread runtime 驱动网络 IO，
/// 渲染线程只 `enter()` 拿 context —— 这样 poll future 时也能找到 reactor。
/// 模块级运行时指针：会话结束时可以整体关掉，重建一个新的。
static NET_RT: std::sync::atomic::AtomicPtr<tokio::runtime::Runtime> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

fn net_runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::atomic::Ordering;

    let cur = NET_RT.load(Ordering::Acquire);
    if !cur.is_null() {
        return unsafe { &*cur };
    }

    let rt = Box::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("sxdffle-net")
            .build()
            .expect("创建 tokio runtime 失败"),
    );
    let ptr = Box::into_raw(rt);
    match NET_RT.compare_exchange(std::ptr::null_mut(), ptr, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => unsafe { &*ptr },
        Err(existing) => {
            drop(unsafe { Box::from_raw(ptr) });
            unsafe { &*existing }
        }
    }
}

// ---------------------------------------------------------------------------
// 消息与执行器
// ---------------------------------------------------------------------------

/// 触摸阶段（与 Kotlin 侧 `EmbedFlashController.PHASE_*` 对应）
pub const PHASE_DOWN: u8 = 0;
pub const PHASE_MOVE: u8 = 1;
pub const PHASE_UP: u8 = 2;

enum EmbedMsg {
    /// 引擎内部 future 就绪，需要在渲染线程上轮询
    Task(PlayerRunnable),
    Touch {
        phase: u8,
        x: f64,
        y: f64,
    },
    Scroll {
        dy: f64,
    },
    Key {
        down: bool,
        tag: String,
    },
    Resize {
        width: u32,
        height: u32,
    },
    SetZoom(f64),
    /// 运行时修改舞台尺寸（0 表示恢复 SWF 自带值）
    SetStageSize(u32, u32),
    /// 暂停/恢复渲染（切后台时暂停 present，但保留 player 与网络连接）
    SetRenderPaused(bool),
    /// 挂上新的 Surface（切回前台，Surface 已被系统重建）
    AttachSurface(AndroidSurfaceWindow),
    /// 网页 JS 调用 SWF 通过 ExternalInterface.addCallback 注册的方法
    ExternalCall { name: String, args: String },
    /// 系统内存紧张（Activity.onTrimMemory）：跑一次 GC + 归还空闲页
    TrimMemory,
    /// 同上，但要等返回值（页面用它轮询 swf_is_load 这类状态）
    ExternalCallSync {
        name: String,
        args: String,
        reply: Sender<String>,
    },
    Stop,
}

/// ExternalInterface 调用日志限流：同一方法名 5 秒内只记一次。
fn should_log_external(name: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let map = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut guard) = map.lock() else {
        return true;
    };
    let now = Instant::now();
    match guard.get(name) {
        Some(t) if now.duration_since(*t) < Duration::from_secs(5) => false,
        _ => {
            guard.insert(name.to_string(), now);
            true
        }
    }
}

/// 宿主的 ExternalInterface 实现。
///
/// 页游经常用 `ExternalInterface.addCallback('function_xxx', ...)` 把方法暴露给
/// 网页，网页再通过 `<embed>` 元素调用。我们的引擎跑在网页之外，DOM 上的
/// `<embed>` 是个死元素 —— 于是页面会打印「function_xxx is not exist」。
///
/// 这里提供 provider 让 `ExternalInterface.available` 为 true（游戏才会走这条路），
/// 反向调用由 [EmbedMsg::ExternalCall] 送进引擎线程执行；
/// SWF → JS 的 call_method 没有同步回传通道，返回 undefined（目前够用）。
/// 从 `catch_unwind` 的 payload 里取出可读信息。
fn panic_text(e: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// 安装全局 panic hook：把位置与消息写进应用日志。
///
/// 之前只打印"渲染线程 panic，已隔离"，payload 被吞掉，无法定位。
/// 这个 hook 只记录一次（多个会话共用）。
fn install_panic_logger() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "?".into());
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown".to_string()
            };
            log::error!("SXDffle PANIC at {}: {}", loc, msg);
        }));
    });
}

struct HostExternalInterface;

/// 构建标记：确认当前 .so 里确实包含这次改动（LLVM 会把短字符串
/// 比较内联成立即数，从 .rodata 里搜不到，所以单独留一个 `#[used]` 常量）。
#[used]
static SXDFFLE_BUILD_TAG: &[u8] = b"SXDFFLE_BUILD_20261019_WARLOAD";

impl ExternalInterfaceProvider for HostExternalInterface {
    fn call_method(
        &self,
        _context: &mut UpdateContext<'_>,
        name: &str,
        args: &[ExternalValue],
    ) -> ExternalValue {
        // ---- 神仙道的宿主能力探测 ----
        //
        // 曾经尝试对 SxdAssetCache/SxdWarLoadOpt 返回 true（模拟真实联运容器），
        // 想让它走"资源缓存 + 战报分帧"路径。实测结果相反：
        //   帧 342 时 GC 对象 3121 万、RSS 7.85GB，战报卡住不出来。
        // 说明游戏只是"问一句宿主有没有这个能力"，真正干活要配套的预加载/喂数据
        // 接口（SxdAssetCache.setFeedFlash）。只回 true 不做事，会让它反复重试。
        //
        // 因此这里恢复 undefined（= 不支持），维持 0.261046 的行为。
        // 其它未知方法也保持 undefined。
        // 页游会把这类探测当轮询用（实测单局 2 万多次），
        // 逐条写日志会把日志文件撑到几 MB。每个方法名 5 秒最多记一条。
        if should_log_external(name) {
            log::info!(
                "SXDffle embed: ExternalInterface.call({}, {} 个参数) -> undefined",
                name,
                args.len()
            );
        } else {
            log::debug!("ExternalInterface.call({}) 已限流", name);
        }
        ExternalValue::Undefined
    }

    fn on_callback_available(&self, name: &str) {
        log::info!("SXDffle embed: 已注册回调 {}", name);
    }

    fn get_id(&self) -> Option<String> {
        Some("sxdffle".to_string())
    }
}

/// 把 JS 传来的 JSON 数组转成引擎的参数类型
fn parse_external_args(json: &str) -> Vec<ExternalValue> {
    fn conv(v: serde_json::Value) -> ExternalValue {
        match v {
            serde_json::Value::Null => ExternalValue::Null,
            serde_json::Value::Bool(b) => ExternalValue::Bool(b),
            serde_json::Value::Number(n) => ExternalValue::Number(n.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(s) => ExternalValue::String(s),
            serde_json::Value::Array(a) => {
                ExternalValue::List(a.into_iter().map(conv).collect())
            }
            serde_json::Value::Object(o) => {
                ExternalValue::Object(o.into_iter().map(|(k, v)| (k, conv(v))).collect())
            }
        }
    }
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Array(items)) => items.into_iter().map(conv).collect(),
        _ => Vec::new(),
    }
}

/// 引擎返回值 → JSON 字符串（回传给网页 JS）
fn external_to_json(v: &ExternalValue) -> serde_json::Value {
    match v {
        ExternalValue::Undefined | ExternalValue::Null => serde_json::Value::Null,
        ExternalValue::Bool(b) => serde_json::Value::Bool(*b),
        ExternalValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        ExternalValue::String(s) => serde_json::Value::String(s.clone()),
        ExternalValue::List(l) => {
            serde_json::Value::Array(l.iter().map(external_to_json).collect())
        }
        ExternalValue::Object(o) => serde_json::Value::Object(
            o.iter().map(|(k, v)| (k.clone(), external_to_json(v))).collect(),
        ),
    }
}

/// 把 Ruffle 的 future 调度到渲染线程。
///
/// 与全屏版的 `AndroidExecutor` 同构，区别是「唤醒」方式：
/// 那边靠 `AndroidAppWaker` 打断 android-activity 的 poll，
/// 这边靠 `mpsc::Sender` 打断 `recv_timeout` —— 更简单，也更适合独立线程。
struct EmbedExecutor {
    tx: Sender<EmbedMsg>,
    player_id: PlayerId,
}

impl<E: std::error::Error + 'static> FutureSpawner<E> for EmbedExecutor {
    fn spawn(&self, future: OwnedFuture<(), E>) {
        let future = async {
            if let Err(e) = future.await {
                log::error!("SXDffle embed 异步错误: {}", e);
            }
        };

        let tx = self.tx.clone();
        let scheduler = move |task: async_task::Runnable<PlayerId>| {
            let _ = tx.send(EmbedMsg::Task(PlayerRunnable::from_runnable(task)));
        };

        let (runnable, task) = async_task::Builder::new()
            .metadata(self.player_id)
            .spawn_local(|_| future, scheduler);

        task.detach();
        runnable.schedule();
    }
}

// ---------------------------------------------------------------------------
// 启动参数
// ---------------------------------------------------------------------------

/// 由 Kotlin 侧一次性传入的全部启动上下文。
///
/// 之所以全部用值传递而不是后续回调 Java，是为了让渲染线程**不持有任何 JNI 引用**：
/// 一旦它持有了 `JObject`，就必须 attach/detach 到 JVM，退出时机稍有不慎就会崩。
pub struct EmbedConfig {
    pub swf_url: String,
    pub referer: String,
    pub cookie: String,
    pub flash_vars: String,
    pub storage_dir: String,
    pub trace_output: Option<String>,
    /// 视口缩放比 = 物理像素 / CSS 像素。
    ///
    /// **不能用屏幕 density**：WebView 开了 useWideViewPort 后页面按 980 CSS px
    /// 布局再整体缩放，CSS→物理的真实比例约 1.1，而 density 是 2.75。
    /// 传 density 会让引擎的逻辑视口只有 1080/2.75≈393，远小于网页实际的 980，
    /// 游戏就会按 393 布局再铺到 1080 上 —— 画面明显偏小。
    pub viewport_scale: f64,
    pub width: u32,
    pub height: u32,
    /// 覆盖 SWF 的舞台尺寸（None = 用 SWF 头里自带的值）。
    ///
    /// 有些页游的 UI 会按 `Stage.stageWidth/Height` 决定展示多少内容，
    /// SWF 头里的尺寸（《神仙道》是 1250x650）可能偏小，导致部分功能挤在一起。
    /// 这里允许用户手动放大舞台，游戏会按更大的画布重新布局。
    pub stage_size: Option<(u32, u32)>,
    /// 是否启用音频后端（调试开关：用来二分内存增长来源）
    pub enable_audio: bool,
    /// 是否挂载 ExternalInterface（调试开关：同上）
    pub enable_external_interface: bool,
}

impl EmbedConfig {
    /// 与 Flash Player 一致的 flashvars 解析：按 `&` 切分并对 name/value 做 URL 解码。
    ///
    /// 注意不能直接把原始串塞给引擎：《神仙道》的 flashVars 里 `server_url` 含 `%3A%2F%2F`，
    /// 不解码的话游戏会在 `JSON.parse` 处抛 `Unexpected %`，画面停在黑屏。
    fn parameters(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(self.flash_vars.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 会话句柄（Kotlin 侧持有）
// ---------------------------------------------------------------------------

/// 一个嵌入式播放会话。`nativeEmbedStart` 返回它的裸指针给 Kotlin，
/// 后续所有操作都用这个指针找回自己。
pub struct EmbedSession {
    tx: Sender<EmbedMsg>,
    status: Arc<AtomicI32>,
    /// 已渲染帧数。用来区分「引擎没起来」和「起来了但没出图」——
    /// 这两种情况在用户眼里都是黑屏，但根因完全不同。
    frames: Arc<AtomicU64>,
    /// 渲染线程是否已真正退出（用于带超时的 join）
    finished: Arc<AtomicBool>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl EmbedSession {
    pub fn status(&self) -> i32 {
        self.status.load(Ordering::Relaxed)
    }

    pub fn frame_count(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn send(&self, msg: EmbedMsg) {
        let _ = self.tx.send(msg);
    }

    /// 停止并等待渲染线程退出。
    ///
    /// 必须等：调用方通常是 `surfaceDestroyed`，若不等线程退出就返回，
    /// 线程会拿着已销毁的 ANativeWindow 继续 present → SIGSEGV。
    ///
    /// 但不能无限等：`surfaceDestroyed` 跑在主线程，join 卡太久就是 ANR。
    /// 正常情况线程在收到 Stop 后下一次 `recv_timeout` 就会醒（≤ 一帧），
    /// 所以 2 秒超时只是兜底 —— 真触发时说明 GPU 侧卡死了。
    fn stop(&self) {
        self.send(EmbedMsg::Stop);

        let deadline = Instant::now() + Duration::from_millis(2000);
        while !self.finished.load(Ordering::Relaxed) {
            if Instant::now() >= deadline {
                log::error!(
                    "SXDffle embed: 渲染线程 2s 内未退出，放弃 join 以免 ANR（可能泄漏一个 Surface）"
                );
                if let Ok(mut g) = self.join.lock() {
                    let _ = g.take();
                }
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }

        if let Ok(mut g) = self.join.lock() {
            if let Some(h) = g.take() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for EmbedSession {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// 启动入口
// ---------------------------------------------------------------------------

/// 创建会话并启动渲染线程，立即返回（不阻塞 UI）。
pub fn start(config: EmbedConfig, window: AndroidSurfaceWindow) -> Box<EmbedSession> {
    install_panic_logger();
    ensure_mem_monitor();
    let (tx, rx) = channel();
    let status = Arc::new(AtomicI32::new(STATUS_INIT));
    let frames = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let thread_status = status.clone();
    let thread_frames = frames.clone();
    let thread_finished = finished.clone();
    let thread_tx = tx.clone();

    let handle = thread::Builder::new()
        .name("sxdffle-embed".into())
        .spawn(move || {
            // 捕获 panic：引擎内部任何一处 unwrap 失败都只应让「这块画面」黑掉，
            // 不能把整个 App 带走。配合 lib.rs 里 panic hook 的线程名过滤，
            // 内嵌崩溃既不会弹 PanicActivity，也不会影响全屏功能。
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                render_thread(config, window, rx, thread_tx, thread_status.clone(), thread_frames)
            }));

            match result {
                Ok(Ok(())) => {
                    thread_status.store(STATUS_STOPPED, Ordering::Relaxed);
                    log::info!("SXDffle embed: 渲染线程正常退出");
                }
                Ok(Err(e)) => {
                    thread_status.store(STATUS_FAILED, Ordering::Relaxed);
                    log::error!("SXDffle embed: 渲染线程失败: {}", e);
                }
                Err(e) => {
                    thread_status.store(STATUS_FAILED, Ordering::Relaxed);
                    log::error!("SXDffle embed: 渲染线程 panic，已隔离: {}", panic_text(&e));
                }
            }

            // 最后一步：通知 stop() 可以安全 join。
            // 必须放在 player / window 全部 drop 之后 —— 提前置位会让主线程
            // 在 EGLSurface 还没销毁时就往下走（removeView → Surface disconnect）。
            thread_finished.store(true, Ordering::Release);
        })
        .expect("无法创建 sxdffle-embed 线程");

    Box::new(EmbedSession {
        tx,
        status,
        frames,
        finished,
        join: Mutex::new(Some(handle)),
    })
}

// ---------------------------------------------------------------------------
// 渲染线程主体
// ---------------------------------------------------------------------------

fn render_thread(
    config: EmbedConfig,
    window: AndroidSurfaceWindow,
    rx: Receiver<EmbedMsg>,
    tx: Sender<EmbedMsg>,
    status: Arc<AtomicI32>,
    frames: Arc<AtomicU64>,
) -> Result<(), String> {
    // ---- 0. 运行时上下文（必须在任何网络对象之前建立）----
    // install_default 是进程级的且幂等：先跑过全屏版也不会冲突。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = net_runtime();
    let _rt_guard = rt.enter();

    let (width, height) = (config.width.max(1), config.height.max(1));

    log::info!(
        "SXDffle embed: 启动 {}x{} viewport_scale={:.3} swf={}",
        width,
        height,
        config.viewport_scale,
        config.swf_url
    );

    crate::crash::set_stage(3);

    // ---- 1. 渲染器：直接以 Kotlin 传入的 Surface 作为 swapchain 目标 ----
    //
    // 这一步是全屏版 `MainEvent::InitWindow` 里 `for_window_unsafe` 的等价物，
    // 唯一区别是窗口句柄来自参数而不是 `AndroidApp`。
    let mut renderer = unsafe {
        WgpuRenderBackend::<SwapChainTarget>::for_window_unsafe(
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Android(AndroidDisplayHandle::new())),
                raw_window_handle: window.handle(),
            },
            (width, height),
            wgpu::Backends::GL,
            wgpu::PowerPreference::HighPerformance,
            None,
        )
    }
    .map_err(|e| format!("创建 wgpu surface 失败: {e}"))?;

    log::info!(
        "SXDffle embed: wgpu surface 就绪 (ANativeWindow {}x{})",
        window.width(),
        window.height()
    );

    crate::crash::set_stage(4);

    // ---- 2. Player ----
    let movie_url = Url::parse(&config.swf_url)
        .or_else(|_| Url::parse("file://movie.swf"))
        .map_err(|e| format!("SWF URL 非法: {e}"))?;

    let referer_url = if config.referer.is_empty() {
        None
    } else {
        Url::parse(&config.referer).ok()
    };
    let cookie = if config.cookie.is_empty() {
        None
    } else {
        Some(config.cookie.clone())
    };

    let player_id = PlayerId::new();
    let future_spawner = EmbedExecutor {
        tx: tx.clone(),
        player_id,
    };

    // 与全屏版保持完全一致的网络配置：SocketMode::Allow 放行任意 TCP
    // （《神仙道》需要连 9x050.sxdweb.xd.com:8200），Referer/Cookie 保证防盗链与登录态。
    let navigator = ExternalNavigatorBackend::new(
        movie_url.clone(),
        referer_url.clone(),
        cookie,
        future_spawner,
        None,
        true,
        Default::default(),
        SocketMode::Allow,
        Rc::new(PlayingContent::DirectFile(ContentDescriptor::new_remote(
            movie_url.clone(),
        ))),
        AndroidNavigatorInterface,
    );

    log::info!("SXDffle embed: navigator 就绪 referer={:?}", referer_url);

    // 静态资源磁盘缓存。
    //
    // 位置很关键：必须落在 **cacheDir**（`/data/data/<pkg>/cache`），
    // 而不是 storage_dir（`files`）。放在 files 下时，Android 的「存储占用」
    // 会把它算进「用户数据」——实测 716MB 用户数据 / 55MB 缓存，
    // 系统在空间紧张时也不会清它，用户只能手动「清除数据」连账号一起丢。
    // 放到 cache 下之后它是真正的可丢弃缓存：系统随时能回收，卸载也不留。
    // storage_dir 是 `files` 的绝对路径，所以用 `../cache/httpcache`。
    {
        let old_dir = std::path::Path::new(&config.storage_dir).join("httpcache");
        let cache_dir = std::path::Path::new(&config.storage_dir).join("../cache/httpcache");
        // 0.261095 之前缓存在 files/httpcache。第一次启动时搬过去，
        // 免得用户升级后要重新下载几百 MB（同一分区，rename 是原子的）。
        if old_dir.is_dir() && !cache_dir.exists() {
            if let Some(parent) = cache_dir.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::rename(&old_dir, &cache_dir) {
                Ok(()) => log::info!("SXDffle embed: HTTP 缓存已迁移到 cacheDir"),
                Err(e) => log::warn!("SXDffle embed: 缓存迁移失败 {}", e),
            }
        }
        // 建目录后先 canonicalize 再交给引擎：`files/../cache` 在内核里等价于
        // `cache`，但部分 ROM 的「存储占用」按路径字符串归类，留 `..` 会算回用户数据。
        let _ = std::fs::create_dir_all(&cache_dir);
        let cache_dir = std::fs::canonicalize(&cache_dir).unwrap_or(cache_dir);
        ruffle_frontend_utils::backends::navigator::set_http_cache_dir(cache_dir.clone());
        log::info!("SXDffle embed: HTTP 缓存目录 {}", cache_dir.display());

        // 内核版本：`madvise(MADV_PAGEOUT)` 需要 Linux 5.4+，
        // 4.x 内核上它会直接 EINVAL —— 压页日志配合这一行就能判定。
        if let Ok(v) = std::fs::read_to_string("/proc/version") {
            log::info!("SXDffle embed: 内核 {}", v.trim());
        }

        // 存储占用明细：Android 设置页只给三个数字（应用/用户数据/缓存），
        // 拆不出「用户数据几百 MB」是谁的。这里按目录摊开，一次就能对上。
        {
            let base = std::path::Path::new(&config.storage_dir);
            let pkg = base.parent().unwrap_or(base);
            let mut parts = Vec::new();
            for (label, dir) in [
                ("files", base.to_path_buf()),
                ("cache", pkg.join("cache")),
                ("app_webview", pkg.join("app_webview")),
                ("databases", pkg.join("databases")),
                ("shared_prefs", pkg.join("shared_prefs")),
            ] {
                let (b, n) = dir_usage(&dir);
                if b > 0 {
                    parts.push(format!("{}={}MB/{}个", label, b / (1024 * 1024), n));
                }
            }
            if !parts.is_empty() {
                log::info!("SXDffle embed: 存储占用 {}", parts.join(" · "));
            }
        }
    }

    let mut builder = PlayerBuilder::new()
        .with_ui(AndroidUiBackend::new())
        .with_renderer(renderer)
        .with_storage(Box::new(DiskStorageBackend::new(
            config.storage_dir.clone().into(),
        )))
        .with_navigator(navigator)
        .with_log(FileLogBackend::new(
            config.trace_output.as_deref().map(std::path::Path::new),
        ))
        .with_video(ruffle_video_software::backend::SoftwareVideoBackend::new())
        // 内测魔改版：脚本单帧超时从默认 15 秒降到 8 秒。
        // 15 秒内战报解析能造出 2000 万对象 / 8GB，按时间中断太晚；
        // 前面还有一道"单帧 300 万对象"的熔断，这里是第二道保险。
        .with_max_execution_duration(Duration::from_secs(15));

    if config.enable_audio {
        builder = builder
            .with_audio(AAudioAudioBackend::new().map_err(|e| format!("音频后端初始化失败: {e:?}"))?);
    } else {
        log::warn!("SXDffle embed: 音频已按调试开关禁用（静音运行）");
        builder = builder.with_audio(ruffle_core::backend::audio::NullAudioBackend::new());
    }

    if config.enable_external_interface {
        builder = builder.with_external_interface(Box::new(HostExternalInterface));
    } else {
        log::warn!("SXDffle embed: ExternalInterface 已按调试开关禁用");
    }

    let player = builder.build();

    // 尺寸状态：必须在锁块之外声明，否则作用域只覆盖初始化段
    let mut zoom: f64 = 1.0;
    let mut view_w = width;
    let mut view_h = height;
    // 引擎的 scale_factor 固定为 CSS→物理的比例，不再随 zoom 变化 ——
    // 缩放是显示层的事（SurfaceView.scaleX/scaleY），引擎必须保持原生布局，
    // 否则游戏会重排、文字变小、点击坐标错位。
    let scale_factor = |_z: f64| config.viewport_scale;

    log::info!("SXDffle embed: Player 构建完成");
    // 内测魔改版：对象布局瘦身 —— 动态属性表与绑定方法表改为惰性分配。
    // 这两个数字直接决定 800 万级对象堆的下限，日志里留一行便于回归对比。
    log::info!(
        "SXDffle embed: 对象布局 {}（属性表惰性分配）",
        ruffle_core::object_layout_report(),
    );

    // ---- 3. 字体链（与全屏版一致，缺了中文会变方块）----
    {
    let mut p = player.lock().unwrap();
    p.set_default_font(
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
    p.set_default_font(
        DefaultFont::Serif,
        vec![
            "Noto Serif CJK SC".into(),
            "Source Han Serif SC".into(),
            "Noto Serif".into(),
        ],
    );
    p.set_default_font(
        DefaultFont::Typewriter,
        vec!["Droid Sans Mono".into(), "Noto Sans Mono".into()],
    );

    // ---- 4. 视口与画面适配 ----
    // 自定义分辨率必须用 NoScale：
    //
    // Ruffle 的 `build_matrices` 只在 `NoScale` 下才 fire RESIZE 事件，
    // 也只有这时 `stage_size` 才取自「视口逻辑尺寸」而不是 SWF 头。
    // 用默认 ShowAll 的话，`set_movie_size` 只是改了个数值，
    // 游戏既收不到 resize 通知、stageWidth 也不跟着视口变 —— 表现为「改了没反应」。
    if config.stage_size.is_some() {
        log::info!("SXDffle embed: 使用 NoScale 模式（视口即舞台）");
        p.set_scale_mode(ruffle_core::StageScaleMode::NoScale);
    }

    // 加载期背景改黑。
    //
    // 之前看到的"白块"并不是 Surface buffer 未初始化，而是 **Ruffle 的 stage 默认背景**：
    // `Player::render()` 里 `stage.background_color().unwrap_or(Color::WHITE)` —— 默认就是白。
    // 所以 SWF 还没渲染出来时，整块画面是白的。
    //
    // 这里在 fetch_root_movie 之前把它设成黑色，加载期就是黑底；
    // SWF 加载完后游戏画面会铺满 stage，黑底只留在 letterbox 区域（本来也是黑边）。
    //
    // 特别注意：**不能用 lockCanvas 去"预涂黑"** —— 它会把 Surface 切到 CPU 可写模式
    // 并重配 buffer 队列，紧接着 EGL 创建 window surface 会 BadAlloc（真机已复现）。
    p.set_background_color(Some(ruffle_core::Color::from_rgb(0x000000, 255)));
    log::info!("SXDffle embed: 加载期背景已设为黑色");

    p.set_letterbox(Letterbox::On);
    p.set_viewport_dimensions(ViewportDimensions {
        width: view_w,
        height: view_h,
        scale_factor: scale_factor(zoom),
    });

    // ---- 5. 载入 SWF ----
    let parameters = config.parameters();
    log::info!("SXDffle embed: flashVars 解析出 {} 项", parameters.len());
    crate::crash::set_stage(5);
    log::info!("SXDffle embed: 开始加载 SWF {}", movie_url);
    p.fetch_root_movie(movie_url.to_string(), parameters, Box::new(|_| {}));
    p.set_is_playing(true);
    } // 释放初始化锁：此后所有访问都是短锁，避免与 future 轮询互锁

    crate::crash::set_stage(6);
    status.store(STATUS_RUNNING, Ordering::Relaxed);
    log::info!(
        "SXDffle embed: player 就绪，进入渲染循环 viewport={}x{} scale_factor={:.2}",
        view_w,
        view_h,
        scale_factor(zoom)
    );

    // 舞台尺寸：自定义分辨率走 NoScale（视口即舞台），无需再 set_movie_size；
    // 这里只在「自动」模式下什么都不做。
    let mut pending_stage: Option<(u32, u32)> = None;

    // ---- 6. 主循环 ----
    let mut last_frame = Instant::now();
    let mut next_frame = Instant::now();
    let mut first_render = true;
    // window 可能被 AttachSurface 替换（切后台再回来时 Surface 是新的）
    let mut window: Option<AndroidSurfaceWindow> = Some(window);
    let mut render_paused = false;
    // 每 5 秒记录一次 RSS：页游加载资源时内存会阶跃上涨，
    // 有了时间线才能判断是哪一局把水位顶上去的。
    let mut last_rss_log = Instant::now();
    // 冲击检测单独用 1 秒采样：战报加载 5 秒能涨 6GB，5 秒一次根本来不及。
    let mut last_rss_check = Instant::now();
    // 主动内存管理：每分钟检查一次，对象数过高就强制完整 GC。
    // 页游会把大量临时对象（战报、特效帧）留在 GC 堆里，默认的增量 GC
    // 只按"分配债务"推进，债务为 0 时几乎不干活。
    let mut last_full_gc = Instant::now();
    // 加载期 GC 泵：每 250ms 给一小片预算，把回收摊到每帧。
    let mut last_gc_pump = Instant::now();
    let mut gc_pump_batches = 0u64;
    // GC 刚回收一大批 → 立刻归还空闲页（见下面的泵逻辑）
    let mut want_mi_sweep = false;
    // 增量回收剩余片数 / 连续无进展次数（0 表示当前没有紧急回收在进行）
    let mut gc_slice_budget = 0u32;
    let mut gc_slice_idle = 0u32;
    // 上一次采样的 RSS，用于识别"5 秒内涨几百 MB"的瞬时冲击
    let mut last_sample_mb = 0u64;
    // 爆发期快照的节流（崩溃往往在 5 秒内发生，等不到 15 秒的常规采样）
    let mut last_burst_log = Instant::now() - Duration::from_secs(10);
    // 渲染节流：一局页游每渲染一帧大约多占 0.2~0.3MB（Ruffle 的位图是原始 RGBA，
    // 没有 Flash 那样的压缩/按需解码），把 render 限制在 30fps 能直接按比例减缓增长。
    // 游戏本身是 24fps，节流到 30 不会影响手感。
    let mut last_present = Instant::now() - Duration::from_secs(1);
    let min_frame_interval = Duration::from_millis(33);
    // 切后台时定期把匿名页压进 zram。
    let mut last_pageout = Instant::now() - Duration::from_secs(60);

    // Stop 是从**内层**消息队列里读到的，`break` 只会跳出内层循环，
    // 外层循环还会继续跑下一帧 —— 表现就是线程永远不退出、每次关内嵌
    // 都超时泄漏一整个 player（内存持续上涨，最终被系统 kills）。
    // 用显式标记把退出信号传到外层。
    let mut quitting = false;
    // 收到 Stop 后不立刻退出：先"空转"一段时间，继续 poll 队列里的加载 future，
    // 让它们把在途请求跑完、自己松手（否则硬丢 future 会在 tokio/GC 的析构里崩）。
    let mut stop_at: Option<Instant> = None;

    loop {
        // 6a. 先排空消息队列
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if !handle_msg(
                        msg,
                        &player,
                        &mut window,
                        &mut render_paused,
                        &mut zoom,
                        &mut view_w,
                        &mut view_h,
                        Some(&mut stop_at),
                        scale_factor,
                    ) {
                        quitting = true;
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                // 消息通道断开（会话已被丢弃）：同样走统一收尾
                Err(TryRecvError::Disconnected) => {
                    quitting = true;
                    break;
                }
            }
        }
        if quitting {
            break;
        }

        // 6a-1. 停止中：等所有加载 future 自己结束。
        // 引用计数回到 1（只剩我们这一份）就说明它们都松手了。
        if let Some(t) = stop_at {
            if std::sync::Arc::strong_count(&player) <= 1
                || t.elapsed() > Duration::from_secs(6)
            {
                log::info!(
                    "SXDffle embed: 收尾等待结束（{}ms），引用计数={}",
                    t.elapsed().as_millis(),
                    std::sync::Arc::strong_count(&player)
                );
                break;
            }
        }

        // 6a-2. 舞台尺寸覆盖（等 SWF 真正加载完）
        if let Some((tw, th)) = pending_stage {
            let ready = { player.lock().unwrap().movie_width() > 0 };
            if ready {
                apply_stage_size(&player, tw, th);
                pending_stage = None;
            }
        }

        // 6b. 切后台（Surface 已销毁）时保持低驻留。
        //
        // LMK 杀的是「RSS 大的进程」，不是「在干活的进程」。
        // 0.261111 那次战斗结束/切走时被系统杀掉，日志停在
        // `系统内存紧张(level=20)` + `swap_buffers failed`，RSS 仍有 3.7GB。
        // 所以只要不在前台，就每 20 秒把匿名页压走一次，让 RSS 落到几百 MB。
        if render_paused && last_pageout.elapsed() >= Duration::from_secs(20) {
            last_pageout = Instant::now();
            let before = rss_mb();
            let (n, bytes, failed) = pageout_anon_pages(256 * 1024);
            log::info!(
                "SXDffle embed: 后台压页 {} 段 / {}MB（失败 {}）· RSS {}MB → {}MB",
                n,
                bytes / (1024 * 1024),
                failed,
                before,
                rss_mb()
            );
        }

        // 6b. 推进一帧（停止中就不再 tick/render，只等 future 收尾）
        let now = Instant::now();
        let dt_ms = now.duration_since(last_frame).as_micros() as f64 / 1000.0;
        // 后台时限速到 ~8fps：压走的页一旦被全速 tick 摸一遍就全换回来了，
        // 那样 PAGEOUT 等于白做。8fps 足够维持动画/网络状态，代价只是后台时 CPU 略动。
        let paused_throttle =
            render_paused && now.duration_since(last_frame) < Duration::from_millis(120);
        if dt_ms > 0.0 && stop_at.is_none() && !paused_throttle {
            last_frame = now;
            // 锁只在这一小段里持有：一旦持有跨过 task.run()，
            // 而 future 内部又要 lock 同一个 Player，就会自死锁。
            let mut p = player.lock().unwrap();
            // 长帧归因：伙伴出手/伤害数字/换字体这些"瞬间卡"到底是卡在
            // 逻辑帧（tick）还是渲染帧（render）。超过 120ms 才记一条，
            // 正常 24fps 下不会刷屏。
            let t0 = Instant::now();
            p.tick(FloatDuration::from_millis(dt_ms));
            let tick_ms = t0.elapsed().as_millis();
            next_frame = now + p.time_til_next_frame();

            if let Some(audio) = <dyn Any>::downcast_mut::<AAudioAudioBackend>(p.audio_mut()) {
                audio.recreate_stream_if_needed();
            }

            // 暂停渲染时仍继续 tick —— 游戏逻辑与 TCP 连接保持活着，
            // 只是不再 present（Surface 已销毁，present 会失败）。
            if !render_paused
                && (p.needs_render() || first_render)
                && last_present.elapsed() >= min_frame_interval
            {
                let t1 = Instant::now();
                // SXDffle：渲染阶段的分配（wgpu 缓冲、命令、上传暂存）
                let _sxd_tag = ruffle_core::alloc_tag::Guard::new(4);
                p.render();
                drop(_sxd_tag);
                let render_ms = t1.elapsed().as_millis();
                if tick_ms + render_ms > 120 {
                    let (gc_n, _) = p.gc_stats();
                    log::warn!(
                        "SXDffle 长帧: tick {}ms + render {}ms · GC {} 个 · 帧 #{}",
                        tick_ms,
                        render_ms,
                        gc_n,
                        frames.load(Ordering::Relaxed)
                    );
                }
                last_present = Instant::now();
                frames.fetch_add(1, Ordering::Relaxed);
                if first_render {
                    log::info!("SXDffle embed: 首帧已渲染");
                    crate::crash::set_stage(7);
                }
                first_render = false;
            }

            if want_mi_sweep {
                want_mi_sweep = false;
                let before = rss_mb();
                unsafe { libmimalloc_sys::mi_collect(true) };
                let after = rss_mb();
                if before.saturating_sub(after) >= 32 {
                    log::info!(
                        "SXDffle embed: GC 后归还 RSS {}MB → {}MB（削 {}MB）",
                        before,
                        after,
                        before - after
                    );
                }
            }

            // ---- 紧急回收的增量切片 ----
            //
            // 每帧只做一片（50 万对象），60 片封顶；连续 3 片没有可回收的东西
            // 就提前收工。这样即使面对 1400 万临时对象，单帧也只多几十毫秒，
            // 进度条和游戏逻辑都不会被按住。
            if gc_slice_budget > 0 {
                gc_slice_budget -= 1;
                let (b, a) = p.pump_gc(500_000.0);
                if a >= b {
                    gc_slice_idle += 1;
                } else {
                    gc_slice_idle = 0;
                }
                if gc_slice_idle >= 3 {
                    gc_slice_budget = 0;
                }
                log::info!(
                    "SXDffle embed: 增量回收 {} → {} 个对象（剩余 {} 片）",
                    b,
                    a,
                    gc_slice_budget
                );
                if gc_slice_budget == 0 {
                    drop(p);
                    trim_memory("增量回收结束");
                    continue;
                }
            }

            // ---- 加载期 GC 泵 ----
            //
            // 战报加载时每帧新增十几万对象，而 20 秒一次的常态回收只能"事后"扫尾：
            // 等它跑起来，RSS 早已从 2GB 冲到 8GB，渲染线程连进度条都画不出来。
            // 这里改成每 250ms 给一小片预算（20 万对象），把回收摊进每帧，
            // 让存活对象数在增长过程中就被压住，而不是攒到两千万再一次性扫。
            if last_gc_pump.elapsed() >= Duration::from_millis(250) {
                let (c, _) = p.gc_stats();
                if c > 1_500_000 {
                    last_gc_pump = Instant::now();
                    let (b, a) = p.pump_gc(200_000.0);
                    gc_pump_batches += 1;
                    // SXDffle：一轮 GC 真回收掉大量对象，说明刚才刚发生过大批释放 ——
                    // 此刻 mimalloc 手里必然多出一堆空闲页，是归还的最佳时机。
                    // 比「按 RSS 阈值盲调」精准得多：不浪费在没碎片的时刻，
                    // 也不会像 0.261105 那样因为退避而错过碎片的产生。
                    if b > a + 150_000 {
                        want_mi_sweep = true;
                    }
                    // 每 8 批（约 2 秒）记一次，避免日志本身成为负担
                    if gc_pump_batches % 8 == 1 {
                        log::info!(
                            "SXDffle embed: GC 泵 #{} {} → {} 个对象",
                            gc_pump_batches,
                            b,
                            a
                        );
                    }
                }
            }
        }

        // ---- 每秒一次：冲击检测 ----
        //
        // 之前的采样周期是 5 秒，而战报加载 5 秒能涨 6GB：
        // 等采样发现时已经在 7~8GB，回收只是亡羊补牢。
        // 现在 1 秒采样、涨 400MB 就立刻回收，把触发点提前到 2~3GB。
        // 500ms 一次（原来 1 秒）：战报加载时 RSS 一秒能涨 1.5GB，
        // 1 秒的采样间隔会让阈值形同虚设 —— 上一次看到 3.5GB，
        // 下一次直接就是 5.2GB，中间那段没人管。
        if last_rss_check.elapsed() >= Duration::from_millis(500) {
            last_rss_check = Instant::now();
            let mb = rss_mb();

            if last_sample_mb > 0 && mb > last_sample_mb + 400 {
                log::warn!(
                    "SXDffle embed: RSS 1 秒内 {}MB → {}MB，转入增量回收",
                    last_sample_mb,
                    mb
                );
                // ---- 爆发快照 ----
                //
                // 崩溃发生在爆发后 5 秒内，15 秒的常规采样永远看不到现场。
                // 这里在「1 秒涨 400MB」的瞬间就把账摊开（最多 2 秒一次）：
                // 分档合计 / 大块 TOP / 映射 TOP / slab，一次性回答
                // 「这 6GB 到底是 GC 对象、大块缓冲，还是分配器攥着的页」。
                if last_burst_log.elapsed() >= Duration::from_secs(2) {
                    last_burst_log = Instant::now();
                    let stats = crate::alloc_bucket_stats();
                    let mut total = 0usize;
                    let mut parts: Vec<String> = Vec::new();
                    for (i, (bytes, count)) in stats.iter().enumerate() {
                        total += *bytes;
                        if *bytes >= 32 * 1024 * 1024 {
                            parts.push(format!(
                                "{}:{:.0}万/{}MB",
                                crate::ALLOC_BUCKET_LABELS[i],
                                *count as f64 / 10000.0,
                                bytes / (1024 * 1024)
                            ));
                        }
                    }
                    let (objs, heap_mb, mv_n, mv_b) = {
                        let p = player.lock().unwrap();
                        let (n, _) = p.gc_stats();
                        let (mn, mb2) = p.movie_resident_stats();
                        (n, 0.0f64, mn, mb2)
                    };
                    // 位图 CPU 像素：GC 堆统计看不到它，但经常是最大的单项。
                    let (bm_n, bm_b) = ruffle_core::bitmap::bitmap_data::bitmap_stats();
                    log::warn!(
                        "SXDffle embed: 爆发快照 RSS {}MB · 档位合计 {}MB · slab {}MB · GC {}万对象 · 位图 {}个/{}MB · SWF {}部/{}MB · {}",
                        mb,
                        total / (1024 * 1024),
                        ruffle_core::Player::gc_slab_bytes() / (1024 * 1024),
                        objs / 10000,
                        bm_n,
                        bm_b / (1024 * 1024),
                        mv_n,
                        mv_b / (1024 * 1024),
                        parts.join(" ")
                    );
                    let _ = heap_mb;
                    let top = crate::big_alloc_top(6);
                    if !top.is_empty() {
                        let line: Vec<String> = top
                            .iter()
                            .map(|(sz, n)| format!("{}KB×{}", sz / 1024, n))
                            .collect();
                        log::warn!("SXDffle embed: 爆发大块 TOP {}", line.join(" · "));
                    }
                    // 精确的「阶段归属」：块头带标签，合计应与 ≥64K 档位近似相等。
                    // 之前这张表一直是盲区（只认出 800MB / 实际 2.8GB），
                    // 现在换成块头记账，这一行才能当证据用。
                    {
                        let (tags, tag_total) = crate::alloc_tag_stats();
                        let mut tparts: Vec<String> = Vec::new();
                        for (i, b) in tags.iter().enumerate() {
                            if *b >= 8 * 1024 * 1024 {
                                tparts.push(format!(
                                    "{} {}MB",
                                    crate::TAG_LABELS[i],
                                    b / (1024 * 1024)
                                ));
                            }
                        }
                        log::warn!(
                            "SXDffle embed: 爆发归属(≥64K) 合计 {}MB · {}",
                            tag_total / (1024 * 1024),
                            tparts.join(" · ")
                        );
                    }
                    let maps = mem_top_mappings(5);
                    if !maps.is_empty() {
                        let line: Vec<String> = maps
                            .iter()
                            .map(|(name, mb)| format!("{}={}MB", name, mb))
                            .collect();
                        log::warn!("SXDffle embed: 爆发映射 {}", line.join(" · "));
                    }
                }
                // 0.261090：不再整轮阻塞。
                //
                // 0.261087/089 的 96% 卡死就是这里：战报加载瞬间产生 1200 万个
                // 临时对象（GC 堆 1400 万），`force_full_gc` 一轮要 4~5 秒，
                // 连跑 5 轮 = 9.4 秒完全停住，游戏的 War.init 超时失败，
                // 单位全留在原点 —— 就是截图里"空擂台 + 都挤在左上角"。
                //
                // 改成把同样的活切成 40 片、每帧一片（80 万对象），
                // 单帧最多几十毫秒；RSS 的峰值不变，但界面一直在动。
                gc_slice_budget = 60;
                gc_slice_idle = 0;
            }

            // ---- mimalloc 强制归还（止血） ----
            //
            // GC 只负责回收「没人引用的对象」，回收完之后那些页仍然攥在
            // mimalloc 手里：战报加载时对象数在几百万上下剧烈增减，
            // size-class 页被撑开之后不会自己缩回去，RSS 就一路顶到 6.9GB 被系统杀。
            //
            // `mi_collect(true)` 会把所有线程堆里完全空闲的页还给内核。
            // 它要锁所有线程堆，单次可能几百毫秒到数秒 —— 之前不敢在加载期用，
            // 但现在的前提是「不还就 OOM」，卡一下远好于闪退。
            // 实测一次归还削掉 1083MB（5944 → 4861），说明 mimalloc 手里
            // 确实攥着大量完全空闲的页。阈值降到 3.6GB、冷却 8 秒，
            // 让它更早介入，把峰值压在系统上限以下，而不是等冲高再救。
            // 指数退避：连续几次归还都削不掉东西时，说明 mimalloc 手里
            // 已经没有空闲页了，再调就是每次白卡几百毫秒。
            // 实测 0.261103：第一次 5235 → 3889（削 1346MB），
            // 之后连调 9 次全是 0~2MB 的空转 —— 那 9 次卡顿完全是浪费。
            static MI_STATE: std::sync::Mutex<(Option<Instant>, u32, u64)> =
                std::sync::Mutex::new((None, 0, 0));
            if mb >= 3600 {
                let (last, idle, last_mb) = MI_STATE
                    .lock()
                    .map(|g| *g)
                    .unwrap_or((None, 0, 0));
                // 关键：如果 RSS 比上次调用时又涨了 300MB 以上，说明**新的碎片还在产生**，
                // 此时绝不能退避 —— 0.261105 就栽在这里：空转 4 次后退避到 128 秒，
                // 期间 RSS 从 4426MB 冲到 7117MB 才被救回，等于退避把安全阀关掉了。
                let growing = mb > last_mb + 300;
                let cooldown = if growing {
                    Duration::ZERO
                } else {
                    // 没在涨才退避，且最多 32 秒（128 秒太长，错过一次增长就是几 GB）
                    Duration::from_secs(8u64 << idle.min(2))
                };
                let due = last.map(|t| t.elapsed() >= cooldown).unwrap_or(true);
                if due {
                    let before = rss_mb();
                    unsafe { libmimalloc_sys::mi_collect(true) };
                    let after = rss_mb();
                    let freed = before.saturating_sub(after);
                    let next_idle = if freed >= 64 || growing { 0 } else { idle + 1 };
                    if let Ok(mut g) = MI_STATE.lock() {
                        *g = (Some(Instant::now()), next_idle, mb);
                    }
                    log::warn!(
                        "SXDffle embed: mimalloc 强制归还 RSS {}MB → {}MB（削 {}MB · 连续空转 {}）",
                        before,
                        after,
                        freed,
                        next_idle
                    );
                }
            }
            last_sample_mb = mb;

            // ---- 每 5 秒的详细日志 ----
            // 15 秒一次（原来 5 秒）。
            //
            // 这一整块要遍历 GC 对象做分类统计、读 /proc/self/smaps 解析
            // 三千行映射表 —— 单次 30~120ms。在 8fps（每帧 125ms）的情况下，
            // 5 秒一次等于每 40 帧就有一帧被诊断吃掉，**测量本身在拖慢被测量的东西**。
            // 内存曲线的时间分辨率降到 15 秒完全够用，帧率却能拿回来几个点。
            if last_rss_log.elapsed() >= Duration::from_secs(15) {
                last_rss_log = Instant::now();

                // 回收「已解析完 + 45 秒没被查表、也没推进时间轴」的电影解压字节。
                //
                // SXDffle 0.261112：**默认关闭**。
                // 0.261111 在第一次战斗时硬崩（原生崩溃，RSS 只有 3745MB，不是 OOM），
                // 而这条路径是唯一新增的“会移动数据生命周期”的改动。
                // 原理上它也有无法覆盖的情形：一个 `stop()` 停在某帧的 clip 不推进时间轴、
                // 也不查库表，45 秒后字节被丢；游戏再 `play()`/`gotoAndPlay()` 时
                // 要从字节里读 Place 标签 —— 读到空，轻则丢子对象，重则崩。
                // 要安全地回收，必须先有“字节丢了还能从 httpcache 同步取回”的路径，
                // 那是下一阶段的事。这里先留开关，默认 0 = 不回收。
                // 这是第一次真正让战报内存掉下来：900 多部电影的解压字节
                // 挂在 movie_libraries 的键上，GC 碰不到，只有这里能动手。
                const RELEASE_COLD_SWF_IDLE_SECS: u32 = 20; // 只对「无时间轴角色」的电影生效
                if RELEASE_COLD_SWF_IDLE_SECS > 0 {
                    let pl = player.lock().unwrap();
                    let (rn, rb) = pl.release_cold_movie_data(RELEASE_COLD_SWF_IDLE_SECS);
                    drop(pl);
                    if rn > 0 {
                        log::info!(
                            "SXDffle embed: 回收冷电影字节 {} 部 / {}MB",
                            rn,
                            rb / (1024 * 1024)
                        );
                    }
                }
                // 分项统计：GC 存活对象数 + BitmapData 的 CPU 像素字节。
                // 两者合起来能回答"这 3.6GB 到底是对象泄漏还是位图数据"。
                let (gc_count, gc_debt, obj_top, (so_count, str_count), heap) = {
                    let p = player.lock().unwrap();
                    let (c, d) = p.gc_stats();
                    (c, d, p.object_stats_top(6), p.alloc_counters(), p.heap_bytes())
                };
                // GC 堆真实字节：live = 分配 - 释放。
                // 和 RSS 一比就能判断内存是在活对象上、还是在分配器碎片上。
                let (heap_alloc, heap_free, heap_live_objs) = heap;
                let heap_live = heap_alloc.saturating_sub(heap_free);
                let avg = if heap_live_objs > 0 {
                    heap_live / heap_live_objs
                } else {
                    0
                };
                log::info!(
                    "SXDffle embed: GC slab {}MB（GcBox 实际占用）",
                    ruffle_core::Player::gc_slab_bytes() / (1024 * 1024)
                );
                log::info!(
                    "SXDffle embed: GC 堆 存活 {}MB（对象 {} 个 · 平均 {}B）· 累计分配 {}MB / 释放 {}MB",
                    heap_live / (1024 * 1024),
                    heap_live_objs,
                    avg,
                    heap_alloc / (1024 * 1024),
                    heap_free / (1024 * 1024)
                );
                // 存活对象按尺寸分桶。1800 万对象到底是什么，看这一行。
                {
                    let (hist, labels) = ruffle_core::Player::gc_size_histogram();
                    let top: Vec<(&str, usize)> = hist
                        .iter()
                        .zip(labels.iter())
                        .filter(|(n, _)| **n > 1000)
                        .map(|(n, l)| (*l, *n))
                        .collect();
                    if !top.is_empty() {
                        let line: Vec<String> =
                            top.iter().map(|(l, n)| format!("{}:{}万", l, n / 10000)).collect();
                        log::info!("SXDffle embed: GC 对象尺寸 {}", line.join(" · "));
                    }
                }
                // 存活字符串：字符缓冲是普通堆分配，GC 堆字节统计看不到它。
                // 这是「1681MB mimalloc 里只有 172MB 是 GcBox」的第一嫌疑。
                // 按大小档位的存活字节：直接回答"1.2GB 是小对象还是巨块"。
                {
                    let stats = crate::alloc_bucket_stats();
                    let mut parts: Vec<String> = Vec::new();
                    let mut total = 0usize;
                    for (i, (bytes, count)) in stats.iter().enumerate() {
                        total += *bytes;
                        if *count > 0 {
                            parts.push(format!(
                                "{}:{}个/{}MB",
                                crate::ALLOC_BUCKET_LABELS[i],
                                count,
                                bytes / (1024 * 1024)
                            ));
                        }
                    }
                    log::info!(
                        "SXDffle embed: 堆分配档位 合计 {}MB · {}",
                        total / (1024 * 1024),
                        parts.join(" · ")
                    );
                }
                let (swf_n, swf_comp, swf_raw) = ruffle_core::swf_load_stats();
                log::info!(
                    "SXDffle embed: SWF 载入 {} 个 · 压缩 {}MB → 解压 {}MB（整块常驻）",
                    swf_n,
                    swf_comp / (1024 * 1024),
                    swf_raw / (1024 * 1024)
                );
                // 分配归属：按执行阶段拆分存活字节，用来盯住那 800MB 未归因。
                {
                    let (t, _total) = crate::alloc_tag_stats();
                    let mut parts = Vec::new();
                    for (i, b) in t.iter().enumerate() {
                        if *b >= 8 * 1024 * 1024 {
                            parts.push(format!(
                                "{} {}MB",
                                crate::TAG_LABELS[i],
                                b / (1024 * 1024)
                            ));
                        }
                    }
                    if !parts.is_empty() {
                        log::info!(
                            "SXDffle embed: 大块归属(≥64K) {}",
                            parts.join(" · ")
                        );
                    }
                }
                // AVM2 指令热点：战报加载时 update 段单帧 1.4~2.2 秒，
                // 这行直接指出慢在属性访问、调用还是对象创建上。
                {
                    let ops = ruffle_core::Player::avm2_opstats();
                    let total: u64 = ops.iter().map(|(_, n, _)| *n).sum();
                    let total_us: u64 = ops.iter().map(|(_, _, t)| *t).sum();
                    if total > 0 && total_us > 0 {
                        // 同时给「次数占比」和「耗时占比」。
                        // 前者说明代码在干什么，后者说明时间花在哪 ——
                        // 两者差距越大，越说明那条指令背后挂着沉重的 native 工作。
                        let line: Vec<String> = ops
                            .iter()
                            .take(6)
                            .map(|(n, c, t)| {
                                format!(
                                    "{} {:.0}%/{:.0}%",
                                    n,
                                    *c as f64 * 100.0 / total as f64,
                                    *t as f64 * 100.0 / total_us as f64
                                )
                            })
                            .collect();
                        log::info!(
                            "SXDffle embed: AVM2 热点(次数/耗时) {}",
                            line.join(" · ")
                        );
                    }
                }
                // 上面是累计值；这一行才是此刻还活着的电影数据。
                // try_lock：这段代码在监控线程里跑，拿不到就跳过，不能阻塞渲染。
                if let Ok(p) = player.try_lock() {
                    let (mv_n, mv_b) = p.movie_resident_stats();
                    log::info!(
                        "SXDffle embed: SWF 数据常驻 {} 部 / {}MB（movie_libraries 只增不减）",
                        mv_n,
                        mv_b / (1024 * 1024)
                    );
                }
                // 大块分配来源：64K 以上的 1900 次分配合计 688MB，
                // 尺寸重复度能直接指认「同一类结构被反复分配」。
                let big = crate::big_alloc_top(10);
                if !big.is_empty() {
                    let line: Vec<String> = big
                        .iter()
                        .map(|(sz, n)| format!("{}KB×{}", sz / 1024, n))
                        .collect();
                    log::info!("SXDffle embed: 大块 TOP {}", line.join(" · "));
                }
                let (ld, ldb, ul, ulc, _) = ruffle_core::loader_stats::stats();
                log::info!(
                    "SXDffle embed: 加载/卸载 调用 {} 次 / 卸载 {} 次（其中有内容 {} 次）· Library 回收 {} 个 · SWF 载入 {} 个",
                    ld,
                    ul,
                    ulc,
                    ruffle_core::loader_stats::libraries_pruned(),
                    ruffle_core::swf_load_stats().0
                );
                let (rb_b, rb_n) = ruffle_core::Player::rebuildable_bitmaps();
                log::info!(
                    "SXDffle embed: 可重建位图 {} 个 / {}MB（自带 SWF 来源，可丢弃后重解码）",
                    rb_n,
                    rb_b / (1024 * 1024)
                );
                let (slot_b, slot_n) = ruffle_core::Player::slot_bytes();
                log::info!(
                    "SXDffle embed: 对象 slots {}MB（{} 个对象带字段表）",
                    slot_b / (1024 * 1024),
                    slot_n
                );
                let arr_bytes = ruffle_core::Player::array_bytes();
                log::info!(
                    "SXDffle embed: 数组堆外存储 {}MB（dense 元素缓冲）",
                    arr_bytes / (1024 * 1024)
                );
                let (str_live, str_bytes) = ruffle_core::Player::live_strings();
                log::info!(
                    "SXDffle embed: 存活字符串 {} 个 · 字符缓冲 {}MB",
                    str_live,
                    str_bytes / (1024 * 1024)
                );
                // mimalloc 自己的账：commit ≈ rss 说明没有空闲页可还。
                let (m_rss, m_peak, m_commit, m_faults) = mi_proc_info();
                log::info!(
                    "SXDffle mimalloc 账目: rss {}MB · 峰值 {}MB · commit {}MB · 缺页 {}",
                    m_rss,
                    m_peak,
                    m_commit,
                    m_faults
                );
                // 每 4 次采样（约 20 秒）打一次分配分布，避免刷屏。
                static MI_TICK: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                if MI_TICK.fetch_add(1, Ordering::Relaxed) % 4 == 0 {
                    log_mi_stats();
                    // 注意：log_heap_blocks() 已停用。
                    // mi_heap_visit_blocks 是真机启动即崩的元凶（外部 C 符号 ABI 不可靠），
                    // 在拿到可靠替代方案前不再调用。
                }
                // smaps 是全量文本解析（几千行），单独降到 60 秒一次。
                static LAST_SMAPS: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let now_s = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let prev = LAST_SMAPS.swap(now_s, Ordering::Relaxed);
                let top = if now_s.saturating_sub(prev) >= 60 {
                    mem_top_mappings(8)
                } else {
                    Vec::new()
                };
                if !top.is_empty() {
                    let line: Vec<String> = top
                        .iter()
                        .map(|(name, mb)| format!("{}={}MB", name, mb))
                        .collect();
                    log::info!("SXDffle embed: 内存映射 TOP {}", line.join(" · "));
                }
                // GPU 侧归因：真机证明显存（/dev/kgsl-3d0）能到 3.5GB，
                // 这里把「位图纹理」和「Shape 网格」分开计量，看谁是大头。
                let (tex_bytes, tex_count) = ruffle_render_wgpu::texture_gpu_stats();
                let (mesh_bytes, mesh_count) = ruffle_render_wgpu::mesh_gpu_stats();
                let tex_used = ruffle_render_wgpu::texture_used_count();
                log::info!(
                    "SXDffle embed: GPU 纹理冷热：曾被绘制 {} 个 / 存活 {} 个（未绘制 {} 个）",
                    tex_used,
                    tex_count,
                    tex_count.saturating_sub(tex_used)
                );
                let hist = ruffle_render_wgpu::texture_size_histogram();
                let tex_refs = ruffle_render_wgpu::texture_ref_count();
                let fnum = frames.load(Ordering::Relaxed).max(1);
                log::info!(
                    "SXDffle embed: 纹理引用 累计 {} 次 · 每帧平均 {} 次（存活 {} 个）",
                    tex_refs,
                    tex_refs / fnum as usize,
                    tex_count
                );
                log::info!(
                    "SXDffle embed: GPU 纹理尺寸分布 ≤32:{} ≤64:{} ≤128:{} ≤256:{} ≤512:{} >512:{}",
                    hist[0],
                    hist[1],
                    hist[2],
                    hist[3],
                    hist[4],
                    hist[5]
                );
                log::info!(
                    "SXDffle embed: GPU 位图纹理 {} 个/{}MB · Shape 网格 {} 个/{}MB",
                    tex_count,
                    tex_bytes / (1024 * 1024),
                    mesh_count,
                    mesh_bytes / (1024 * 1024)
                );
                // 把纹理拆成两类：SWF 静态美术（BitmapCharacter）与脚本 BitmapData。
                // 前者是 1.2GB 的主要来源，也是本轮降采样的对象。
                {
                    let (bm_n, bm_b) = ruffle_core::bitmap::bitmap_data::bitmap_stats();
                    let (art_n, art_b, art_saved) =
                        ruffle_core::bitmap::bitmap_data::art_texture_stats();
                    log::info!(
                        "SXDffle embed: 纹理归属 · SWF静态图 {} 个/{}MB（降采样省下 {}MB）· 脚本位图 {} 个/{}MB",
                        art_n,
                        art_b / (1024 * 1024),
                        art_saved / (1024 * 1024),
                        bm_n,
                        bm_b / (1024 * 1024)
                    );
                }
                let (sm_rss, sm_anon, sm_swap) = mem_smaps_mb();
                log::info!(
                    "SXDffle embed: 进程内存 RSS {}MB · 匿名 {}MB · Swap {}MB",
                    sm_rss,
                    sm_anon,
                    sm_swap
                );
                let (sk_packets, sk_bytes, sk_max) = ruffle_core::socket::socket_stats();
                log::info!(
                    "SXDffle embed: Socket 投递 {} 次 / 累计 {}KB / 单次最大 {}KB",
                    sk_packets,
                    sk_bytes / 1024,
                    sk_max / 1024
                );
                log::info!(
                    "SXDffle embed: 累计分配 ScriptObject {} 个 · 字符串 {} 个",
                    so_count,
                    str_count
                );
                if !obj_top.is_empty() {
                    let line: Vec<String> = obj_top
                        .iter()
                        .map(|(name, n)| format!("{}={}万", name, n / 10_000))
                        .collect();
                    log::info!("SXDffle embed: 新建对象 TOP {}", line.join(" "));
                }
                let (bm_count, bm_bytes) = ruffle_core::bitmap::bitmap_data::bitmap_stats();
                let (ds_count, ds_saved) =
                    ruffle_core::bitmap::bitmap_data::bitmap_downscale_stats();
                log::info!(
                    "SXDffle embed: 位图降采样 {} 张 · 累计省下 {}MB",
                    ds_count,
                    ds_saved / (1024 * 1024)
                );
                log::info!(
                    "SXDffle embed: 运行中 RSS {}MB · 已渲染 {} 帧 · GC {} 个/债务 {:.0} · 位图 {} 个/{}MB · 泵 {} 批",
                    mb,
                    frames.load(Ordering::Relaxed),
                    gc_count,
                    gc_debt,
                    bm_count,
                    bm_bytes / (1024 * 1024),
                    gc_pump_batches
                );
                // 运行期也要归还：一局页游每帧都会产生/丢弃整屏大小的纹理，
                // allocator 只把它们留在空闲链表里，RSS 会以 ~4MB/帧 的速度涨到 1.5GB，
                // 然后 C++ 侧分配失败 → SIGABRT。超过 800MB 就顺手 trim 一次。
                if mb > 800 {
                    trim_memory("运行中");
                }
            }

            // 常态化回收：每 20 秒、对象数超过 300 万时，连跑几轮完整 GC。
            //
            // 实测 gc-arena 的 sweep 是增量的：一次 1000 万债务只能扫掉一小部分
            // （945 万对象时只降 9%），而"紧急回收"连跑 6 轮能把 2136 万降到 169 万。
            // 也就是说，之前每分钟只跑一次的预算根本不够，垃圾一直堆在堆里。
            if last_full_gc.elapsed() >= Duration::from_secs(20) {
                last_full_gc = Instant::now();
                let count = {
                    let p = player.lock().unwrap();
                    p.gc_stats().0
                };
                if count > 3_000_000 {
                    let t0 = Instant::now();
                    let mut p = player.lock().unwrap();
                    let mut total_before = count;
                    let mut total_after = count;
                    for _ in 0..4 {
                        let (b, a) = p.force_full_gc();
                        total_before = b;
                        total_after = a;
                        if b == a {
                            break;
                        }
                    }
                    drop(p);
                    log::info!(
                        "SXDffle embed: 常态回收 GC {} → {} 个对象（耗时 {}ms）",
                        total_before,
                        total_after,
                        t0.elapsed().as_millis()
                    );
                    trim_memory("常态回收后");
                }
            }
        }

        // 6c. 睡到下一帧，或被新消息唤醒
        let timeout = if stop_at.is_some() {
            Duration::from_millis(30)
        } else {
            next_frame
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1))
        };

        match rx.recv_timeout(timeout) {
            Ok(msg) => {
                if !handle_msg(
                    msg,
                    &player,
                    &mut window,
                    &mut render_paused,
                    &mut zoom,
                    &mut view_w,
                    &mut view_h,
                    Some(&mut stop_at),
                    scale_factor,
                ) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // ---- 7. 收尾 ----
    //
    // 分阶段释放并各留一行日志：SIGABRT 常发生在 EGL/音频的析构里，
    // 只有知道"卡在 player 还是 Surface"，下次日志才能直接指认。
    let count_before = std::sync::Arc::strong_count(&player);
    log::info!(
        "SXDffle embed: 渲染循环结束，开始释放 player（引用计数={}）",
        count_before
    );

    // 收尾等待已在主循环里完成（Stop 后最多 6 秒，让在途请求自己结束）。
    // 这里只补一句日志：如果仍 >1，说明还有请求没返回，Player 会随它们一起
    // 延后释放 —— 这是可控的（不会崩），只是内存晚一点回收。
    if count_before > 1 {
        log::warn!(
            "SXDffle embed: 仍有 {} 个引用持有 player（加载未完成），将随请求结束释放",
            count_before - 1
        );
    }

    drop(player);
    log::info!("SXDffle embed: player 已释放，开始释放 Surface（RSS {}MB）", rss_mb());
    drop(window);
    log::info!("SXDffle embed: Surface 已释放");
    trim_memory("会话结束");
    Ok(())
}

/// 处理一条消息。返回 `false` 表示要退出循环。
fn handle_msg(
    msg: EmbedMsg,
    player: &Arc<Mutex<Player>>,
    window: &mut Option<AndroidSurfaceWindow>,
    render_paused: &mut bool,
    zoom: &mut f64,
    view_w: &mut u32,
    view_h: &mut u32,
    stop_at: Option<&mut Option<Instant>>,
    scale_factor: impl Fn(f64) -> f64,
) -> bool {
    match msg {
        EmbedMsg::Stop => {
            log::info!("SXDffle embed: 收到停止指令，进入收尾等待");
            crate::crash::set_stage(8);
            player.lock().unwrap().set_is_playing(false);
            if let Some(slot) = stop_at {
                *slot = Some(Instant::now());
            }
            return true;
        }
        EmbedMsg::Task(task) => {
            // 注意：这里**不能**持锁。future 内部会自行 lock Player，
            // 若外层已持有，同线程不可重入的 Mutex 会直接死锁。
            task.run();
        }
        EmbedMsg::Touch { phase, x, y } => {
            let event = match phase {
                PHASE_DOWN => PlayerEvent::MouseDown {
                    x,
                    y,
                    button: MouseButton::Left,
                    index: None,
                },
                PHASE_MOVE => PlayerEvent::MouseMove { x, y },
                _ => PlayerEvent::MouseUp {
                    x,
                    y,
                    button: MouseButton::Left,
                },
            };
            player.lock().unwrap().handle_event(event);
        }
        EmbedMsg::Scroll { dy } => {
            player
                .lock()
                .unwrap()
                .handle_event(PlayerEvent::MouseWheel {
                    delta: MouseWheelDelta::Pixels(dy),
                });
        }
        EmbedMsg::Key { down, tag } => {
            if let Some(desc) = crate::keycodes::key_tag_to_key_descriptor(&tag) {
                let event = if down {
                    PlayerEvent::KeyDown { key: desc }
                } else {
                    PlayerEvent::KeyUp { key: desc }
                };
                let mut p = player.lock().unwrap();
                p.handle_event(event);
                if down {
                    if let ruffle_core::events::LogicalKey::Character(c) = desc.logical_key {
                        p.handle_event(PlayerEvent::TextInput { codepoint: c });
                    }
                }
            }
        }
        EmbedMsg::Resize { width, height } => {
            let (w, h) = (width.max(1), height.max(1));
            if w == *view_w && h == *view_h {
                return true;
            }
            *view_w = w;
            *view_h = h;

            log::info!("SXDffle embed: 尺寸变更 {}x{}", w, h);
            let mut p = player.lock().unwrap();

            // 重建 swapchain：Android 的 Surface 在尺寸变化后，旧的 EGLSurface 已失效，
            // 必须重新创建，否则 present 会静默失败（画面冻结在旧帧）。
            if let Some(renderer) = <dyn Any>::downcast_mut::<WgpuRenderBackend<SwapChainTarget>>(
                p.renderer_mut(),
            ) {
                unsafe {
                    if let Err(e) = renderer.recreate_surface_unsafe(
                        wgpu::SurfaceTargetUnsafe::RawHandle {
                            raw_display_handle: Some(RawDisplayHandle::Android(
                                AndroidDisplayHandle::new(),
                            )),
                            raw_window_handle: match window.as_ref() {
                                Some(w) => w.handle(),
                                None => return true,
                            },
                        },
                        (w, h),
                    ) {
                        log::error!("SXDffle embed: 重建 surface 失败: {e}");
                    }
                }
            }

            p.set_viewport_dimensions(ViewportDimensions {
                width: w,
                height: h,
                scale_factor: scale_factor(*zoom),
            });
        }
        EmbedMsg::SetRenderPaused(paused) => {
            *render_paused = paused;
            log::info!(
                "SXDffle embed: 渲染{}（player 与网络连接保持）",
                if paused { "已暂停" } else { "已恢复" }
            );
        }
        EmbedMsg::AttachSurface(new_window) => {
            // 切回前台：Surface 是系统新建的，旧的 ANativeWindow 已经失效，
            // 必须用新的重建 swapchain，否则 present 会失败。
            let (w, h) = (new_window.width().max(1) as u32, new_window.height().max(1) as u32);
            log::info!("SXDffle embed: 挂载新 Surface {}x{}", w, h);
            *window = Some(new_window);

            let mut p = player.lock().unwrap();
            if let Some(renderer) =
                <dyn Any>::downcast_mut::<WgpuRenderBackend<SwapChainTarget>>(p.renderer_mut())
            {
                if let Some(wref) = window.as_ref() {
                    unsafe {
                        if let Err(e) = renderer.recreate_surface_unsafe(
                            wgpu::SurfaceTargetUnsafe::RawHandle {
                                raw_display_handle: Some(RawDisplayHandle::Android(
                                    AndroidDisplayHandle::new(),
                                )),
                                raw_window_handle: wref.handle(),
                            },
                            (w, h),
                        ) {
                            log::error!("SXDffle embed: 重建 surface 失败: {e}");
                        }
                    }
                }
            }
            *view_w = w;
            *view_h = h;
            p.set_viewport_dimensions(ViewportDimensions {
                width: w,
                height: h,
                scale_factor: scale_factor(*zoom),
            });
        }
        EmbedMsg::SetStageSize(w, h) => {
            let (tw, th) = if w == 0 || h == 0 {
                // 恢复 SWF 自带尺寸
                let mut p = player.lock().unwrap();
                (p.movie_width(), p.movie_height())
            } else {
                (w, h)
            };
            apply_stage_size(player, tw, th);
        }
        EmbedMsg::TrimMemory => {
            let mut p = player.lock().unwrap();
            let (before, _) = p.gc_stats();
            p.force_gc();
            let (after, _) = p.gc_stats();
            drop(p);
            log::info!(
                "SXDffle embed: 系统内存紧张，强制 GC {} → {} 个对象",
                before,
                after
            );
            trim_memory("系统内存紧张");
            // 再主动把匿名页压进 zram：mi_collect 只能还「完全空闲」的页，
            // 而系统要的是「RSS 变小」，PAGEOUT 能压走**还在用但暂时不需要**的页。
            {
                let before = rss_mb();
                let (n, bytes, failed) = pageout_anon_pages(256 * 1024);
                log::warn!(
                    "SXDffle embed: 压出匿名页 {} 段 / {}MB（失败 {} 段）· RSS {}MB → {}MB",
                    n,
                    bytes / (1024 * 1024),
                    failed,
                    before,
                    rss_mb()
                );
            }
            // 只有在「系统主动要求释放」时才做分配器的完整归还。
            //
            // 原因：mimalloc 会把已释放的页留在自己的 segment 里不还给内核
            // （真机实测：GC 堆 184MB + 位图 186MB，而 [anon:mimalloc] 高达 1760MB，
            // 其中约 1.4GB 是空闲页）。这正是「离开场景/断线后内存不掉、
            // 切后台才掉」的原因 —— 掉的那部分就是系统强制回收的。
            //
            // mi_collect(true) 会锁住所有线程的堆、耗时可达数秒，
            // 所以绝不能周期性调用（0.261068 就是这样把加载流程卡死的），
            // 只放在 onTrimMemory 这种「本来就在后台/内存紧张」的时刻。
            unsafe { libmimalloc_sys::mi_collect(true) };
            trim_memory("分配器归还后");
        }
        EmbedMsg::ExternalCallSync { name, args, reply } => {
            let values = parse_external_args(&args);
            let mut p = player.lock().unwrap();
            let result = p.call_internal_interface(&name, values);
            let json = serde_json::to_string(&external_to_json(&result))
                .unwrap_or_else(|_| "null".to_string());
            let _ = reply.send(json);
        }
        EmbedMsg::ExternalCall { name, args } => {
            let values = parse_external_args(&args);
            let mut p = player.lock().unwrap();
            let _ = p.call_internal_interface(&name, values);
            log::info!("SXDffle embed: JS 调用回调 {}", name);
        }
        EmbedMsg::SetZoom(z) => {
            let z = z.clamp(0.4, 4.0);
            if (z - *zoom).abs() < f64::EPSILON {
                return true;
            }
            *zoom = z;
            log::info!("SXDffle embed: 缩放 -> {:.2}x", z);
            player.lock().unwrap().set_viewport_dimensions(ViewportDimensions {
                width: *view_w,
                height: *view_h,
                scale_factor: scale_factor(z),
            });
        }
    }
    true
}

/// 覆盖舞台尺寸并重建矩阵。
///
/// `set_movie_size` 只写字段、不触发重排，必须紧跟 `build_matrices`，
/// 否则 `Stage.stageWidth` 变了但画面还是旧布局。
fn apply_stage_size(player: &Arc<Mutex<Player>>, w: u32, h: u32) {
    if w == 0 || h == 0 {
        return;
    }
    let mut p = match player.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    p.mutate_with_update_context(|ctx| {
        let stage = ctx.stage;
        stage.set_movie_size(w, h);
        stage.build_matrices(ctx);
    });
    log::info!("SXDffle embed: 舞台尺寸 -> {}x{}", w, h);
}

// ---------------------------------------------------------------------------
// 对外操作（供 JNI 层调用）
// ---------------------------------------------------------------------------

impl EmbedSession {
    pub fn resize(&self, width: u32, height: u32) {
        self.send(EmbedMsg::Resize { width, height });
    }

    pub fn touch(&self, phase: u8, x: f32, y: f32) {
        self.send(EmbedMsg::Touch {
            phase,
            x: x as f64,
            y: y as f64,
        });
    }

    pub fn scroll(&self, dy: f32) {
        self.send(EmbedMsg::Scroll { dy: dy as f64 });
    }

    pub fn key(&self, down: bool, tag: String) {
        self.send(EmbedMsg::Key { down, tag });
    }

    pub fn set_zoom(&self, zoom: f32) {
        self.send(EmbedMsg::SetZoom(zoom as f64));
    }

    /// 运行时修改舞台尺寸；传 0,0 恢复 SWF 自带值。
    pub fn set_stage_size(&self, w: u32, h: u32) {
        self.send(EmbedMsg::SetStageSize(w, h));
    }

    /// 暂停/恢复渲染。暂停期间 player 与网络连接保持活着。
    pub fn set_render_paused(&self, paused: bool) {
        self.send(EmbedMsg::SetRenderPaused(paused));
    }

    /// 切回前台后挂上新 Surface。
    pub fn attach_surface(&self, window: AndroidSurfaceWindow) {
        self.send(EmbedMsg::AttachSurface(window));
    }

    /// 系统内存紧张时调用：跑一次 GC 并把空闲页还给内核。
    pub fn trim_memory_now(&self) {
        self.send(EmbedMsg::TrimMemory);
    }

    /// 网页 JS 调用 SWF 注册的回调（ExternalInterface.addCallback）。
    pub fn external_call(&self, name: String, args: String) {
        self.send(EmbedMsg::ExternalCall { name, args });
    }

    /// 同步版本：等引擎把返回值算出来再返回（最长 300ms，超时返回 null）。
    ///
    /// 页面常用 `while (!swf_is_load())` 这种轮询，没有返回值就会一直判定未加载。
    pub fn external_call_sync(&self, name: String, args: String) -> String {
        let (tx, rx) = channel();
        self.send(EmbedMsg::ExternalCallSync {
            name,
            args,
            reply: tx,
        });
        rx.recv_timeout(Duration::from_millis(300))
            .unwrap_or_else(|_| "null".to_string())
    }
}
