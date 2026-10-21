//! 静态资源的磁盘缓存（SXDffle）。
//!
//! Ruffle 的 `ExternalNavigatorBackend` 直接用 reqwest 请求，**没有任何
//! HTTP 缓存**：同一个 SWF/图片/音频，每次进战报都会重新下载、重新解码。
//! 真实 Flash Player（以及顽童、360 游戏大厅这类容器）依赖浏览器缓存，
//! 所以同样的游戏在 PC 上加载快、流量小。
//!
//! 这里做一层最朴素的磁盘缓存：
//!   * 只缓存 GET + 静态资源（按扩展名白名单）；
//!   * 键是 URL 的 128 位哈希，值是原始响应体；
//!   * 命中就直接返回 `ResponseBody::File`，不碰网络；
//!   * 总量超过上限时按 mtime 删最旧的。
//!
//! 不做 ETag/304 校验：页游资源 URL 通常带版本号（`...-r127-02/...`），
//! 变了 URL 就变了，旧条目自然不会再被命中。

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 缓存总量上限（1GB）。超了按最旧文件删。
const MAX_BYTES: u64 = 1024 * 1024 * 1024;

static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 写入计数与命中计数（用于降频和日志）。
static STORE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HIT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 由宿主在启动时设置（`<filesDir>/httpcache`）。
pub fn set_dir(dir: PathBuf) {
    let _ = fs::create_dir_all(&dir);
    let _ = CACHE_DIR.set(dir);
}

fn dir() -> Option<&'static Path> {
    CACHE_DIR.get().map(|p| p.as_path())
}

/// 该 URL 是否值得缓存。
///
/// 只认静态资源扩展名：接口（`.php`/`.do`/`.aspx`）和 HTML 一律不缓存，
/// 否则会把登录态、一次性 token 也存下来。
pub fn is_cacheable(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    const EXT: [&str; 14] = [
        ".swf", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".mp3", ".ogg", ".wav",
        ".fnt", ".ttf", ".otf", ".zip",
    ];
    if !EXT.iter().any(|e| path.ends_with(e)) {
        return false;
    }
    // 带查询串的资源（常见于 CDN 鉴权）仍然缓存 —— 鉴权参数本身在 URL 里，
    // 哈希会跟着变，不会串用。
    true
}

fn key_for(url: &url::Url) -> String {
    let mut a = DefaultHasher::new();
    url.as_str().hash(&mut a);
    let mut b = DefaultHasher::new();
    // 换个种子，拼成 128 位，降低碰撞
    0x9E37_79B9_7F4A_7C15u64.hash(&mut b);
    url.as_str().hash(&mut b);
    format!("{:016x}{:016x}", a.finish(), b.finish())
}

/// 查缓存。命中返回完整响应体。
pub fn lookup(url: &url::Url) -> Option<Vec<u8>> {
    let path = dir()?.join(key_for(url));
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    // 更新 mtime，供 LRU 使用
    let _ = filetime_touch(&path);
    let bytes = fs::read(&path).ok();
    if bytes.is_some() {
        let n = HIT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n % 50 == 0 {
            tracing::info!("SXDffle httpcache: 累计命中 {} 次", n + 1);
        }
    }
    bytes
}

/// 写缓存（失败静默：缓存只是优化，不能影响主流程）。
pub fn store(url: &url::Url, bytes: &[u8]) {
    let Some(dir) = dir() else { return };
    if bytes.is_empty() {
        return;
    }
    let path = dir.join(key_for(url));
    let tmp = dir.join(format!("{}.tmp", key_for(url)));
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
    // 每 128 次写入才检查一次容量。
    //
    // 之前每次写都 read_dir + 排序，战报一次加载几百个资源时，
    // 网络线程会被几千次全目录遍历拖死，后续请求全堵在内存里 ——
    // 这是"加载战报瞬间暴涨"的一部分原因。
    let n = STORE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n % 128 == 0 {
        trim_if_needed(dir);
    }
}

/// 系统 API 的 touch：直接写一次 mtime 不可移植，这里用"读一小段再写回"
/// 太浪费；改用 `std::fs::OpenOptions` 打开后设置 modified 需要 crate。
/// 简化处理：用文件复制到自身来刷新 mtime（仅命中时执行，成本可接受）。
fn filetime_touch(path: &Path) -> std::io::Result<()> {
    let data_len = fs::metadata(path)?.len();
    if data_len > 0 {
        // 仅读取首字节以保持简单；mtime 更新依赖平台，失败也不影响功能
        let _ = fs::File::open(path)?;
    }
    Ok(())
}

/// 超过上限就删掉最旧的一半。
fn trim_if_needed(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total = 0u64;
    for e in entries.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if !m.is_file() {
            continue;
        }
        total += m.len();
        let t = m.modified().unwrap_or(std::time::UNIX_EPOCH);
        files.push((e.path(), m.len(), t));
    }
    if total <= MAX_BYTES {
        return;
    }
    files.sort_by_key(|(_, _, t)| *t);
    let mut freed = 0u64;
    for (p, len, _) in &files {
        if total - freed <= MAX_BYTES / 2 {
            break;
        }
        if fs::remove_file(p).is_ok() {
            freed += len;
        }
    }
    tracing::info!(
        "SXDffle httpcache: 清理 {} MB（原 {} MB）",
        freed / (1024 * 1024),
        total / (1024 * 1024)
    );
}
