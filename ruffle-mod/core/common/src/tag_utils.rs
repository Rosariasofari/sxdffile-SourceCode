use crate::sandbox::SandboxType;
use crate::swf_store::SwfStore;

use gc_arena::{Collect, Gc, GcWeak, Mutation};
use std::fmt::{Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;
use swf::{Fixed8, HeaderExt, Rectangle, Twips};
use url::Url;

pub type SwfStream<'a> = swf::read::Reader<'a>;

/// A shared pointer to a `SwfMovieData`.
#[derive(Clone, Collect, Copy, Debug)]
#[collect(no_drop)]
pub struct SwfMovie<'gc>(Gc<'gc, SwfMovieData>);

// ---- SXDffle: SWF 载入体量统计 ----
//
// `SwfMovieData.data` 是**解压后的完整 SWF**，整块常驻、不受 GC 管理。
// 真机实测：启动 3 秒内 RSS 从 281MB 涨到 769MB，而当时 GC 堆只有 90MB、
// 位图 0MB —— 这块差额只能来自 SWF 解压数据与解析结构。
static SWF_LOADED_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static SWF_LOADED_COMPRESSED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static SWF_LOADED_RAW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 返回 (载入的 SWF 个数, 压缩字节合计, 解压字节合计)。
pub fn swf_load_stats() -> (usize, usize, usize) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        SWF_LOADED_COUNT.load(Relaxed),
        SWF_LOADED_COMPRESSED.load(Relaxed),
        SWF_LOADED_RAW.load(Relaxed),
    )
}

impl<'gc> SwfMovie<'gc> {
    pub fn new(mc: &Mutation<'gc>, data: SwfMovieData) -> Self {
        if data.is_movie && data.data_len() > 0 {
            use std::sync::atomic::Ordering::Relaxed;
            SWF_LOADED_COUNT.fetch_add(1, Relaxed);
            SWF_LOADED_COMPRESSED.fetch_add(data.compressed_len(), Relaxed);
            SWF_LOADED_RAW.fetch_add(data.data_len(), Relaxed);
            tracing::info!(
                "SXDffle SWF 载入: {} 压缩 {}KB → 解压 {}KB（窗口化）",
                data.url(),
                data.compressed_len() / 1024,
                data.data_len() / 1024
            );
        }
        SwfMovie(Gc::new(mc, data))
    }

    pub fn downgrade(self) -> SwfMovieWeak<'gc> {
        SwfMovieWeak(Gc::downgrade(self.0))
    }

    pub fn as_ptr(self) -> *const SwfMovieData {
        Gc::as_ptr(self.0)
    }

    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        std::ptr::eq(this.as_ptr(), other.as_ptr())
    }
}

impl<'gc> Deref for SwfMovie<'gc> {
    type Target = SwfMovieData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PartialEq for SwfMovie<'_> {
    fn eq(&self, other: &Self) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl Eq for SwfMovie<'_> {}

impl Hash for SwfMovie<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(self.0).hash(state);
    }
}

/// Like `SwfMovie`, but is a weak reference.
#[derive(Clone, Collect, Copy, Debug)]
#[collect(no_drop)]
pub struct SwfMovieWeak<'gc>(GcWeak<'gc, SwfMovieData>);

impl<'gc> SwfMovieWeak<'gc> {
    pub fn upgrade(&self, mc: &Mutation<'gc>) -> Option<SwfMovie<'gc>> {
        self.0.upgrade(mc).map(SwfMovie)
    }

    /// 底层对象是否已被 GC 回收。
    ///
    /// SXDffle：`MovieLibraries` 用它判「这部电影已经没人引用了」，
    /// 进而在不触发 panic 的前提下回收整条 Library。
    pub fn is_dropped(self) -> bool {
        self.0.is_dropped()
    }
}


/// An open, fully parsed SWF movie ready to play back, either in a Player or a
/// MovieClip.
#[derive(Clone, Collect)]
#[collect(require_static)]
pub struct SwfMovieData {
    /// The SWF header parsed from the data stream.
    header: HeaderExt,

    /// Uncompressed SWF data.
    ///
    /// SXDffle 0.261145：改成窗口化的 `SwfStore`。
    ///
    /// 原来是 `Arc<Mutex<Option<Arc<Vec<u8>>>>>` —— 一整块解压数据。
    /// 问题在于 `SwfSlice` 也持有那个 `Arc`，所以「电影丢字节」只是把
    /// `SwfMovieData` 手上那份引用去掉，内存并不会还给系统：
    /// 实测战报里 558 部 / 714MB 三分钟一动不动，只有整个 MovieClip 被回收时才降。
    ///
    /// `SwfStore` 只保留压缩字节 + 一块可逐出的映射区，`SwfSlice` 持有的是
    /// 这个存储的 `Arc`（小对象），所以字节可以随时丢、需要时再解压回来。
    store: Arc<SwfStore>,
    /// 该电影是否已经整片 preload 完成（完成前绝不能丢字节）。
    loaded: Arc<std::sync::atomic::AtomicBool>,
    /// 最近一次被库访问的「进程内秒数」，用于判断「冷」。
    last_used: Arc<std::sync::atomic::AtomicU32>,
    /// 最近一次时间轴推进 / 帧构造的「进程内秒数」。
    ///
    /// 这是真正决定「字节还能不能丢」的信号：一个还在播放的 MovieClip
    /// 每帧都会从 `swf_data` 里读 tag（Place/Remove/帧脚本），
    /// 而它**不会**去查库表 —— 只用 `last_used` 判断会把正在播的动画的字节丢掉。
    last_active: Arc<std::sync::atomic::AtomicU32>,

    /// The URL the SWF was downloaded from.
    url: String,

    /// The URL that triggered the SWF load.
    loader_url: Option<String>,

    /// Any parameters provided when loading this movie (also known as 'flashvars'),
    /// as a list of key-value pairs.
    parameters: Vec<(String, String)>,

    /// The suggest encoding for this SWF.
    encoding: &'static swf::Encoding,

    /// The compressed length of the entire datastream
    compressed_len: usize,

    /// Whether this SwfMovieData actually represents a loaded movie or fills in for
    /// something else, like an loaded image, filler movie, or error state.
    is_movie: bool,

    /// Whether this movie should be interpreted as AVM1, regardless of what the
    /// header declares.
    force_avm1: bool,

    /// Whether this movie was loaded using `Loader.loadBytes`.
    ///
    /// If this is `true`, the `url` field is likely meaningless, as the movie
    /// was loaded from bytes, not a URL. If this is `true`, an HTTP status
    /// event will not be fired on the `loaderInfo` of this movie.
    is_from_bytes: bool,

    /// Security sandbox type enforced for this movie.
    ///
    /// It absolutely cannot be changed after constructing
    /// the object in order to ensure proper sandboxing.
    sandbox_type: SandboxType,
}

impl SwfMovieData {
    /// Construct an empty movie.
    pub fn empty(swf_version: u8, loader_url: Option<String>) -> Self {
        let url = "file:///".to_string();
        let header = HeaderExt::default_with_swf_version(swf_version);

        // TODO What sandbox type should we use here?
        let sandbox_type = SandboxType::infer(url.as_str(), &header);
        Self {
            header,
            store: SwfStore::empty(),
            loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_used: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            last_active: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            url,
            loader_url,
            parameters: Vec::new(),
            encoding: swf::UTF_8,
            compressed_len: 0,
            is_movie: false,
            force_avm1: false,
            is_from_bytes: false,
            sandbox_type,
        }
    }

    /// Construct an empty movie with a fake `compressed_len`.
    /// This is used by `Loader` when firing an initial `progress` event:
    /// `LoaderInfo.bytesTotal` is set to the actual value, but no data is available,
    /// and `LoaderInfo.parameters` is empty.
    pub fn fake_with_compressed_len(
        swf_version: u8,
        loader_url: Option<String>,
        compressed_len: usize,
    ) -> Self {
        let url = "file:///".to_string();
        let header = HeaderExt::default_with_swf_version(swf_version);

        // TODO What sandbox type should we use here?
        let sandbox_type = SandboxType::infer(url.as_str(), &header);
        Self {
            header,
            compressed_len,
            store: SwfStore::empty(),
            loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_used: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            last_active: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            url,
            loader_url,
            parameters: Vec::new(),
            encoding: swf::UTF_8,
            is_movie: false,
            force_avm1: false,
            is_from_bytes: false,
            sandbox_type,
        }
    }

    /// Like `fake_with_compressed_len`, but uses actual data.
    /// This is used when loading a Bitmap to expose the underlying content
    pub fn fake_with_compressed_data(
        swf_version: u8,
        loader_url: Option<String>,
        compressed_data: Vec<u8>,
    ) -> Self {
        let url = "file:///".to_string();
        let header = HeaderExt::default_with_swf_version(swf_version);

        // TODO What sandbox type should we use here?
        let sandbox_type = SandboxType::infer(url.as_str(), &header);
        Self {
            header,
            compressed_len: compressed_data.len(),
            // 图片伪电影：尽力解析，失败就用空存储（它只为 `loaderInfo.bytes` 服务）。
            store: SwfStore::new(Arc::new(compressed_data)).map_or_else(
                |_| SwfStore::empty(),
                |(store, _)| store,
            ),
            loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_used: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            last_active: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            url,
            loader_url,
            parameters: Vec::new(),
            encoding: swf::UTF_8,
            is_movie: false,
            force_avm1: false,
            is_from_bytes: false,
            sandbox_type,
        }
    }

    /// Constructs the error state movie stub in which some attributes have certain
    /// error values to signal that no valid file could be loaded.
    ///
    /// This happens if no file could be loaded or if the loaded content is no valid
    /// supported content.
    pub fn error_movie(movie_url: String) -> Self {
        let header = HeaderExt::default_error_header();

        // TODO What sandbox type should we use here?
        let sandbox_type = SandboxType::infer(movie_url.as_str(), &header);
        Self {
            header,
            store: SwfStore::empty(),
            loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_used: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            last_active: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            url: movie_url,
            loader_url: None,
            parameters: Vec::new(),
            encoding: swf::UTF_8,
            compressed_len: 0,
            is_movie: false,
            force_avm1: false,
            is_from_bytes: false,
            sandbox_type,
        }
    }

    /// Construct a movie based on the contents of the SWF datastream. If the
    /// SWF was loaded specifically from the AVM2 method `Loader.loadBytes`,
    /// this method accepts extra information as a `LoadBytesInfo`.
    pub fn from_data(
        swf_data: &[u8],
        url: String,
        loader_url: Option<String>,
        load_bytes_info: Option<LoadBytesInfo>,
    ) -> Result<Self, swf::error::Error> {
        let compressed_len = swf_data.len();
        // SXDffle 0.261145：不再整块解压。
        //
        // 原来这里 `decompress_swf` 把整部 SWF 解压成一个 `Vec` 并永久常驻，
        // 战报 500 多部电影就是 700MB 以上；而且 `SwfSlice` 持有同一个 `Arc`，
        // 于是「丢弃字节」实际不生效（实测 558 部 / 714MB 三分钟不动）。
        // 改为窗口存储：只解压到够解析文件头，其余按 256KB 窗口在真正被读到时展开。
        let _sxd_tag = crate::alloc_tag::Guard::new(1);
        let raw = Arc::new(swf_data.to_vec());
        let (store, header) = SwfStore::new(raw)?;
        let encoding = swf::SwfStr::encoding_for_version(header.version());

        // The loader SWF has full control over the tags of a SWF loaded using
        // `Loader.loadBytes`, so if we were to use the sandbox type declared in
        // that SWF's header, the SWF could break sandboxing. Instead, always
        // use the sandbox type of the loader SWF, to ensure that it can't load
        // a child SWF with different sandboxing.
        let sandbox_type = load_bytes_info
            .map(|i| i.loader_sandbox_type)
            .unwrap_or_else(|| SandboxType::infer(url.as_str(), &header));

        let mut movie = Self {
            header,
            store,
            loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_used: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            last_active: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            url,
            loader_url,
            parameters: Vec::new(),
            encoding,
            compressed_len,
            is_movie: true,
            force_avm1: false,
            is_from_bytes: load_bytes_info.is_some(),
            sandbox_type,
        };
        movie.append_parameters_from_url();
        Ok(movie)
    }

    /// Construct a movie based on a loaded image (JPEG, GIF or PNG).
    pub fn from_loaded_image(
        url: String,
        is_from_bytes: bool,
        length: usize,
        width: u32,
        height: u32,
    ) -> Self {
        let stage_size = Rectangle::ZERO
            .with_width(Twips::from_pixels_i32(width as i32))
            .with_height(Twips::from_pixels_i32(height as i32));
        let header = HeaderExt::default_with_uncompressed_len(length as i32, stage_size);
        let sandbox_type = SandboxType::infer(url.as_str(), &header);
        let mut movie = Self {
            header,
            store: SwfStore::empty(),
            loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_used: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            last_active: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            url,
            loader_url: None,
            parameters: Vec::new(),
            encoding: swf::UTF_8,
            compressed_len: length,
            is_movie: false,
            force_avm1: false,
            is_from_bytes,
            sandbox_type,
        };
        movie.append_parameters_from_url();
        movie
    }

    fn append_parameters_from_url(&mut self) {
        match Url::parse(&self.url) {
            Ok(url) => {
                for (key, value) in url.query_pairs() {
                    self.parameters.push((key.into_owned(), value.into_owned()));
                }
            }
            Err(e) => {
                tracing::error!(
                    "Failed to parse loader URL when extracting query parameters: {}",
                    e
                );
            }
        }
    }

    pub fn header(&self) -> &HeaderExt {
        &self.header
    }

    /// Get the version of the SWF.
    pub fn version(&self) -> u8 {
        self.header.version()
    }

    /// 解压后的 tag stream（全量）。
    ///
    /// SXDffle 0.261145：**会触发整部解压**，只留给确实需要整份字节的路径
    /// （保存 SWF、视频帧）。只关心长度的调用方请用 [`Self::data_len`]，
    /// 只关心某一段的请用 [`Self::slice`]。
    pub fn data(&self) -> &[u8] {
        self.store.slice(0, self.store.len())
    }

    /// tag stream 的长度。不触发解压。
    #[inline]
    pub fn data_len(&self) -> usize {
        self.store.len()
    }

    /// 区间视图。只解压到 `end` 为止，不会整部展开。
    #[inline]
    pub fn slice(&self, start: usize, end: usize) -> &[u8] {
        self.store.slice(start, end)
    }

    /// 整片 preload 是否已经完成。
    #[inline]
    pub fn is_loaded(&self) -> bool {
        self.loaded.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 最近一次时间轴推进的「进程内秒数」。
    #[inline]
    pub fn last_active_secs(&self) -> u32 {
        self.last_active.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 最近一次真正读字节的「进程内秒数」。
    #[inline]
    pub fn last_store_use_secs(&self) -> u32 {
        self.store.last_use_secs()
    }

    /// 立刻逐出解压窗口（不看 idle）。
    ///
    /// SXDffle：这是窗口存储才有的能力 —— 字节丢了能重放回来，
    /// 所以调用方只需要判断「现在还要不要读 tag」，不必担心丢过头。
    #[inline]
    pub fn evict_data(&self) -> usize {
        self.store.evict()
    }

    /// 标记整片 preload 完成。此后字节才可以被回收。
    pub fn mark_loaded(&self) {
        self.loaded
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 刷新最近访问时间（由库在每次查表时调用）。
    pub fn touch(&self, now_secs: u32) {
        self.last_used
            .store(now_secs, std::sync::atomic::Ordering::Relaxed);
    }

    /// 当前进程内秒数（无系统调用，vDSO/Instant）。
    pub fn now_secs() -> u32 {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_secs() as u32
    }

    /// 标记「时间轴正在推进」——由 MovieClip 在 run_frame / goto / construct 时调用。
    pub fn touch_active(&self) {
        self.last_active
            .store(Self::now_secs(), std::sync::atomic::Ordering::Relaxed);
    }

    /// 丢弃解压数据（保留头部窗口），返回释放的字节数。
    ///
    /// SXDffle 0.261145：现在是真的释放。
    ///
    /// 旧实现把 `Arc<Vec<u8>>` 从 `Mutex<Option<..>>` 里 `take()` 出来，
    /// 但每个 `MovieClipShared` 的 `SwfSlice` 都持有同一个 `Arc` ——
    /// 引用计数永远 ≥ 2，字节一个都掉不了（实测 558 部 / 714MB 三分钟不动）。
    ///
    /// 窗口存储下 `SwfSlice` 持有的是 `Arc<SwfStore>`（小对象），
    /// 逐出的是 `SwfStore` 内部的映射区，所以这里 `madvise(MADV_DONTNEED)`
    /// 之后物理页立刻还给系统；下次真要读，从压缩字节重放即可。
    pub fn try_release_data(&self, now_secs: u32, idle_secs: u32) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.loaded.load(Relaxed) {
            return 0;
        }
        if now_secs.saturating_sub(self.last_used.load(Relaxed)) < idle_secs {
            return 0;
        }
        // 时间轴还在推进就先留着：下一帧要把 tag 读回来，逐出会引起重放。
        if now_secs.saturating_sub(self.last_active.load(Relaxed)) < idle_secs {
            return 0;
        }
        self.store.evict()
    }

    /// 是否已丢弃解压数据（只剩头部窗口）。
    pub fn is_data_released(&self) -> bool {
        self.store.resident_len() < self.store.len()
    }

    /// 当前驻留的字节数（物理内存里真正占着的部分）。
    pub fn resident_len(&self) -> usize {
        self.store.resident_len()
    }

    /// Returns the suggested string encoding for the given SWF version.
    /// For SWF version 6 and higher, this is always UTF-8.
    /// For SWF version 5 and lower, this is locale-dependent,
    /// and we default to WINDOWS-1252.
    pub fn encoding(&self) -> &'static swf::Encoding {
        self.encoding
    }

    /// The width of the movie in twips.
    pub fn width(&self) -> Twips {
        self.header.stage_size().width()
    }

    /// The height of the movie in twips.
    pub fn height(&self) -> Twips {
        self.header.stage_size().height()
    }

    /// Get the URL this SWF was fetched from.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn set_url(&mut self, url: String) {
        self.url = url;
    }

    pub fn set_force_avm1(&mut self) {
        self.force_avm1 = true;
    }

    pub fn is_from_bytes(&self) -> bool {
        self.is_from_bytes
    }

    /// Get the URL that triggered the fetch of this SWF.
    pub fn loader_url(&self) -> Option<&str> {
        self.loader_url.as_deref()
    }

    pub fn parameters(&self) -> &[(String, String)] {
        &self.parameters
    }

    pub fn append_parameters(&mut self, params: impl IntoIterator<Item = (String, String)>) {
        self.parameters.extend(params);
    }

    pub fn compressed_len(&self) -> usize {
        self.compressed_len
    }

    pub fn uncompressed_len(&self) -> i32 {
        self.header.uncompressed_len()
    }

    /// Whether the SWF's FileAttributes tag declares the SWF to be AVM2.
    pub fn is_declared_action_script_3(&self) -> bool {
        self.header.is_action_script_3()
    }

    /// Whether this `SwfMovieData` should be interpreted as AVM2.
    ///
    /// This usually is the same as `is_declared_action_script_3`, but will
    /// return false if this is an AVM2 movie loaded by AVM1 (which we mark by
    /// setting `force_avm1` to true).
    pub fn is_action_script_3(&self) -> bool {
        self.header.is_action_script_3() && !self.force_avm1
    }

    pub fn stage_size(&self) -> &Rectangle<Twips> {
        self.header.stage_size()
    }

    pub fn num_frames(&self) -> u16 {
        self.header.num_frames()
    }

    pub fn frame_rate(&self) -> Fixed8 {
        self.header.frame_rate()
    }

    pub fn is_movie(&self) -> bool {
        self.is_movie
    }

    pub fn sandbox_type(&self) -> SandboxType {
        self.sandbox_type
    }
}

impl Debug for SwfMovieData {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwfMovieData")
            .field("header", &self.header)
            .field("data", &self.data_len())
            .field("resident", &self.resident_len())
            .field("url", &self.url)
            .field("loader_url", &self.loader_url)
            .field("parameters", &self.parameters)
            .field("encoding", &self.encoding)
            .field("compressed_len", &self.compressed_len)
            .field("is_movie", &self.is_movie)
            .field("sandbox_type", &self.sandbox_type)
            .finish()
    }
}

/// A shared-ownership reference to some portion of an SWF datastream.
#[derive(Debug, Clone, Collect)]
#[collect(no_drop)]
pub struct SwfSlice {
    /// SXDffle 0.261145：不再持有整块解压字节，只持有窗口存储。
    ///
    /// 原来这里是 `Arc<Vec<u8>>`，每个 MovieClip 一份引用，导致
    /// `SwfMovieData::try_release_data` 永远拿不到独占权、字节放不掉。
    store: Arc<SwfStore>,
    swf_version: u8,

    pub start: usize,
    pub end: usize,
}

impl<'gc> From<SwfMovie<'gc>> for SwfSlice {
    fn from(movie: SwfMovie<'gc>) -> Self {
        let end = movie.store.len();
        Self {
            store: movie.store.clone(),
            swf_version: movie.version(),
            start: 0,
            end,
        }
    }
}

impl AsRef<[u8]> for SwfSlice {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.data()
    }
}

impl SwfSlice {
    /// Creates an empty SwfSlice.
    #[inline]
    pub fn empty(movie: SwfMovie<'_>) -> Self {
        Self {
            store: movie.store.clone(),
            swf_version: movie.version(),
            start: 0,
            end: 0,
        }
    }

    /// Creates an empty SwfSlice of the same movie.
    #[inline]
    pub fn copy_empty(&self) -> Self {
        Self {
            store: self.store.clone(),
            swf_version: self.swf_version,
            start: 0,
            end: 0,
        }
    }

    /// Construct a new SwfSlice from a regular slice.
    ///
    /// This function panics if the given slice is not a subslice of the current
    /// slice.
    pub fn to_subslice(&self, slice: &[u8]) -> Self {
        // 映射区地址固定，所以「子切片相对基址的偏移」仍然是可靠的换算依据。
        let self_pval = self.store.as_ptr() as usize;
        let slice_pval = slice.as_ptr() as usize;

        if (self_pval + self.start) <= slice_pval && slice_pval < (self_pval + self.end) {
            Self {
                store: self.store.clone(),
                swf_version: self.swf_version,
                start: slice_pval - self_pval,
                end: (slice_pval - self_pval) + slice.len(),
            }
        } else {
            self.copy_empty()
        }
    }

    /// Construct a new SwfSlice from a movie subslice.
    ///
    /// This function allows subslices outside the current slice to be formed,
    /// as long as they are valid subslices of the movie itself.
    pub fn to_unbounded_subslice(&self, slice: &[u8]) -> Self {
        let self_pval = self.store.as_ptr() as usize;
        let self_len = self.store.len();
        let slice_pval = slice.as_ptr() as usize;

        if self_pval <= slice_pval && slice_pval < (self_pval + self_len) {
            Self {
                store: self.store.clone(),
                swf_version: self.swf_version,
                start: slice_pval - self_pval,
                end: (slice_pval - self_pval) + slice.len(),
            }
        } else {
            self.copy_empty()
        }
    }

    /// Construct a new SwfSlice from a SwfStream.
    ///
    /// This is intended to allow constructing references to the contents of a
    /// given SWF tag.
    ///
    /// If the reader references a slice outside the bounds of the current
    /// slice, or the given reader refers to a different underlying movie, this
    /// function returns an empty slice.
    pub fn resize_to_reader(&self, reader: &SwfStream<'_>) -> Self {
        self.to_subslice(reader.get_ref())
    }

    /// Convert the SwfSlice into a standard data slice.
    ///
    /// SXDffle 0.261145：按需解压到 `end`，不会整部展开。
    pub fn data(&self) -> &[u8] {
        self.store.slice(self.start, self.end)
    }

    /// Access the entire data of the movie backing this SwfSlice.
    ///
    /// 注意：这会解压整部电影。只有 AVM1 的 `seek` 这类确实需要全量基址的
    /// 路径才该用它。
    pub fn entire_data(&self) -> &[u8] {
        self.store.slice(0, self.store.len())
    }

    /// Checks if this slice is empty
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }

    /// Construct a reader for this slice.
    ///
    /// The `from` parameter is the offset to start reading the slice from.
    pub fn read_from(&self, from: u64) -> swf::read::Reader<'_> {
        let data = self.data();
        let tail = data.get(from as usize..).unwrap_or(&[]);
        swf::read::Reader::new(tail, self.swf_version)
    }

    /// Get the length of the SwfSlice.
    pub fn len(&self) -> usize {
        self.end - self.start
    }
}

/// Extra information provided when a SWF is loaded using `Loader.loadBytes`
/// (as opposed to when it is loaded from a URL).
#[derive(Clone, Copy)]
pub struct LoadBytesInfo {
    pub loader_sandbox_type: SandboxType,
}
