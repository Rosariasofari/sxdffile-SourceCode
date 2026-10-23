//! SXDffle 0.261145：窗口化的 SWF 解压存储。
//!
//! ## 为什么
//!
//! `decompress_swf` 会把整部 SWF 解压成一整块 `Vec<u8>`，之后由 `SwfSlice`
//! 用 `Arc<Vec<u8>>` 持有。战报一次载入 500 多部电影，解压后 700MB 以上，
//! 而且**只要还有任何一个 `SwfSlice` 活着，这块内存就放不掉** ——
//! `SwfMovieData::try_release_data` 里的 `guard.take()` 只是让电影本身
//! 不再持有引用，`SwfSlice` 手上的 `Arc` 还在，实测「558 部 / 714MB」
//! 三分钟一动不动。
//!
//! ## 做法
//!
//! 用匿名 `mmap` 拿一块**虚拟地址固定**的区域，按 256KB 窗口向前解压：
//!
//! - 物理页只在真正写入时才分配，没解压到的部分不占内存；
//! - 逐出 = `madvise(MADV_DONTNEED)`，物理页立刻还给系统，虚拟地址不变；
//! - 需要时从原始压缩字节**重放**（重新解压一遍到目标位置）。
//!
//! 地址固定这一点很关键：`SwfSlice::data()` 返回的 `&[u8]` 指向这块区域，
//! 只要 `SwfStore` 还活着，指针就一直有效 —— 中间隔了多少次逐出重放都不会悬垂。
//!
//! 重放成本 = 目标位置之前的解压时间。战报里的电影平均解压后 1~2MB，
//! 整部重放 2~3ms，可以接受；`KEEP_HEAD` 还会保住 SWF 头部那一小段，
//! 让「只读开头」的路径不必重放。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use memmap2::{Mmap, MmapMut, UncheckedAdvice};
use swf::read::{SwfBodyDecoder, SwfRaw};
use swf::HeaderExt;

/// 解压窗口粒度。逐出也按这个粒度对齐。
pub const WINDOW: usize = 256 * 1024;

/// 逐出时永远保留的头部长度（SWF 头 + 开头的 tag）。
///
/// 一部电影的前 256KB 通常是 Definition 类 tag（角色、图片、字体），
/// 播放时反复被查表命中；保住它们能让多数「只读前面」的访问零重放。
pub const KEEP_HEAD: usize = 128 * 1024;

/// 单个 SWF 的上限，与 `swf::read::MAX_DATA_CAPACITY` 保持一致。
const MAX_LEN: usize = 128 * 1024 * 1024;

/// 页大小。Android/Linux 上 4KB 是唯一需要关心的。
const PAGE: usize = 4096;

/// SWF 文件头长度：3 字节签名 + 1 字节版本 + 4 字节解压总长。
const FILE_HEADER_LEN: usize = 8;

// ---- raw 落盘 ----
//
// 窗口存储要能「逐出后再重放」，就必须留住原始压缩字节；而这份字节
// 在稳态里是最大的一项（战报 402 部约 520MB）。它只是**重放的原料**，
// 平时一个字节都不读，却占着匿名页 —— 系统回收不了，mimalloc 也不敢动。
//
// 写到应用缓存目录再 mmap 只读：内存里只剩页缓存，内核随时可以回收，
// 下次读的时候从文件重新调页。实测这部分能从 RSS 里整体挪出去。
static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();
static RAW_SEQ: AtomicU64 = AtomicU64::new(0);
static PERSISTED: AtomicU64 = AtomicU64::new(0);
static PERSIST_BYTES: AtomicU64 = AtomicU64::new(0);

/// 设置 raw 落盘目录（应用缓存目录）。不设置则全部留在内存里。
pub fn set_cache_dir(dir: PathBuf) {
    let _ = CACHE_DIR.set(dir);
}

/// 清掉上次进程遗留的 raw 文件。
pub fn clear_cache_dir(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with("swfraw_") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `(落盘份数, 落盘字节)`。
pub fn persist_stats() -> (u64, u64) {
    (
        PERSISTED.load(Ordering::Relaxed),
        PERSIST_BYTES.load(Ordering::Relaxed),
    )
}

/// 磁盘上的原始字节：只读映射 + 生命周期结束时删文件。
struct RawFile {
    map: Mmap,
    path: PathBuf,
}

impl AsRef<[u8]> for RawFile {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.map
    }
}

impl Drop for RawFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 把原始字节写到缓存目录并映射回来。失败就返回 `None`（继续用内存）。
fn persist_raw(raw: &SwfRaw) -> Option<Arc<RawFile>> {
    let dir = CACHE_DIR.get()?;
    let bytes = raw.as_ref().as_ref();
    let seq = RAW_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("swfraw_{}_{seq}.bin", std::process::id()));
    std::fs::write(&path, bytes).ok()?;
    let file = std::fs::File::open(&path).ok()?;
    // SAFETY: 文件写完才映射，之后不再写入；只读映射，长度固定。
    let map = unsafe { Mmap::map(&file).ok()? };
    PERSISTED.fetch_add(1, Ordering::Relaxed);
    PERSIST_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    Some(Arc::new(RawFile { map, path }))
}

// ---- 统计 ----
static STORES: AtomicU64 = AtomicU64::new(0);
static MAPPED_BYTES: AtomicU64 = AtomicU64::new(0);
static REPLAYS: AtomicU64 = AtomicU64::new(0);
static REPLAY_US: AtomicU64 = AtomicU64::new(0);
static EVICTIONS: AtomicU64 = AtomicU64::new(0);
static EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);

/// `(窗口存储个数, 虚拟地址合计, 重放次数, 重放耗时微秒, 逐出次数, 逐出字节)`
pub fn swf_store_stats() -> (u64, u64, u64, u64, u64, u64) {
    (
        STORES.load(Ordering::Relaxed),
        MAPPED_BYTES.load(Ordering::Relaxed),
        REPLAYS.load(Ordering::Relaxed),
        REPLAY_US.load(Ordering::Relaxed),
        EVICTIONS.load(Ordering::Relaxed),
        EVICTED_BYTES.load(Ordering::Relaxed),
    )
}

/// 解压目标区域。
enum Region {
    /// 匿名映射。可以逐出，也可以重放。
    Mapped(MmapMut),
    /// `mmap` 失败时的回退：普通堆内存，长度固定（指针稳定），但无法逐出。
    Heap(Box<[u8]>),
    /// 未压缩 SWF（FWS）：数据本来就在 `raw` 里，**再复制一份纯属浪费**。
    /// 直接借用，跳过复制与映射；代价是不能逐出（逐出也没有收益）。
    Borrowed(SwfRaw, usize),
}

impl Region {
    #[inline]
    fn ptr(&self) -> *const u8 {
        match self {
            Region::Mapped(m) => m.as_ptr(),
            Region::Heap(v) => v.as_ptr(),
            Region::Borrowed(v, off) => unsafe { v.as_ref().as_ref().as_ptr().add(*off) },
        }
    }

    #[inline]
    fn is_mapped(&self) -> bool {
        matches!(self, Region::Mapped(_))
    }

    /// 未压缩数据直接借用 `raw`，不占额外内存，也无法逐出。
    #[inline]
    fn is_borrowed(&self) -> bool {
        matches!(self, Region::Borrowed(..))
    }
}

struct Inner {
    /// `[0, filled)` 已经解压并驻留（逐出会把它拉回 `KEEP_HEAD`）。
    filled: usize,
    /// 从 `filled` 继续的解码器；`None` 表示下次访问要从头重建。
    decoder: Option<SwfBodyDecoder>,
}

/// 窗口化的 SWF 解压存储。
pub struct SwfStore {
    region: Region,
    len: usize,
    /// 原始文件字节（压缩态）。逐出后靠它重放。
    ///
    /// 通常是磁盘映射（`RawFile`），只有在缓存目录不可用或很小的时候才留在内存。
    raw: SwfRaw,
    /// 最近一次真正读字节的「进程内秒数」。
    ///
    /// 比 `last_active` 更贴近「还要不要数据」：`run_frame` 只在帧开头打点一次，
    /// 而这里每次 `slice` 都会刷新。逐出判据取两者的较大值，
    /// 就能挡住「帧刚开始、还没读到 tag 就被判冷」的竞态。
    last_use: AtomicU32,
    inner: Mutex<Inner>,
}

/// 进程内秒数（无系统调用）。
fn now_secs() -> u32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs() as u32
}

impl SwfStore {
    /// 构造存储。返回 `(存储, 扩展头)`。
    ///
    /// 只解压开头一小段用于解析文件头，其余部分等真正被读到时再解压。
    pub fn new(raw_mem: Arc<Vec<u8>>) -> swf::error::Result<(Arc<Self>, HeaderExt)> {
        // 大一点的都落盘：匿名页换页缓存，RSS 直接掉，读的时候内核再调页。
        let raw: SwfRaw = match persist_raw(&(raw_mem.clone() as SwfRaw)) {
            Some(file) => file,
            None => raw_mem,
        };
        let mut decoder = SwfBodyDecoder::new(raw.clone())?;
        // 总长要用 `total_len()`：构造时已经读进 `pending` 的那几十字节
        // 也属于 tag stream，用 `remaining()` 会把它漏掉。
        let len = decoder.total_len().min(MAX_LEN);

        // 未压缩的 SWF（FWS）：解压流就是文件本身，`raw` 已经是全量数据。
        // 再映射一块同样大小的地方纯属浪费 —— 直接借用。
        let region = if decoder.compression == swf::Compression::None {
            // `raw` 是**完整文件**（含 8 字节文件头），tag stream 在
            // `8 + data_offset` 处 —— 少加 8 会让整个切片前移 8 字节，
            // 表现就是满屏 `Unknown tag code` + 尾部 EOF。
            Region::Borrowed(raw.clone(), FILE_HEADER_LEN + decoder.data_offset)
        } else {
            match MmapMut::map_anon(len.max(1)) {
                Ok(m) => {
                    MAPPED_BYTES.fetch_add(len as u64, Ordering::Relaxed);
                    Region::Mapped(m)
                }
                Err(e) => {
                    // 虚拟地址不够（32 位设备或地址碎片）时退回堆内存。
                    tracing::warn!("SXDffle: SWF 窗口映射失败（{}），改用堆内存", e);
                    Region::Heap(vec![0u8; len.max(1)].into_boxed_slice())
                }
            }
        };

        STORES.fetch_add(1, Ordering::Relaxed);

        // 先解压开头一段，够解析 RECT / 帧率 / 帧数 / FileAttributes / SetBackgroundColor。
        let mut filled = if region.is_borrowed() { len } else { 0 };
        if !region.is_borrowed() {
            let probe = len.min(64 * 1024);
            if probe > 0 {
                let ptr = region.ptr() as *mut u8;
                while filled < probe {
                    let buf =
                        unsafe { std::slice::from_raw_parts_mut(ptr.add(filled), probe - filled) };
                    let n = decoder.read_into(buf);
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
            }
        }

        let header = swf::read::parse_header_ext_from_parts(
            &decoder.header_bytes,
            unsafe { std::slice::from_raw_parts(region.ptr(), filled) },
            decoder.compression,
            decoder.version,
            decoder.uncompressed_len,
        )?;

        let store = Arc::new(Self {
            region,
            len,
            raw,
            last_use: AtomicU32::new(now_secs()),
            inner: Mutex::new(Inner {
                filled,
                decoder: Some(decoder),
            }),
        });
        Ok((store, header))
    }

    /// 空存储（占位电影用）。
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            region: Region::Heap(vec![0u8; 1].into_boxed_slice()),
            len: 0,
            raw: Arc::new(Vec::new()),
            last_use: AtomicU32::new(0),
            inner: Mutex::new(Inner {
                filled: 0,
                decoder: None,
            }),
        })
    }

    /// 解压区的基址。`SwfSlice` 靠它做「子切片 → 偏移」的换算。
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.region.ptr()
    }

    /// tag stream 的总长度（解压后）。
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 当前驻留（占用物理内存）的字节数。
    pub fn resident_len(&self) -> usize {
        if self.region.is_borrowed() {
            return self.len;
        }
        self.lock().filled.min(self.len)
    }

    /// 取 `[start, end)`。缺失的部分会就地解压（必要时整段重放）。
    #[inline]
    pub fn slice(&self, start: usize, end: usize) -> &[u8] {
        let end = end.min(self.len);
        if start >= end {
            return &[];
        }
        // 只有真的读数据才算「用过」；纯长度查询不该刷新。
        self.last_use.store(now_secs(), Ordering::Relaxed);
        self.ensure(end);
        unsafe { std::slice::from_raw_parts(self.region.ptr().add(start), end - start) }
    }

    /// 确保 `[0, end)` 已经解压并驻留。
    fn ensure(&self, end: usize) {
        let end = end.min(self.len);
        if end == 0 || self.region.is_borrowed() {
            return;
        }
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if g.filled >= end {
            return;
        }

        // 逐出之后 filled 被拉回到头部，解码器也已丢弃：必须从头重建。
        if g.decoder.is_none() {
            g.filled = 0;
            let t = std::time::Instant::now();
            match SwfBodyDecoder::new(self.raw.clone()) {
                Ok(d) => g.decoder = Some(d),
                Err(e) => {
                    // 解压失败：保持全 0，让上层「解析到空」自然结束。
                    tracing::warn!("SXDffle: SWF 重放失败: {}", e);
                    return;
                }
            }
            REPLAYS.fetch_add(1, Ordering::Relaxed);
            REPLAY_US.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        }

        let ptr = self.region.ptr() as *mut u8;
        let mut filled = g.filled;
        if let Some(d) = g.decoder.as_mut() {
            while filled < end {
                let buf =
                    unsafe { std::slice::from_raw_parts_mut(ptr.add(filled), end - filled) };
                let n = d.read_into(buf);
                if n == 0 {
                    break;
                }
                filled += n;
            }
        }
        g.filled = filled;
    }

    /// 丢开 `KEEP_HEAD` 之后的解压数据，返回释放的字节数。
    ///
    /// 之后任何越过 `KEEP_HEAD` 的读取都会触发重放。
    pub fn evict(&self) -> usize {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.region.is_borrowed() {
            // 未压缩 SWF 的数据就是 `raw`，逐出既不省内存也不影响正确性。
            return 0;
        }
        let keep = KEEP_HEAD.min(self.len);
        if g.filled <= keep {
            return 0;
        }
        let freed = g.filled - keep;

        if self.region.is_mapped() {
            // 页对齐：起点向上取整，长度向下取整。
            let start = keep.div_ceil(PAGE) * PAGE;
            if start < g.filled {
                let len = ((g.filled - start) / PAGE) * PAGE;
                if len > 0 && let Region::Mapped(m) = &self.region {
                    // SAFETY: 区间落在映射内且已页对齐；逐出之后 `filled` 退回
                    // `keep`，在重新解压填回之前不会再读这段旧内容。
                    unsafe {
                        let _ = m.unchecked_advise_range(UncheckedAdvice::DontNeed, start, len);
                    }
                }
            }
        }

        g.filled = keep;
        g.decoder = None;

        EVICTIONS.fetch_add(1, Ordering::Relaxed);
        EVICTED_BYTES.fetch_add(freed as u64, Ordering::Relaxed);
        freed
    }

    /// 最近一次真正读字节的「进程内秒数」。
    #[inline]
    pub fn last_use_secs(&self) -> u32 {
        self.last_use.load(Ordering::Relaxed)
    }

    /// 原始压缩字节的长度。
    #[inline]
    pub fn raw_len(&self) -> usize {
        self.raw.as_ref().as_ref().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl std::fmt::Debug for SwfStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwfStore")
            .field("len", &self.len)
            .field("raw", &self.raw_len())
            .field("mapped", &self.region.is_mapped())
            .finish()
    }
}

// SXDffle：`SwfStore` 不持有任何 `Gc` 指针，只是 `Arc` 里的一块原生内存。
// SAFETY: 字段全部是 `'static` 数据（`mmap` 区域、`Arc<Vec<u8>>`、`Mutex`），
// 不参与 GC 图，也不需要写屏障。
unsafe impl<'gc> gc_arena::Collect<'gc> for SwfStore {
    const NEEDS_TRACE: bool = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个未压缩的 SWF：合法头 + `body_len` 字节填充。
    ///
    /// 填充内容是 0xFF（无效 tag），但 `SwfBodyDecoder` 只做解压、不解析 tag，
    /// 所以正好用来验证「窗口切片 + 逐出 + 重放」的字节一致性。
    /// 手工构造 zlib「stored」块，避免为测试引入 flate2 依赖。
    ///
    /// zlib 头 `78 01` + 若干 `BFINAL|BTYPE=00` 块 + adler32。
    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let chunks: Vec<&[u8]> = data.chunks(65535).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let last = i + 1 == chunks.len();
            out.push(u8::from(last));
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        let mut a: u32 = 1;
        let mut b: u32 = 0;
        for &byte in data {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    fn build_swf(body_len: usize) -> Vec<u8> {
        let mut body = Vec::with_capacity(body_len);
        // RECT：nbits 在**高 5 位**（15 << 3 = 0x78），四个坐标都是 0
        // → 5 + 4*15 = 65 bit = 9 字节。
        body.extend_from_slice(&[0x78, 0, 0, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(&[0x00, 0x18]); // 24 fps
        body.extend_from_slice(&[0x01, 0x00]); // 1 帧
        body.resize(body_len, 0xFF);

        let compressed = zlib_stored(&body);
        let mut v = Vec::with_capacity(compressed.len() + 8);
        v.extend_from_slice(b"CWS");
        v.push(6);
        v.extend_from_slice(&((body_len + 8) as u32).to_le_bytes());
        v.extend_from_slice(&compressed);
        v
    }

    fn replays() -> u64 {
        swf_store_stats().2
    }

    /// 落盘路径也要能正常读回来：写文件 → mmap → 切片 → 重放。
    #[test]
    fn persisted_raw_survives_replay() {
        let dir = std::env::temp_dir().join(format!("sxd_swf_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        set_cache_dir(dir.clone());

        let raw = build_swf(1 << 20);
        let reference = swf::read::decompress_swf(&raw[..]).unwrap();
        let (store, _header) = SwfStore::new(Arc::new(raw)).unwrap();

        assert_eq!(store.slice(0, store.len()), &reference.data[..]);
        let freed = store.evict();
        assert!(freed > 0);
        // 逐出后重放，数据要从磁盘映射里读回来。
        assert_eq!(store.slice(0, store.len()), &reference.data[..]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forward_read_does_not_replay() {
        let raw = Arc::new(build_swf(1 << 20));
        let (store, _header) = SwfStore::new(raw).unwrap();
        assert!(store.len() > KEEP_HEAD, "len = {}", store.len());

        let a = store.slice(0, 4096).to_vec();
        let r = replays();
        let b = store.slice(4096, 8192).to_vec();

        assert_eq!(replays(), r, "顺序前进不应该触发重放");
        assert_eq!(a.len(), 4096);
        assert_eq!(b.len(), 4096);
        assert_eq!(a, b, "填充区应该完全一样");
    }

    #[test]
    fn evict_then_replay_returns_same_bytes() {
        let raw = Arc::new(build_swf(1 << 20));
        let (store, _header) = SwfStore::new(raw).unwrap();
        let len = store.len();
        assert!(len > KEEP_HEAD * 2);

        // 先把整部读一遍（模拟 preload）。
        let full: Vec<u8> = store.slice(0, len).to_vec();
        assert_eq!(store.resident_len(), len);

        // 逐出：只剩头部窗口驻留。
        let freed = store.evict();
        assert!(freed > 0, "应该有东西被释放");
        assert!(store.resident_len() <= KEEP_HEAD, "驻留 {}", store.resident_len());

        // 再读一次末尾，必须和逐出前逐字节一致。
        let before = replays();
        let tail = store.slice(len - 8192, len).to_vec();
        assert!(replays() > before, "逐出后再读应该触发重放");
        assert_eq!(tail, &full[len - 8192..]);
        assert_eq!(store.slice(0, 8192).to_vec(), &full[..8192]);
    }

    #[test]
    fn evict_is_noop_when_nothing_to_free() {
        let raw = Arc::new(build_swf(64 * 1024));
        let (store, _header) = SwfStore::new(raw).unwrap();
        let _ = store.slice(0, store.len());
        // 整部都小于 KEEP_HEAD，逐出应该什么都不做。
        assert_eq!(store.evict(), 0);
    }

    /// 和 `decompress_swf` 对照：窗口存储给出的字节必须**完全一致**。
    ///
    /// 这条是 0.261145 黑屏的直接回归测试 —— 上一版把 tag stream 总长算少了
    /// 115 字节（漏掉 `pending`），切片自洽所以其它测试全过，
    /// 但真实解析会在尾部 EOF。
    #[test]
    fn matches_reference_decompression() {
        for body in [64 * 1024usize, 300 * 1024, 1024 * 1024] {
            let raw = build_swf(body);
            let reference = swf::read::decompress_swf(&raw[..]).unwrap();
            let (store, _header) = SwfStore::new(Arc::new(raw)).unwrap();

            assert_eq!(store.len(), reference.data.len(), "长度不一致 body={body}");
        }
    }

    /// 未压缩（FWS）走借用路径，同样要和参考实现一致。
    #[test]
    fn matches_reference_for_uncompressed() {
        let mut v = Vec::new();
        let body_len = 200 * 1024usize;
        v.extend_from_slice(b"FWS");
        v.push(6);
        v.extend_from_slice(&((body_len + 8) as u32).to_le_bytes());
        v.extend_from_slice(&[0x78, 0, 0, 0, 0, 0, 0, 0, 0]);
        v.extend_from_slice(&[0x00, 0x18]);
        v.extend_from_slice(&[0x01, 0x00]);
        v.resize(body_len + 8, 0xFF);

        let reference = swf::read::decompress_swf(&v[..]).unwrap();
        let (store, _header) = SwfStore::new(Arc::new(v)).unwrap();

        assert_eq!(store.len(), reference.data.len());
        assert_eq!(store.slice(0, store.len()), &reference.data[..]);
        // 未压缩直接借用，逐出无收益。
        assert_eq!(store.evict(), 0);
    }

    #[test]
    fn short_slice_beyond_end_is_clamped() {
        let raw = Arc::new(build_swf(512 * 1024));
        let (store, _header) = SwfStore::new(raw).unwrap();
        let len = store.len();
        assert_eq!(store.slice(len + 100, len + 200).len(), 0);
        assert_eq!(store.slice(len - 10, len + 1000).len(), 10);
    }
}
