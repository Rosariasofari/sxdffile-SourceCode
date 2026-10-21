//! 嵌入式宿主的 JNI 边界。
//!
//! 所有函数都绑定到 Kotlin 的 `rs.ruffle.EmbedFlashController`。
//! 设计原则：**只做参数搬运，不持有任何 Java 引用**——渲染线程完全运行在原生世界，
//! 这样即使 Activity 被回收、WebView 刷新，也不会出现悬空的全局引用。

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring};
use jni::errors::ThrowRuntimeExAndDefault;
use jni::EnvUnowned;

use crate::embed::{self, AndroidSurfaceWindow, EmbedConfig, EmbedSession};

/// handle 是 `Box<EmbedSession>` 的裸指针。0 表示无效。
fn as_session<'a>(handle: jlong) -> Option<&'a EmbedSession> {
    if handle == 0 {
        None
    } else {
        Some(unsafe { &*(handle as *const EmbedSession) })
    }
}

fn get_string(env: &mut jni::Env, s: &JString) -> String {
    if s.as_raw().is_null() {
        return String::new();
    }
    s.try_to_string(env).unwrap_or_default()
}

/// 创建嵌入式播放会话。
///
/// 参数顺序必须与 Kotlin 侧 `external fun nativeEmbedStart` **严格一致**。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedStart<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    surface: JObject<'local>,
    width: jint,
    height: jint,
    swf_url: JString<'local>,
    referer: JString<'local>,
    cookie: JString<'local>,
    flash_vars: JString<'local>,
    storage_dir: JString<'local>,
    trace_output: JString<'local>,
    viewport_scale: jfloat,
    activity: JObject<'local>,
    log_path: JString<'local>,
    stage_w: jint,
    stage_h: jint,
    audio_on: jboolean,
    external_on: jboolean,
) -> jlong {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jlong> {
            let config = EmbedConfig {
                swf_url: get_string(env, &swf_url),
                referer: get_string(env, &referer),
                cookie: get_string(env, &cookie),
                flash_vars: get_string(env, &flash_vars),
                storage_dir: get_string(env, &storage_dir),
                trace_output: {
                    let t = get_string(env, &trace_output);
                    if t.is_empty() { None } else { Some(t) }
                },
                viewport_scale: viewport_scale as f64,
                width: width.max(1) as u32,
                height: height.max(1) as u32,
                // 0 表示用 SWF 自带舞台尺寸
                stage_size: if stage_w > 0 && stage_h > 0 {
                    Some((stage_w as u32, stage_h as u32))
                } else {
                    None
                },
                enable_audio: audio_on,
                enable_external_interface: external_on,
            };

            // ---- 日志双写 ----
            // 全屏版在 nativeInit 里做这件事；内嵌路径冷启动时若没做过，
            // 引擎的所有 log::info! 只进 logcat —— 用户不接电脑就完全看不到，
            // 「黑屏」这类问题会变成纯猜。这里补上，让日志同时落进 sxdffle.log。
            let log_file = get_string(env, &log_path);
            if !log_file.is_empty() {
                if let Err(e) = crate::trace::init_app_log(std::path::Path::new(&log_file)) {
                    log::warn!("SXDffle embed: 初始化日志文件失败: {e}");
                }
            }
            crate::trace::TeeLogger::install(
                android_logger::Config::default()
                    .with_max_level(log::LevelFilter::Info)
                    .with_tag("ruffle")
                    .with_filter(
                        android_logger::FilterBuilder::new()
                            .parse("warn,ruffle=info,sxdffle=info")
                            .build(),
                    ),
            );
            log::info!("SXDffle embed: 日志已接入（file={}）", log_file);

            // ---- Android 信任库接入 ----
            // 全屏版在 run() 的初始化段做这件事，内嵌路径必须自己做，
            // 否则 reqwest 用不到系统 CA，HTTPS 请求会失败（表现为资源加载不出来）。
            {
                static PV_INIT: std::sync::Once = std::sync::Once::new();
                PV_INIT.call_once(|| {
                    match rustls_platform_verifier::android::init_with_env(env, activity) {
                        Ok(()) => log::info!("SXDffle embed: rustls platform verifier 已初始化"),
                        Err(e) => log::warn!("SXDffle embed: platform verifier 初始化失败: {e:?}"),
                    }
                });
            }

            // 取 ANativeWindow。注意 env 是 jni-sys 0.4 的类型，
            // 而 C 函数只关心它是一个 JNIEnv*，用 c_void 传递即可。
            let raw_env = env.get_raw() as *mut std::ffi::c_void;
            let raw_surface = surface.as_raw() as *mut std::ffi::c_void;

            // 先装致命信号捕获：从这一刻起，native 崩溃也会在日志里留下"死在哪一步"
            crate::crash::install(&log_file);
            crate::crash::set_stage(1);

            let window = match AndroidSurfaceWindow::from_surface(raw_env, raw_surface) {
                Some(w) => w,
                None => {
                    log::error!("SXDffle embed: ANativeWindow_fromSurface 返回空，Surface 可能已失效");
                    return Ok(0);
                }
            };

            crate::crash::set_stage(2);
            let session = embed::start(config, window);
            let handle = Box::into_raw(session) as jlong;
            log::info!("SXDffle embed: 会话已创建 handle={}", handle);
            Ok(handle)
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedStop<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // 交回所有权并 drop：EmbedSession::drop 会发 Stop 并 join 渲染线程，
    // 保证 ANativeWindow 的 release 发生在最后一次 present 之后。
    let _ = Box::from_raw(handle as *mut EmbedSession);
    log::info!("SXDffle embed: 会话已销毁 handle={}", handle);
    // 会话没了 → 之后任何 native 崩溃都不是引擎析构导致的。
    // 打上"空闲"标记，下次日志里的 stage 就不会再被 stage 8 误导。
    crate::crash::set_stage(9);
}

#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedResize<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    width: jint,
    height: jint,
) {
    if let Some(s) = as_session(handle) {
        s.resize(width.max(1) as u32, height.max(1) as u32);
    }
}

#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedTouch<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    phase: jint,
    x: jfloat,
    y: jfloat,
) {
    if let Some(s) = as_session(handle) {
        s.touch(phase as u8, x, y);
    }
}

#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedScroll<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    dy: jfloat,
) {
    if let Some(s) = as_session(handle) {
        s.scroll(dy);
    }
}

#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedKey<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    down: jboolean,
    tag: JString<'local>,
) {
    unowned_env.with_env(|env| -> jni::errors::Result<()> {
        if let Some(s) = as_session(handle) {
            s.key(down, get_string(env, &tag));
        }
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}

#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedSetZoom<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    zoom: jfloat,
) {
    if let Some(s) = as_session(handle) {
        s.set_zoom(zoom);
    }
}

/// 网页 JS 调用 SWF 注册的回调（ExternalInterface.addCallback）。
///
/// 参数用 JSON 数组传，避免为每种类型单独开 JNI 方法。
#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedExternalCall<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    name: JString<'local>,
    args: JString<'local>,
) -> jstring {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jstring> {
            let out = match as_session(handle) {
                Some(s) => {
                    s.external_call_sync(get_string(env, &name), get_string(env, &args))
                }
                None => String::new(),
            };
            Ok(env.new_string(out)?.into_raw())
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// 系统内存紧张时调用：强制 GC + 归还空闲页。
#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedTrimMemory<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    let _ = unowned_env.with_env(|_env| -> jni::errors::Result<()> {
        if let Some(s) = as_session(handle) {
            s.trim_memory_now();
        }
        Ok(())
    });
}

/// 查询会话状态（0=初始化中 1=运行中 2=失败 3=已停止）。
#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedStatus<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jint {
    as_session(handle).map(|s| s.status()).unwrap_or(-1)
}

/// 已渲染帧数。Kotlin 侧用它判断「黑屏是因为没出帧」还是「出了帧但内容不对」。
#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedFrameCount<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    as_session(handle).map(|s| s.frame_count() as jlong).unwrap_or(-1)
}

/// 运行时修改舞台尺寸（0,0 = 恢复 SWF 自带值）。
#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedSetStageSize<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    width: jint,
    height: jint,
) {
    if let Some(s) = as_session(handle) {
        s.set_stage_size(width.max(0) as u32, height.max(0) as u32);
    }
}

/// 暂停/恢复渲染。切后台时调用，player 与 TCP 连接保持存活。
#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedSetRenderPaused<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    paused: jboolean,
) {
    if let Some(s) = as_session(handle) {
        s.set_render_paused(paused);
    }
}

/// 切回前台后挂上新 Surface（系统重建过 Surface，旧的 ANativeWindow 已失效）。
#[no_mangle]
pub unsafe extern "C" fn Java_rs_ruffle_EmbedFlashController_nativeEmbedAttachSurface<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    surface: JObject<'local>,
) -> jboolean {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jboolean> {
            let Some(s) = as_session(handle) else {
                return Ok(false);
            };
            let raw_env = env.get_raw() as *mut std::ffi::c_void;
            let raw_surface = surface.as_raw() as *mut std::ffi::c_void;
            match AndroidSurfaceWindow::from_surface(raw_env, raw_surface) {
                Some(w) => {
                    s.attach_surface(w);
                    Ok(true)
                }
                None => {
                    log::error!("SXDffle embed: attachSurface 拿到空窗口");
                    Ok(false)
                }
            }
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}
