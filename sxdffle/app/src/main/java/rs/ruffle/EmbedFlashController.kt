package rs.ruffle

/**
 * 嵌入式 Flash 宿主的 JNI 边界（实验性分支 `experiment/embedded-loader`）。
 *
 * 与全屏版 [PlayerActivity] 的区别：这里不启动 Activity，而是把 Kotlin 侧的
 * `Surface` 直接交给原生引擎，由引擎在一独立线程上渲染。网页、地址栏、登录态
 * 全部保留，Flash 只是网页上的一个「视图」。
 *
 * 原生实现见 `src/embed.rs` / `src/embed_jni.rs`。
 */
object EmbedFlashController {

    init {
        // 与 GameActivity 用的 lib_name 是同一个库；重复 load 是安全的
        System.loadLibrary("sxdffle")
    }

    /** 触摸阶段，必须与 Rust 侧 `PHASE_*` 常量一致 */
    const val PHASE_DOWN = 0
    const val PHASE_MOVE = 1
    const val PHASE_UP = 2

    /** 会话状态，与 Rust 侧 `STATUS_*` 一致 */
    /**
     * 调试开关：下次启动会话时生效。
     *
     * 压测时单局内存以 ~1.6MB/帧 增长，需要二分是音频还是 ExternalInterface
     * 引起的，所以在调试菜单里给了这两个开关。
     */
    @JvmField
    var audioEnabled = true

    @JvmField
    var externalEnabled = true

    const val STATUS_INIT = 0
    const val STATUS_RUNNING = 1
    const val STATUS_FAILED = 2
    const val STATUS_STOPPED = 3

    /**
     * 创建会话。返回 0 表示失败（通常是 Surface 已失效）。
     *
     * 该调用立即返回，真正的引擎初始化发生在原生渲染线程里；
     * 用 [nativeEmbedStatus] 轮询是否已进入渲染循环。
     */
    @JvmStatic
    external fun nativeEmbedStart(
        surface: android.view.Surface,
        width: Int,
        height: Int,
        swfUrl: String,
        referer: String,
        cookie: String,
        flashVars: String,
        storageDir: String,
        traceOutput: String?,
        /** 物理像素 / CSS 像素（不是屏幕 density，见 native 侧说明） */
        viewportScale: Float,
        activity: android.app.Activity,
        logPath: String,
        /** 覆盖舞台尺寸；0,0 表示用 SWF 自带值 */
        stageWidth: Int,
        stageHeight: Int,
        /** 调试开关：音频后端（false = 静音运行） */
        audioEnabled: Boolean,
        /** 调试开关：ExternalInterface */
        externalEnabled: Boolean,
    ): Long

    /** 停止并销毁会话（会等待渲染线程退出，避免 Surface 被提前释放）。 */
    @JvmStatic
    external fun nativeEmbedStop(handle: Long)

    @JvmStatic
    external fun nativeEmbedResize(handle: Long, width: Int, height: Int)

    @JvmStatic
    external fun nativeEmbedTouch(handle: Long, phase: Int, x: Float, y: Float)

    @JvmStatic
    external fun nativeEmbedScroll(handle: Long, dy: Float)

    @JvmStatic
    external fun nativeEmbedKey(handle: Long, down: Boolean, tag: String)

    @JvmStatic
    external fun nativeEmbedSetZoom(handle: Long, zoom: Float)

    @JvmStatic
    external fun nativeEmbedStatus(handle: Long): Int

    /** 已渲染帧数；-1 表示句柄无效 */
    @JvmStatic
    external fun nativeEmbedFrameCount(handle: Long): Long

    /**
     * 运行时修改舞台尺寸（0,0 = 交给引擎按当前值处理）。
     *
     * 对应 Ruffle 的 `Stage::set_movie_size` + `build_matrices`：
     * 游戏会按新的 `Stage.stageWidth/Height` 重新布局，
     * 用于「SWF 自带尺寸太小、某些功能挤在一起」的场合。
     */
    @JvmStatic
    external fun nativeEmbedSetStageSize(handle: Long, width: Int, height: Int)

    /** 暂停/恢复渲染。暂停期间 player 与 TCP 连接保持存活（切后台用）。 */
    @JvmStatic
    external fun nativeEmbedSetRenderPaused(handle: Long, paused: Boolean)

    /** 切回前台后挂上新 Surface；返回 false 表示窗口无效，应重建会话。 */
    @JvmStatic
    external fun nativeEmbedAttachSurface(handle: Long, surface: android.view.Surface): Boolean

    /**
     * 网页 JS 调用 SWF 通过 `ExternalInterface.addCallback` 注册的方法。
     *
     * 页游把 `<embed>` 当成双向通道：SWF 注册回调、网页用
     * `document.getElementById(id).函数名(...)` 调用。引擎跑在网页之外，
     * DOM 上的 `<embed>` 是死元素，所以这条链路要靠这里补上。
     *
     * 参数统一用 JSON 数组传，避免为每种类型开一个 JNI 方法。
     */
    @JvmStatic
    external fun nativeEmbedExternalCall(handle: Long, name: String, argsJson: String): String

    /** 系统内存紧张：强制 GC + 归还空闲页（不中断游戏） */
    @JvmStatic
    external fun nativeEmbedTrimMemory(handle: Long)
}
