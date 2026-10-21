//! SXDffle —— Android 设备字体后端
//! =====================================================================
//! 为什么需要这个模块？
//!
//! 中文页游（如《神仙道》）为了压缩 SWF 体积，文本大量使用 **设备字体**
//! （device font），即只写字体名（"Arial" 等），实际字形由运行环境提供。
//!
//! Ruffle 的字体解析分两路：
//!   1. SWF 内嵌字体（DefineFont*）—— 字形在 SWF 里，直接渲染，不依赖系统；
//!   2. 设备字体 —— 由 `UiBackend::load_device_font` 从系统查找并注册。
//!
//! 上游 `ruffle-android` **没有实现 `UiBackend`**，因此走的是
//! `NullUiBackend` 的空实现（`load_device_font` 什么都不做），
//! 结果日志里出现：
//!
//!     WARN ruffle_core::library: Unknown device font "Arial"
//!     WARN ruffle_core::library: Unknown device font "Arimo"
//!
//! 字体查找失败后回退到 Ruffle 的 `default_font`（内置拉丁字体，不含 CJK 字形），
//! 于是中文就渲染成方块/空白 —— 这正是真机上看到"有些文字不显示或显示错误"的原因。
//!
//! 本模块用 `fontdb`（纯 Rust 字体库，桌面版同款）加载 Android 系统字体目录，
//! 让设备字体查询能命中真实字体（含 Noto Sans CJK 等中文字体）。
//! 并在找不到请求字体时主动回退到系统中文字体，保证中文可读。
//! =====================================================================

use ruffle_core::backend::ui::{
    DialogResultFuture, FileFilter, FontDefinition, FullscreenError, LanguageIdentifier,
    MouseCursor, MultiDialogResultFuture, NullUiBackend, UiBackend,
};
use ruffle_core::font::{FontFileData, FontQuery};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use url::Url;

/// Android 系统字体所在目录（不同 ROM 可能有额外目录）
const SYSTEM_FONT_DIRS: &[&str] = &[
    "/system/fonts",
    "/product/fonts",
    "/vendor/fonts",
    "/system/font",
];

/// 中文/日文回退候选（按优先级）。
///
/// 这些是 Android 上常见的 CJK 字体家族名；游戏请求某个不存在的字体名时，
/// 会依次尝试它们，避免中文落到不含 CJK 的拉丁字体上。
const CJK_FALLBACKS: &[&str] = &[
    "Noto Sans CJK SC",
    "Source Han Sans SC",
    "Source Han Sans CN",
    "Noto Sans SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK TC",
    "Droid Sans Fallback",
    "Noto Sans",
    "Roboto",
];

pub struct AndroidUiBackend {
    /// 复用 NullUiBackend 的其余行为，只覆盖字体相关方法
    inner: NullUiBackend,
    fonts: Rc<fontdb::Database>,
}

impl AndroidUiBackend {
    pub fn new() -> Self {
        let mut fonts = fontdb::Database::new();
        let mut loaded_dirs = 0usize;

        for dir in SYSTEM_FONT_DIRS {
            if std::path::Path::new(dir).is_dir() {
                fonts.load_fonts_dir(dir);
                loaded_dirs += 1;
            }
        }

        log::info!(
            "SXDffle 字体库就绪：扫描 {loaded_dirs} 个目录，载入 {} 个字体面",
            fonts.len()
        );

        Self {
            inner: NullUiBackend::new(),
            fonts: Rc::new(fonts),
        }
    }

    /// 在字体库中按「家族名（不区分大小写）」查找
    fn find_family(&self, lowercase_name: &str) -> Option<String> {
        self.fonts.faces().find_map(|face| {
            face.families
                .iter()
                .find(|(family, _)| family.to_lowercase() == lowercase_name)
                .map(|(family, _)| family.clone())
        })
    }

    /// 回退到系统中文字体，保证中文能显示
    fn query_cjk_fallback(&self, weight: fontdb::Weight, style: fontdb::Style) -> Option<fontdb::ID> {
        for family in CJK_FALLBACKS {
            let query = fontdb::Query {
                families: &[fontdb::Family::Name(family)],
                weight,
                style,
                ..Default::default()
            };
            if let Some(id) = self.fonts.query(&query) {
                return Some(id);
            }
        }
        None
    }

    /// 取出字体字节数据（进程级共享）。
    ///
    /// 中文字体是 19MB 级别的文件，而一个页游会依次请求 Arial、宋体、黑体、
    /// Courier New 等好几个家族 —— 如果每次都 `fs::read` 一份再交给引擎，
    /// 单个会话就要装 100MB 以上；平台一切换（新会话）内存直接翻倍。
    ///
    /// `FontFileData` 内部本来就是 `Arc<dyn AsRef<[u8]>>`，所以这里按路径缓存
    /// 一份 `Arc<Vec<u8>>` 共享给所有会话，整个进程只占一份。
    fn face_bytes(face: &fontdb::FaceInfo) -> Option<Arc<Vec<u8>>> {
        fn cache() -> &'static Mutex<HashMap<String, Arc<Vec<u8>>>> {
            static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
            CACHE.get_or_init(|| Mutex::new(HashMap::new()))
        }

        // key 用路径（Binary 字体没有路径，用名字兜底）
        let (key, inline) = match &face.source {
            fontdb::Source::File(path) => (path.to_string_lossy().into_owned(), None),
            fontdb::Source::Binary(data) => (
                format!("mem:{}", face.post_script_name),
                Some(data.as_ref().as_ref().to_vec()),
            ),
            fontdb::Source::SharedFile(path, data) => (
                path.to_string_lossy().into_owned(),
                Some(data.as_ref().as_ref().to_vec()),
            ),
        };

        let mut guard = cache().lock().ok()?;
        if let Some(hit) = guard.get(&key) {
            return Some(hit.clone());
        }
        let bytes = match inline {
            Some(b) => b,
            None => std::fs::read(&key).ok()?,
        };
        let shared = Arc::new(bytes);
        guard.insert(key, shared.clone());
        Some(shared)
    }
}

impl UiBackend for AndroidUiBackend {
    fn mouse_visible(&self) -> bool {
        self.inner.mouse_visible()
    }

    fn set_mouse_visible(&mut self, visible: bool) {
        self.inner.set_mouse_visible(visible)
    }

    fn set_mouse_cursor(&mut self, cursor: MouseCursor) {
        self.inner.set_mouse_cursor(cursor)
    }

    fn clipboard_content(&mut self) -> String {
        self.inner.clipboard_content()
    }

    fn set_clipboard_content(&mut self, content: String) {
        self.inner.set_clipboard_content(content)
    }

    fn set_fullscreen(&mut self, is_full: bool) -> Result<(), FullscreenError> {
        self.inner.set_fullscreen(is_full)
    }

    fn display_root_movie_download_failed_message(&self, invalid_swf: bool, fetch_error: String) {
        self.inner
            .display_root_movie_download_failed_message(invalid_swf, fetch_error)
    }

    fn message(&self, message: &str) {
        self.inner.message(message)
    }

    fn display_unsupported_video(&self, url: Url) {
        self.inner.display_unsupported_video(url)
    }

    fn open_virtual_keyboard(&self) {
        self.inner.open_virtual_keyboard()
    }

    fn close_virtual_keyboard(&self) {
        self.inner.close_virtual_keyboard()
    }

    fn language(&self) -> LanguageIdentifier {
        self.inner.language()
    }

    fn display_file_open_dialog(&mut self, filters: Vec<FileFilter>) -> Option<DialogResultFuture> {
        self.inner.display_file_open_dialog(filters)
    }

    fn display_file_open_dialog_multiple(
        &mut self,
        filters: Vec<FileFilter>,
    ) -> Option<MultiDialogResultFuture> {
        self.inner.display_file_open_dialog_multiple(filters)
    }

    fn close_file_dialog(&mut self) {
        self.inner.close_file_dialog()
    }

    fn display_file_save_dialog(
        &mut self,
        file_name: String,
        domain: String,
    ) -> Option<DialogResultFuture> {
        self.inner.display_file_save_dialog(file_name, domain)
    }

    // ================= 核心：设备字体加载 =================
    fn load_device_font(&self, query: &FontQuery, register: &mut dyn FnMut(FontDefinition)) {
        let weight = if query.is_bold {
            fontdb::Weight::BOLD
        } else {
            fontdb::Weight::NORMAL
        };
        let style = if query.is_italic {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        };

        // 1) 先按请求的字体名查找（fontdb 的 query 区分大小写，
        //    因此先不区分大小写地解析出规范家族名，再查询）
        let id = self
            .find_family(query.lowercase_name.as_str())
            .and_then(|family| {
                self.fonts.query(&fontdb::Query {
                    families: &[fontdb::Family::Name(&family)],
                    weight,
                    style,
                    ..Default::default()
                })
            })
            // 2) 找不到就回退到系统中文字体 —— 否则中文会渲染成方块
            .or_else(|| self.query_cjk_fallback(weight, style));

        let Some(id) = id else {
            log::warn!(
                "SXDffle: 未找到设备字体 \"{}\"（粗体={}, 斜体={}）",
                query.name,
                query.is_bold,
                query.is_italic
            );
            return;
        };

        let Some(face) = self.fonts.face(id) else {
            return;
        };

        let Some(bytes) = Self::face_bytes(face) else {
            log::warn!("SXDffle: 无法读取字体数据 {}", face.post_script_name);
            return;
        };

        log::info!(
            "SXDffle 设备字体 \"{}\" → \"{}\"（{} 字节, 粗体={}, 斜体={}）",
            query.name,
            face.post_script_name,
            bytes.len(),
            query.is_bold,
            query.is_italic
        );

        register(FontDefinition::FontFile {
            name: query.name.clone(),
            is_bold: query.is_bold,
            is_italic: query.is_italic,
            data: FontFileData::new_shared({
                let shared: Arc<dyn AsRef<[u8]>> = bytes.clone();
                shared
            }),
            index: face.index,
        });
    }

    /// SXDffle：不做字形排序。
    ///
    /// 上游桌面版在 Unix 上借助 fontconfig 做"候选字体排序"（用于把
    /// 缺失字形的字符分配到其它字体上）。Android 无 fontconfig，
    /// 返回空列表即可；中文覆盖由上面的 CJK 回退保证。
    fn sort_device_fonts(
        &self,
        _query: &FontQuery,
        _register: &mut dyn FnMut(FontDefinition),
    ) -> Vec<FontQuery> {
        Vec::new()
    }
}

impl Default for AndroidUiBackend {
    fn default() -> Self {
        Self::new()
    }
}
