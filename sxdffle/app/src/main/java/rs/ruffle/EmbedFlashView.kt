package rs.ruffle

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Color
import android.util.Log
import android.view.HapticFeedbackConstants
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.View
import android.view.SurfaceView
import android.widget.FrameLayout
import kotlin.math.abs
import kotlin.math.cos
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min
import kotlin.math.sin

/**
 * 网页内嵌 Flash 播放视图（实验性分支 `experiment/embedded-loader`）。
 *
 * 它不是 Activity —— 只是一个普通 View，可以放进浏览器的 `contentArea` 里任意定位，
 * 因此网页、地址栏、登录态全部保留。
 *
 * # 三条不可动摇的设计约束
 *
 * **1. 引擎分辨率恒定。**
 * `SurfaceView` 的尺寸在引擎启动时**固定**，此后容器怎么变都不改。
 * 缩放与旋转全部只作用于 `SurfaceView.scaleX/scaleY/rotation`。
 * 早先通过 `set_viewport_dimensions(scale_factor)` 实现缩放是错的：
 * 那会让引擎按新视口重新布局，游戏本体跟着缩放，点击坐标也会错位。
 *
 * **2. 屏幕旋转不重建 Surface。**
 * 若让 SurfaceView 跟随容器尺寸变化，旋转屏幕会 `surfaceDestroyed` →
 * 会话被销毁 → 重新下载 SWF → 重新连游戏服务器（还可能顶号）。
 * 固定尺寸后，旋转只改 scale/translation，引擎完全无感。
 *
 * **3. 点击必须和显示对上。**
 * `toSurface()` 要与 `applyTransform()` 严格互逆：先减平移、再逆旋转、再逆缩放。
 *
 * # 为什么本类里没有工具条
 *
 * `SurfaceView` 是「打洞」渲染：它在 Window 上挖掉自己矩形对应的区域，
 * 同一 Window 内与它重叠的普通 View 会被一起挖掉（表现为按钮完全看不见）。
 * 因此所有操作都放在浏览器 ☰ 菜单里。
 */
@SuppressLint("ViewConstructor")
class EmbedFlashView(context: Context) : FrameLayout(context) {

    companion object {
        private const val TAG = "SXDffleEmbed"
        private const val ZOOM_MIN = 0.4f
        private const val ZOOM_MAX = 4.0f

        /** 长按多久进入平移模式 */
        private const val LONG_PRESS_MS = 450L

        /** 手指移动超过这个距离就取消长按 */
        private const val LONG_PRESS_SLOP = 18f

        /** 抬起时位移小于这个值算「点击」 */
        private const val TAP_SLOP = 18f
    }

    /** 启动参数。全部一次性交给原生，之后渲染线程不再回调 Java。 */
    data class Params(
        val swfUrl: String,
        val referer: String,
        val cookie: String,
        val flashVars: String,
        val storageDir: String,
        val traceOutput: String?,
        /** 应用内日志文件绝对路径；引擎日志会双写到这里 */
        val logFile: String,
        /**
         * 物理像素 / CSS 像素。
         *
         * **不是屏幕 density**：WebView 开 `useWideViewPort` 后页面按 980 CSS px
         * 布局再整体缩放，真实比例约 1.1，而 density 是 2.75。
         * 传错会让引擎的逻辑视口只有 393，游戏按 393 布局再铺到 1080 上，画面明显偏小。
         */
        val viewportScale: Float,
        /** 覆盖舞台尺寸；0 = 使用 SWF 自带值 */
        val stageWidth: Int = 0,
        val stageHeight: Int = 0,
    )

    private val surfaceView = SurfaceView(context)

    private var handle: Long = 0L
    private var params: Params? = null

    /** 引擎 surface 的像素尺寸。一旦启动就不再改变（见类注释第 2 条）。 */
    private var surfaceW = 0
    private var surfaceH = 0

    var zoom = 1.0f
        private set

    var rotationDeg = 0
        private set

    /** 平移偏移（容器坐标，像素） */
    private var panX = 0f
    private var panY = 0f

    private var lastPinchDistance = 0f

    // ---- 手势 ----
    /**
     * 单指手势分流：
     *   快速拖动 → 平移画面
     *   短按抬起 → 点击（转发给游戏）
     *   长按后拖动 → 游戏内拖拽（gripMode）
     */
    private var gripMode = false
    private var moved = false

    /**
     * 本次手势里出现过第二根手指。
     *
     * 一旦为真，直到所有手指抬起都不再向游戏发送任何事件。
     * 否则双指捏合结束时（第一根手指最后抬起、位移很小）会被当成"点击"，
     * 游戏里就会莫名触发一次点击。
     */
    private var multiTouch = false
    private var downX = 0f
    private var downY = 0f
    private var lastMoveX = 0f
    private var lastMoveY = 0f

    // ---- 捏合 ----
    private var pinchStartDistance = 0f
    private var pinchStartZoom = 1f

    /** 会话是否已建立（用于菜单操作守卫） */
    val isActive: Boolean get() = handle != 0L

    /** 状态回调：把「未启动 / 失败 / 未出帧 / 运行中」反馈给宿主 */
    var onStatus: ((String) -> Unit)? = null

    /**
     * 切回前台后「连接已断开、等待用户手动重连」的回调。
     *
     * 默认行为（keepAlive=false）：切后台即销毁会话，回前台**不自动重建**。
     * 这是刻意的 —— 自动重建会重新下载 SWF 并重新连服务器，很容易顶号或
     * 触发游戏侧的风控。改为由用户决定：点「重新连接」重建会话，
     * 或点「回到网页」走页面自己的重连/刷新。
     */
    var onDisconnected: (() -> Unit)? = null

    /** 需要用户手动确认后才重建会话（surfaceChanged 时据此跳过自动启动） */
    private var pendingReconnect = false

    /**
     * 锁定画面：锁定时单指操作全部直接转发给游戏，不做平移/缩放。
     * 用户"不想再移动画面、只想正常玩"时打开。
     */
    var locked: Boolean = false
        set(value) {
            field = value
            Log.i(TAG, "画面锁定: $value")
        }

    /**
     * 保持后台运行：切后台时不销毁会话，只暂停渲染。
     *
     * 关（默认）：Surface 销毁即停引擎，回前台重建 —— 会重新下载 SWF 并重连服务器。
     * 开：保留 player 与 TCP 连接，回前台只需挂上新 Surface。
     */
    var keepAlive: Boolean = false

    /** Surface 被销毁但会话仍在，等新的 surfaceChanged 挂载 */
    private var pendingAttach = false

    /** 正在因旋转/重排而调整 Surface；此窗口内 Surface 重建不算「断开」 */
    private var reconfiguring = false

    /** 是否成功启动过（用于区分「首次进入」和「断开后回来」） */
    private var hasEverStarted = false

    /** 断开通知只发一次 */
    private var disconnectNotified = false

    init {
        setBackgroundColor(Color.BLACK)
        addView(
            surfaceView,
            LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT),
        )
    }

    // ------------------------------------------------------------------
    // 生命周期
    // ------------------------------------------------------------------

    fun start(p: Params) {
        params = p
        // 自定义分辨率：Surface 的物理尺寸**直接等于目标分辨率**。
        //
        // 这是「渲染分辨率」真正生效的关键：引擎用 NoScale 模式时
        // stage 尺寸 = 视口逻辑尺寸 = 物理尺寸 / scale_factor，
        // 所以把物理尺寸设成 1360x768、scale_factor 设成 1.0，
        // 游戏读到的 Stage.stageWidth 就是 1360，才会按宽屏重排 UI。
        // 显示时再由 applyTransform 等比缩进容器。
        if (p.stageWidth > 0 && p.stageHeight > 0) {
            surfaceView.layoutParams = LayoutParams(p.stageWidth, p.stageHeight)
            surfaceW = p.stageWidth
            surfaceH = p.stageHeight
        }
        surfaceView.holder.addCallback(holderCallback)
    }

    fun shutdown() {
        stopEngine()
        params = null
    }

    private fun stopEngine() {
        pendingAttach = false
        val h = handle
        if (h != 0L) {
            handle = 0L
            // 先停 present，再停会话。
            //
            // SurfaceView 一旦 removeAllViews，Surface 很快就被系统回收；
            // 如果渲染线程还在 eglSwapBuffers，驱动会在已失效的 buffer 队列上 abort
            // （日志里那 6 次 SIGABRT 都是"关内嵌 → 加载新页面"后 2 秒内发生的）。
            // 先置暂停位可以保证它在下一帧就不再碰 Surface。
            EmbedFlashController.nativeEmbedSetRenderPaused(h, true)
            EmbedFlashController.nativeEmbedStop(h)
            Log.i(TAG, "会话已停止 handle=$h")
        }
        surfaceW = 0
        surfaceH = 0
    }

    override fun onDetachedFromWindow() {
        stopEngine()
        super.onDetachedFromWindow()
    }

    // ------------------------------------------------------------------
    // Surface 回调
    // ------------------------------------------------------------------

    private val holderCallback = object : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {
            // 这里**绝对不能**用 lockCanvas 预涂黑。
            //
            // lockCanvas 会把 Surface 切到 CPU 可写(SW)模式、并重新配置 buffer 队列；
            // 紧接着 EGL 要以 HW_RENDER 用法在同一块 ANativeWindow 上
            // eglCreateWindowSurface —— 会直接 BadAlloc（真机日志已证实）：
            //   WARN  wgpu_hal::gles::egl: Error in create_window_surface: BadAlloc
            // 表现就是"黑屏、完全没有游戏画面"。
            //
            // 白块改由「引擎尽早出首帧」处理（见 src/embed.rs 里的 first_frame 逻辑）。

            if (handle != 0L) {
                // 会话还在（keepAlive）：等 surfaceChanged 拿到尺寸后挂载
                pendingAttach = true
                Log.i(TAG, "Surface 已重建，等待挂载到现有会话")
                return
            }

            // 刻意不在这里启动：此时 View 可能还没 layout，宽高是 0 或旧值。
            // 系统保证 surfaceCreated 后必跟一次 surfaceChanged。
            Log.i(TAG, "Surface 已创建，等待 surfaceChanged 提供准确尺寸")
            surfaceView.postDelayed({
                if (handle == 0L && !pendingReconnect) {
                    val w = surfaceView.width
                    val h = surfaceView.height
                    if (w > 0 && h > 0) {
                        Log.w(TAG, "surfaceChanged 未到达，使用 View 尺寸兜底 ${w}x$h")
                        startEngine(holder, w, h)
                    }
                }
            }, 800)
        }

        private fun startEngine(holder: SurfaceHolder, w: Int, h: Int) {
            val p = params ?: return
            val surface = holder.surface ?: return
            if (handle != 0L || w <= 0 || h <= 0) return
            if (!surface.isValid) {
                Log.w(TAG, "Surface 已失效，放弃启动")
                return
            }
            if (!isAttachedToWindow) {
                Log.w(TAG, "View 已 detach，放弃启动")
                return
            }

            surfaceW = w
            surfaceH = h
            // 关键：把 SurfaceView 从 MATCH_PARENT 改成固定像素。
            // 尺寸与当前完全一致，所以不会触发 surface 重建；
            // 但此后容器变化（旋转、页面重排）就不会再影响它。
            surfaceView.layoutParams = LayoutParams(w, h)
            surfaceView.requestLayout()

            // 自定义分辨率下「物理 = 逻辑」，所以 viewportScale 必须是 1.0
            val useCustom = p.stageWidth > 0 && p.stageHeight > 0
            val vpScale = if (useCustom) 1.0f else p.viewportScale

            Log.i(
                TAG,
                "调用 nativeEmbedStart: ${w}x$h vpScale=$vpScale " +
                    "stage=${p.stageWidth}x${p.stageHeight} surfaceValid=${surface.isValid}"
            )
            handle = EmbedFlashController.nativeEmbedStart(
                surface,
                w,
                h,
                p.swfUrl,
                p.referer,
                p.cookie,
                p.flashVars,
                p.storageDir,
                p.traceOutput,
                vpScale,
                requireNotNull(context as? android.app.Activity) {
                    "EmbedFlashView 必须使用 Activity 作为 Context"
                },
                p.logFile,
                p.stageWidth,
                p.stageHeight,
                EmbedFlashController.audioEnabled,
                EmbedFlashController.externalEnabled,
            )

            Log.i(TAG, "会话创建 handle=$handle 尺寸=${w}x$h swf=${p.swfUrl}")
            if (handle != 0L) hasEverStarted = true
            if (handle == 0L) {
                Log.e(TAG, "引擎启动失败：Surface 可能已失效")
                onStatus?.invoke("启动失败（Surface 无效）")
            } else {
                postDelayed({ healthCheck() }, 3000)
            }
        }

        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
            if (width <= 0 || height <= 0) return

            if (pendingReconnect) {
                // 断开之后回到前台：**不自动重连**，等用户点「重新连接」
                Log.i(TAG, "Surface 就绪，但等待用户手动重连（不自动重建会话）")
                return
            }

            if (handle != 0L && pendingAttach) {
                pendingAttach = false
                val surface = holder.surface
                val ok = surface != null &&
                    EmbedFlashController.nativeEmbedAttachSurface(handle, surface)
                if (ok) {
                    Log.i(TAG, "已挂载新 Surface ${width}x$height")
                    surfaceW = width
                    surfaceH = height
                    EmbedFlashController.nativeEmbedSetRenderPaused(handle, false)
                    // 旋转后的新尺寸必须同步给引擎（视口 = Surface 尺寸）
                    EmbedFlashController.nativeEmbedResize(handle, width, height)
                    applyTransform()
                } else {
                    Log.w(TAG, "挂载新 Surface 失败，重建会话")
                    stopEngine()
                    startEngine(holder, width, height)
                }
                return
            }

            if (handle == 0L) {
                startEngine(holder, width, height)
            } else if (width != surfaceW || height != surfaceH) {
                // 尺寸真的变了（例如 View 被外部改尺寸）：只能重建 surface
                Log.i(TAG, "Surface 尺寸变化 ${width}x$height")
                surfaceW = width
                surfaceH = height
                EmbedFlashController.nativeEmbedResize(handle, width, height)
                applyTransform()
            }
        }

        override fun surfaceDestroyed(holder: SurfaceHolder) {
            if ((keepAlive || reconfiguring) && handle != 0L) {
                // 保留 player 与 TCP：只停掉 present，回前台（或旋转完成）挂个新 Surface 就行
                EmbedFlashController.nativeEmbedSetRenderPaused(handle, true)
                pendingAttach = true
                Log.i(TAG, "Surface 销毁，保持会话（渲染暂停，连接保持）reconfiguring=$reconfiguring")
            } else {
                stopEngine()
                // 只有「本来在播放」才算断开；从未启动过的空视图不该弹重连提示
                // 防重入：隐藏 SurfaceView 也可能再触发一次 destroyed，
                // 这里只通知一次，避免遮罩反复创建。
                if (hasEverStarted && !disconnectNotified) {
                    disconnectNotified = true
                    pendingReconnect = true
                    onDisconnected?.invoke()
                }
            }
        }
    }

    /**
     * 启动后的自检。
     *
     * 黑屏时无法判断是「引擎没起来」还是「起来了但没出帧」，
     * 这两种情况的排查方向相反，所以把状态写进日志。
     */
    private fun healthCheck() {
        val h = handle
        if (h == 0L) {
            onStatus?.invoke("未启动")
            return
        }
        val st = EmbedFlashController.nativeEmbedStatus(h)
        val frames = EmbedFlashController.nativeEmbedFrameCount(h)
        Log.i(TAG, "健康检查: status=$st frames=$frames")
        when {
            st == EmbedFlashController.STATUS_FAILED ->
                onStatus?.invoke("引擎初始化失败，详见日志")

            frames <= 0L ->
                onStatus?.invoke("已启动但未出帧（status=$st）")

            else ->
                onStatus?.invoke("运行中 · 已渲染 $frames 帧")
        }
    }

    // ------------------------------------------------------------------
    // 显示变换
    // ------------------------------------------------------------------

    /**
     * 容器尺寸变化（屏幕旋转）后调用。
     *
     * 关键：没有自定义分辨率时，引擎视口 = Surface 物理尺寸。
     * 旋转后如果不把 Surface 改成新的宽高，引擎仍然按旧横竖比例渲染，
     * 显示层再怎么缩放都只是把错的画面拉一下 —— 表现就是「错层 / 被拉伸」。
     *
     * 自定义分辨率（stageWidth>0）时 Surface 固定为目标分辨率，只需重算显示缩放。
     */
    /**
     * 网页 JS → SWF 的 ExternalInterface 回调（同步，带返回值）。
     *
     * 页面常写 `while (!swf_is_load())` 这种轮询，拿不到返回值就会一直判定"没加载"。
     * 调用发生在 WebView 的 JavaBridge 线程，阻塞它是安全的；原生侧最多等 300ms。
     */
    fun externalCall(name: String, argsJson: String): String {
        val h = handle
        return if (h != 0L) {
            EmbedFlashController.nativeEmbedExternalCall(h, name, argsJson)
        } else {
            ""
        }
    }

    /** 系统内存紧张：让引擎跑一次 GC 并归还空闲页（不中断游戏） */
    fun trimMemory() {
        val h = handle
        if (h != 0L) EmbedFlashController.nativeEmbedTrimMemory(h)
    }

    fun onContainerResized(newW: Int, newH: Int) {
        val p = params ?: return
        if (newW <= 0 || newH <= 0) return

        if (p.stageWidth > 0 && p.stageHeight > 0) {
            applyTransform()
            return
        }
        if (newW == surfaceW && newH == surfaceH) {
            applyTransform()
            return
        }

        Log.i(TAG, "容器尺寸变化，重设 Surface ${surfaceW}x${surfaceH} → ${newW}x$newH")
        // 有的机型在 View 尺寸变化时会把 Surface 销毁重建一次。
        // 这个窗口内必须走「重新挂载」而不是「停会话」，否则旋转一下就断线（顶号风险）。
        reconfiguring = true
        surfaceView.layoutParams = LayoutParams(newW, newH)
        // 交给 surfaceChanged：那里会 nativeEmbedResize（重建 swapchain）
        surfaceView.requestLayout()
        surfaceView.postDelayed({ reconfiguring = false }, 2000)
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        applyTransform()
    }

    /**
     * 计算当前应施加在 SurfaceView 上的旋转 / 缩放 / 平移。
     *
     * 三步都不碰引擎：
     *   base —— 让「旋转后的包围盒」完整放进容器
     *   zoom —— 用户缩放
     *   pan  —— 用户拖拽
     */
    /**
     * 当前应使用的显示缩放（含 base 适配与用户 zoom）。
     *
     * 横屏（容器宽 > 高）时用 **铺满**（max）：画面填满容器，超出部分裁掉。
     * 竖屏时用 **完整**（min）：保证整个游戏画面可见，上下留黑边。
     *
     * 两者都只作用在显示层 —— 引擎的分辨率、布局、点击坐标换算全都不变。
     */
    private fun displayScale(cw: Float, ch: Float): Float {
        val rad = Math.toRadians(rotationDeg.toDouble())
        val c = abs(cos(rad)).toFloat()
        val s = abs(sin(rad)).toFloat()
        val boxW = surfaceW * c + surfaceH * s
        val boxH = surfaceW * s + surfaceH * c
        if (boxW <= 0f || boxH <= 0f) return 1f

        val fill = cw > ch          // 容器横向 → 铺满
        val base = if (fill) max(cw / boxW, ch / boxH) else min(cw / boxW, ch / boxH)
        return base * zoom
    }

    private fun applyTransform() {
        if (surfaceW <= 0 || surfaceH <= 0) return
        val cw = width.toFloat()
        val ch = height.toFloat()
        if (cw <= 0f || ch <= 0f) return

        val scale = displayScale(cw, ch)

        surfaceView.pivotX = surfaceW / 2f
        surfaceView.pivotY = surfaceH / 2f
        surfaceView.rotation = rotationDeg.toFloat()
        surfaceView.scaleX = scale
        surfaceView.scaleY = scale

        clampPan(scale, cw, ch)
        surfaceView.translationX = (cw - surfaceW) / 2f + panX
        surfaceView.translationY = (ch - surfaceH) / 2f + panY

        Log.i(
            TAG,
            "显示变换: 旋转=$rotationDeg zoom=${"%.2f".format(zoom)} " +
                "合计=${"%.2f".format(scale)} ${if (cw > ch) "铺满" else "完整"} " +
                "pan=(${"%.0f".format(panX)},${"%.0f".format(panY)}) " +
                "（引擎 ${surfaceW}x${surfaceH} 不变）"
        )
    }

    /** 平移不能把画面拖到完全看不见 */
    private fun clampPan(scale: Float, cw: Float, ch: Float) {
        val shownW = surfaceW * scale
        val shownH = surfaceH * scale
        val maxX = ((shownW - cw) / 2f).coerceAtLeast(0f) + cw * 0.25f
        val maxY = ((shownH - ch) / 2f).coerceAtLeast(0f) + ch * 0.25f
        panX = panX.coerceIn(-maxX, maxX)
        panY = panY.coerceIn(-maxY, maxY)
    }

    fun rotate() {
        rotationDeg = (rotationDeg + 90) % 360
        // 旋转后原平移量没意义了，归零避免画面飞到看不见的地方
        panX = 0f
        panY = 0f
        applyTransform()
    }

    fun zoomIn() = setZoom(zoom * 1.25f)

    fun zoomOut() = setZoom(zoom / 1.25f)

    private fun setZoom(z: Float) {
        val nz = z.coerceIn(ZOOM_MIN, ZOOM_MAX)
        if (abs(nz - zoom) < 0.005f) return
        zoom = nz
        applyTransform()
    }

    /** 复位缩放与平移（旋转保持不变） */
    fun resetTransform() {
        zoom = 1.0f
        panX = 0f
        panY = 0f
        applyTransform()
    }

    /**
     * 用户点「重新连接」：清掉待重连标记并把 Surface 重新挂出来，
     * 随后系统会回调 surfaceCreated/surfaceChanged，startEngine 重建会话。
     */
    fun reconnect() {
        Log.i(TAG, "用户请求重新连接")
        pendingReconnect = false
        disconnectNotified = false
        overlayHiddenByUser = false
        surfaceView.visibility = View.VISIBLE
        // 尺寸可能已经变了（例如中间转过屏），让 surfaceChanged 重新跑一遍
        surfaceView.requestLayout()
    }

    /**
     * 显示/隐藏 SurfaceView。
     *
     * 断开时必须隐藏：SurfaceView 会「打洞」，不隐藏的话它会把
     * 上层的「已断开，点击重连」遮罩一起挖掉，用户什么也看不见。
     */
    fun setSurfaceVisible(visible: Boolean) {
        surfaceView.visibility = if (visible) View.VISIBLE else View.INVISIBLE
        if (visible) surfaceView.requestLayout()
    }

    private var overlayHiddenByUser = false

    /** 运行时修改舞台尺寸（0,0 = 交给引擎按当前值处理） */
    fun applyStageSize(w: Int, h: Int) {
        if (handle != 0L) {
            EmbedFlashController.nativeEmbedSetStageSize(handle, w, h)
        }
    }

    fun describeTransform(): String {
        val parts = mutableListOf("缩放 ${"%.0f".format(zoom * 100)}%")
        if (rotationDeg != 0) parts += "旋转 $rotationDeg°"
        if (abs(panX) > 1f || abs(panY) > 1f) parts += "已平移"
        return parts.joinToString(" / ")
    }

    /**
     * 容器坐标 → Surface 像素坐标。
     *
     * 顺序必须与 [applyTransform] 完全相反：
     *   applyTransform: surface 坐标 → 缩放 → 旋转 → 平移 → 容器坐标
     *   toSurface     : 容器坐标 → 减平移 → 逆旋转 → 逆缩放 → surface 坐标
     */
    private fun toSurface(x: Float, y: Float): Pair<Float, Float> {
        if (surfaceW <= 0 || surfaceH <= 0) return x to y
        val cw = width.toFloat()
        val ch = height.toFloat()
        val scale = displayScale(cw, ch)
        if (scale <= 0f) return x to y

        // 1) 容器坐标 → 以 surface 中心为原点的坐标
        val dx = x - (cw - surfaceW) / 2f - panX - surfaceW / 2f
        val dy = y - (ch - surfaceH) / 2f - panY - surfaceH / 2f

        // 2) 逆旋转
        val ir = Math.toRadians(-rotationDeg.toDouble())
        val ic = cos(ir).toFloat()
        val isin = sin(ir).toFloat()
        val rx = dx * ic - dy * isin
        val ry = dx * isin + dy * ic

        // 3) 逆缩放
        return (rx / scale + surfaceW / 2f) to (ry / scale + surfaceH / 2f)
    }

    // ------------------------------------------------------------------
    // 触摸
    // ------------------------------------------------------------------

    /**
     * 长按 → 进入「游戏拖拽」模式。
     *
     * 注意这里才发 MouseDown：如果把 DOWN 立刻转发给游戏，
     * 用户在平移画面时游戏也会收到按下/拖拽，角色会乱跑。
     */
    private val longPressRunnable = Runnable {
        if (handle == 0L) return@Runnable
        gripMode = true
        performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
        sendTouch(EmbedFlashController.PHASE_DOWN, downX, downY)
        Log.i(TAG, "长按 → 进入游戏拖拽模式")
        onStatus?.invoke("拖拽模式")
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (handle == 0L) return false

        // 锁定画面：全部单指操作直通游戏，不做平移/长按分流/缩放
        if (locked) {
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN ->
                    sendTouch(EmbedFlashController.PHASE_DOWN, event.x, event.y)

                MotionEvent.ACTION_MOVE ->
                    if (event.pointerCount < 2) {
                        sendTouch(EmbedFlashController.PHASE_MOVE, event.x, event.y)
                    }

                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL ->
                    sendTouch(EmbedFlashController.PHASE_UP, event.x, event.y)
            }
            return true
        }

        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                downX = event.x
                downY = event.y
                lastMoveX = event.x
                lastMoveY = event.y
                gripMode = false
                moved = false
                multiTouch = false
                removeCallbacks(longPressRunnable)
                postDelayed(longPressRunnable, LONG_PRESS_MS)
                // 这里**不发** MouseDown：等确认是点击还是拖拽
            }

            MotionEvent.ACTION_POINTER_DOWN -> {
                // 第二指落下 → 本次手势彻底转为"画面操作"，不再影响游戏
                multiTouch = true
                removeCallbacks(longPressRunnable)
                moved = true   // 让 UP 分支不把这次手势当成点击
                if (event.pointerCount >= 2) {
                    pinchStartDistance = hypot(
                        event.getX(0) - event.getX(1),
                        event.getY(0) - event.getY(1),
                    )
                    pinchStartZoom = zoom
                    // 捏合前如果已经进入拖拽模式，先松开，避免游戏中卡住
                    if (gripMode) {
                        sendTouch(EmbedFlashController.PHASE_UP, downX, downY)
                        gripMode = false
                    }
                }
            }

            MotionEvent.ACTION_MOVE -> {
                if (multiTouch) {
                    // 双指手势（含松开一指后的尾声）只做缩放，绝不转发给游戏
                    if (event.pointerCount >= 2 && pinchStartDistance > 0f) {
                        val d = hypot(
                            event.getX(0) - event.getX(1),
                            event.getY(0) - event.getY(1),
                        )
                        setZoom(pinchStartZoom * (d / pinchStartDistance))
                    }
                    return true
                }
                if (event.pointerCount >= 2) {
                    // 捏合：用「起始距离 + 起始 zoom」算绝对比例。
                    // 之前的增量累乘（zoom *= d/lastD）会累积误差，手指一抖就跳。
                    if (pinchStartDistance > 0f) {
                        val d = hypot(
                            event.getX(0) - event.getX(1),
                            event.getY(0) - event.getY(1),
                        )
                        setZoom(pinchStartZoom * (d / pinchStartDistance))
                    }
                } else if (gripMode) {
                    // 长按后的拖动 → 交给游戏（游戏内拖拽）
                    lastMoveX = event.x
                    lastMoveY = event.y
                    sendTouch(EmbedFlashController.PHASE_MOVE, event.x, event.y)
                } else {
                    // 普通拖动 → 平移画面
                    if (hypot(event.x - downX, event.y - downY) > TAP_SLOP) {
                        moved = true
                        removeCallbacks(longPressRunnable)
                    }
                    panX += event.x - lastMoveX
                    panY += event.y - lastMoveY
                    lastMoveX = event.x
                    lastMoveY = event.y
                    applyTransform()
                }
            }

            MotionEvent.ACTION_POINTER_UP -> {
                pinchStartDistance = 0f
            }

            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                removeCallbacks(longPressRunnable)
                pinchStartDistance = 0f

                if (gripMode) {
                    // 结束拖拽
                    sendTouch(EmbedFlashController.PHASE_UP, event.x, event.y)
                } else if (!multiTouch && !moved && event.actionMasked == MotionEvent.ACTION_UP) {
                    // 短按未移动 → 视为点击，补发一对按下/抬起
                    sendTouch(EmbedFlashController.PHASE_DOWN, downX, downY)
                    sendTouch(EmbedFlashController.PHASE_UP, downX, downY)
                }
                gripMode = false
                moved = false
                multiTouch = false
            }
        }
        return true
    }

    private fun sendTouch(phase: Int, x: Float, y: Float) {
        val (sx, sy) = toSurface(x, y)
        EmbedFlashController.nativeEmbedTouch(handle, phase, sx, sy)
    }
}
