//! SXDffle 日志模块
//! =====================================================================
//! 真机排查时用 `adb logcat` 抓日志最完整，但很多时候不方便接电脑。
//! 因此这里把引擎日志做成 **双写**：
//!
//!   - 一份进 **logcat**          → 供 `adb logcat` 抓取
//!   - 一份追加到 **sxdffle.log**  → 应用内可一键导出/分享
//!
//! 实现方式：`android_logger::AndroidLogger` 本身是一个 `log::Log`，
//! 这里包一层 `TeeLogger` 做分流，因此**所有** `log::info!` 调用点
//! 无需改动即可同时落到两处。
//! =====================================================================

use android_logger::{AndroidLogger, Config};
use ruffle_core::backend::log::LogBackend;
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{LineWriter, Write};
use std::path::Path;
use std::sync::Mutex;

// ======================= 应用内日志文件 =======================

/// 应用内日志文件的写句柄（进程内全局）
static APP_LOG: Mutex<Option<LineWriter<File>>> = Mutex::new(None);

/// 初始化应用内日志文件。
///
/// 路径由 Kotlin 侧提供（`getExternalFilesDir(null)/sxdffle.log`），
/// 放在应用外部目录便于用文件管理器直接取出，也便于通过 FileProvider 分享。
pub fn init_app_log(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    if let Ok(mut guard) = APP_LOG.lock() {
        *guard = Some(LineWriter::new(file));
    }
    Ok(())
}

/// 追加一行到应用内日志
pub fn append_app_log(line: &str) {
    if let Ok(mut guard) = APP_LOG.lock() {
        if let Some(writer) = guard.as_mut() {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.write_all(b"\n");
            // 及时落盘，便于游戏卡死时仍能拿到日志
            let _ = writer.flush();
        }
    }
}

fn flush_app_log() {
    if let Ok(mut guard) = APP_LOG.lock() {
        if let Some(writer) = guard.as_mut() {
            let _ = writer.flush();
        }
    }
}


/// 当前**本地**墙钟 `HH:MM:SS.mmm`。
///
/// 之前用 `SystemTime % 86400` 算的是 UTC，在 +8 时区上比 `[UI]` 行慢 8 小时，
/// 日志根本对不齐。改用 bionic 的 `localtime_r`（不引入新依赖）：
/// `struct tm` 前三个 int 就是 sec/min/hour，用一个 64 字节缓冲接住即可。
fn wall_clock_hms() -> String {
    extern "C" {
        fn localtime_r(t: *const i64, tm: *mut i32) -> *mut i32;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let secs = now_ms / 1000;
    let mut tm = [0i32; 16];
    let ok = unsafe { !localtime_r(&secs, tm.as_mut_ptr()).is_null() };
    if ok {
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            tm[2],
            tm[1],
            tm[0],
            now_ms % 1000
        )
    } else {
        format!("{:02}:{:02}:{:02}.{:03}", 0, 0, 0, now_ms % 1000)
    }
}

// ======================= 双写 Logger =======================

/// 把 `log` 的输出同时送到 logcat 与应用内文件
pub struct TeeLogger {
    android: AndroidLogger,
}

impl TeeLogger {
    pub fn new(config: Config) -> Self {
        Self {
            android: AndroidLogger::new(config),
        }
    }

    /// 安装为全局 logger。
    ///
    /// 注意：`log::max_level()` 在安装 logger 之前默认是 `Off`，
    /// 若用它回填会导致所有日志被丢弃，因此这里显式设为 `Info`。
    pub fn install(config: Config) {
        if log::set_boxed_logger(Box::new(Self::new(config))).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    }
}

impl log::Log for TeeLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.android.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.android.enabled(record.metadata()) {
            return;
        }

        // 1) 进 logcat
        self.android.log(record);

        // 2) 进应用内文件（带时间戳）
        //
        // 之前这里没有时间，导致「切后台」这类事件无法与 Kotlin 侧的
        // `[UI]` 行（带毫秒时间）对齐 —— 排查 LMK 击杀时只能靠猜顺序。
        // 时间戳取自 /proc/uptime 的单调值再换成墙钟，代价是一次除法，可忽略。
        append_app_log(&format!(
            "{} {:<5} [{}] {}",
            wall_clock_hms(),
            record.level(),
            record.target(),
            record.args()
        ));
    }

    fn flush(&self) {
        self.android.flush();
        flush_app_log();
    }
}

// ======================= AS3 trace 后端 =======================

pub struct FileLogBackend {
    writer: Option<RefCell<LineWriter<File>>>,
}

impl FileLogBackend {
    pub fn new(path: Option<&Path>) -> Self {
        Self {
            writer: path
                .map(|path| File::create(path).unwrap())
                .map(LineWriter::new)
                .map(RefCell::new),
        }
    }
}

impl LogBackend for FileLogBackend {
    fn avm_trace(&self, message: &str) {
        log::info!("avm_trace: {message}");
        if let Some(writer) = &self.writer {
            writer.borrow_mut().write_all(message.as_bytes()).unwrap();
            writer.borrow_mut().write_all("\n".as_bytes()).unwrap();
        }
    }

    fn avm_warning(&self, message: &str) {
        log::info!("avm_warning: {message}");
        if let Some(writer) = &self.writer {
            writer.borrow_mut().write_all(message.as_bytes()).unwrap();
            writer.borrow_mut().write_all("\n".as_bytes()).unwrap();
        }
    }
}
