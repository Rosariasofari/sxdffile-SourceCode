//! 致命信号捕获（native crash 兜底）
//!
//! # 为什么需要它
//!
//! `Thread.setDefaultUncaughtExceptionHandler` 只能抓 **Java 异常**。
//! 而 `libsxdffle.so` 里的 SIGSEGV（野指针、EGL/GPU 崩溃）会让进程直接消失，
//! Kotlin 侧连一行日志都来不及写 —— 用户只看到"闪退"，开发者什么都拿不到。
//!
//! 这里注册 `SIGSEGV/SIGABRT/SIGBUS/SIGILL/SIGFPE` 的 handler，
//! 在进程死掉之前把「死在哪一步」写进日志文件。
//!
//! # 为什么不用 `log::error!`
//!
//! signal handler 里只能调用 **async-signal-safe** 的函数。
//! `log` 会走 Mutex、堆分配、格式化，在崩溃现场调用可能二次崩溃或死锁。
//! 因此这里只做两件事：
//!   1. `write(2, ...)` 一个**预先准备好的常量字符串**（无分配、无锁）
//!   2. 恢复默认 handler 并重新 raise，让系统照常生成 tombstone
//!
//! 阶段编号用 `AtomicUsize`，handler 里按下标查表拿常量字节串 —— 全程不做格式化。

use std::os::unix::io::IntoRawFd;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

// bionic / Linux 上的固定值（无需 libc crate）
const SIGILL: i32 = 4;
const SIGABRT: i32 = 6;
const SIGBUS: i32 = 7;
const SIGFPE: i32 = 8;
const SIGSEGV: i32 = 11;
const SIG_DFL: usize = 0;

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn raise(sig: i32) -> i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn pthread_self() -> usize;
    fn pthread_getname_np(thread: usize, name: *mut u8, len: usize) -> i32;
    fn gettid() -> i32;
}

/// 十进制写出（signal handler 里不能用 format!，会分配/加锁）
fn write_i32(fd: i32, mut v: i32) {
    let mut buf = [0u8; 12];
    let mut i = buf.len();
    let neg = v < 0;
    if neg {
        v = -v;
    }
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    unsafe { write(fd, buf.as_ptr().add(i), buf.len() - i) };
}

/// 把崩溃线程的名字写进日志。
///
/// 之前只有"挂在第几阶段"，而阶段号是渲染线程设的、崩溃却可能发生在别的线程，
/// 于是每次都是 stage 8 这个**过期**标记 —— 把线程名记下来才能区分
/// "引擎析构崩的" 和 "UI/WebView/音频线程崩的"。
fn write_thread(fd: i32) {
    unsafe {
        let tid = gettid();
        write(fd, b"  tid: ".as_ptr(), 7);
        write_i32(fd, tid);
        write(fd, b"\n".as_ptr(), 1);

        let mut name = [0u8; 24];
        if pthread_getname_np(pthread_self(), name.as_mut_ptr(), name.len()) == 0 {
            let mut n = 0usize;
            while n < name.len() && name[n] != 0 {
                n += 1;
            }
            if n > 0 {
                write(fd, b"  thread: ".as_ptr(), 10);
                write(fd, name.as_ptr(), n);
                write(fd, b"\n".as_ptr(), 1);
            }
        }
    }
}

/// 预先打开的日志文件 fd。handler 里只能用原始 write。
static CRASH_FD: AtomicI32 = AtomicI32::new(-1);
/// 当前阶段，用于定位崩溃点
static STAGE: AtomicUsize = AtomicUsize::new(0);

/// 阶段表：每一项都是编译期常量，handler 直接按下标取用。
const STAGES: [&[u8]; 10] = [
    b"\n[SXDffle] crash before stage 0 (JNI entry)\n",
    b"\n[SXDffle] crash at stage 1/8: ANativeWindow_fromSurface\n",
    b"\n[SXDffle] crash at stage 2/8: spawning render thread\n",
    b"\n[SXDffle] crash at stage 3/8: creating wgpu instance/surface (EGL)\n",
    b"\n[SXDffle] crash at stage 4/8: building Player (navigator/audio/ui)\n",
    b"\n[SXDffle] crash at stage 5/8: fetch_root_movie\n",
    b"\n[SXDffle] crash at stage 6/8: first frame render\n",
    b"\n[SXDffle] crash at stage 7/8: render loop running\n",
    b"\n[SXDffle] crash at stage 8/8: shutting down\n",
    b"\n[SXDffle] crash while idle (embed session already destroyed)\n",
];

/// 更新当前阶段。渲染线程在每一步开头调用一次。
pub fn set_stage(n: usize) {
    STAGE.store(n, Ordering::Release);
}

extern "C" fn on_fatal_signal(sig: i32) {
    let fd = CRASH_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // 只做 write：无锁、无分配、无格式化
        let idx = STAGE.load(Ordering::Relaxed).min(STAGES.len() - 1);
        let stage = STAGES[idx];
        unsafe {
            write(fd, stage.as_ptr(), stage.len());
        }
        write_thread(fd);

        let kind: &[u8] = match sig {
            SIGSEGV => b"  signal: SIGSEGV (invalid memory access)\n",
            SIGABRT => b"  signal: SIGABRT (abort / C++ assert)\n",
            SIGBUS => b"  signal: SIGBUS (bad memory alignment)\n",
            SIGILL => b"  signal: SIGILL (illegal instruction)\n",
            SIGFPE => b"  signal: SIGFPE (arithmetic error)\n",
            _ => b"  signal: unknown fatal signal\n",
        };
        unsafe {
            write(fd, kind.as_ptr(), kind.len());
        }
    }

    // 交还系统：恢复默认处理并重新抛出，保证 tombstone 正常生成
    unsafe {
        signal(sig, SIG_DFL);
        raise(sig);
    }
}

/// 安装 handler 并把日志文件 fd 常驻。
///
/// 幂等：重复调用只会覆盖 fd（旧 fd 泄漏一次，可忽略）。
pub fn install(log_path: &str) {
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let fd = file.into_raw_fd();
        let old = CRASH_FD.swap(fd, Ordering::Release);
        if old >= 0 {
            // 旧 fd 不再使用；不 close 以免 handler 竞争
            let _ = old;
        }
    }

    unsafe {
        signal(SIGSEGV, on_fatal_signal as usize);
        signal(SIGABRT, on_fatal_signal as usize);
        signal(SIGBUS, on_fatal_signal as usize);
        signal(SIGILL, on_fatal_signal as usize);
        signal(SIGFPE, on_fatal_signal as usize);
    }
}
