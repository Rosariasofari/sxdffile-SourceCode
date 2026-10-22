package rs.ruffle

import android.annotation.SuppressLint
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.graphics.drawable.ColorDrawable
import android.util.Log
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.inputmethod.EditorInfo
import android.webkit.CookieManager
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.PopupWindow
import android.widget.ProgressBar
import android.widget.TextView
import android.widget.Toast
import androidx.activity.OnBackPressedCallback
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.FileProvider
import androidx.core.view.doOnLayout
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import java.io.File
import kotlin.math.abs
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * SXDffle 浏览器主界面。
 *
 * 架构说明：
 *  - 普通网页（HTML/JS）由 WebView 正常渲染；
 *  - 页面中的 Flash 内容（<embed>/<object>/…）无法在 WebView 中执行，
 *    由注入脚本 [SXDffleInject] 识别并叠加可点击遮罩；
 *  - 用户点击后，把 SWF 地址连同 **页面地址(Referer)** 与 **Cookie(登录态)**
 *    交给 [PlayerActivity]，由内置的原生 Flash 引擎（Ruffle 运行时）渲染。
 *
 * 之所以要传递 Referer/Cookie：页游普遍有防盗链与登录态校验，
 * 而原生引擎的网络栈是独立于 WebView 的，必须显式桥接。
 */
class BrowserActivity : AppCompatActivity() {

    private lateinit var webView: WebView
    private lateinit var urlBar: EditText
    private lateinit var progress: ProgressBar

    /** 当前缩放百分比（作用于整个网页） */
    private var zoomPercent = 100

    /** 是否为桌面模式（影响 UA 与视口） */
    private var desktopMode = true

    /** 缓存注入脚本，避免每次加载都读 assets */
    private val injectScript: String by lazy { readAsset("sxdffle_inject.js") }

    /**
     * 文档起手注入脚本：伪造 Flash 插件信息。
     *
     * 必须在页面脚本执行前生效 —— 页游会用 `navigator.plugins['Shockwave Flash']`
     * 判断用户是否装了 Flash（《神仙道》的 common.js 就是如此），
     * 而该值在移除 PPAPI 后的 Chromium 内核中恒为 undefined，
     * 会导致页面判定"未安装 Flash"而拒绝渲染游戏入口。
     */
    private val flashSpoofScript: String by lazy { readAsset("sxdffle_flash_spoof.js") }

    private fun readAsset(name: String): String = try {
        assets.open(name).bufferedReader().use { it.readText() }
    } catch (e: Exception) {
        Log.e(TAG, "读取 assets/$name 失败", e)
        ""
    }

    /** document-start 注入是否可用；不可用时退化为 onPageStarted 注入 */
    private var documentStartSupported = false

    /** 检测脚本是否已通过 document-start 注入到所有框架 */
    private var injectStartSupported = false

    /**
     * 已「提升为顶层」的游戏入口地址。
     *
     * 联运平台的入口常常是 frameset / 自动提交表单，里面套着真正的游戏页。
     * 提升过的地址记在这里，防止页面回弹后又被提升一遍形成死循环。
     */
    private val promotedUrls = LinkedHashSet<String>()

    /** 最近几次提升的时间戳（限流用） */
    private val promoteTimes = ArrayDeque<Long>()

    /** 是否已因频繁提升而暂停（避免日志刷屏） */
    private var promotePaused = false

    /** 上一次提升：从哪来、提到哪、什么时候（用于识别"提上去又被弹回来"） */
    private var promoteFromUrl: String? = null
    private var promoteTargetUrl: String? = null
    private var promoteAtMs = 0L

    /** 回弹过的 host：这些站点的 iframe 不再提升（51 就是这种） */
    private val promoteBannedHosts = mutableSetOf<String>()

    /**
     * Flash 显示方式：true = 网页内嵌（实验性），false = 全屏播放。
     *
     * 内嵌模式下 Flash 画面是覆盖在 WebView 上的原生 SurfaceView，
     * 位置跟随页面中 Flash 元素，浏览器 UI 与登录态完全保留。
     */
    private var embedMode = true

    /** 当前内嵌播放视图；null 表示没有正在内嵌的 Flash */
    private var embedView: EmbedFlashView? = null

    /** 内嵌播放的启动参数，供工具条「⤢ 全屏」复用 */
    private var embedSwfUrl = ""
    private var embedPageUrl = ""
    private var embedRectW = 0
    private var embedRectH = 0
    private var embedVars = ""

    /** 当前内嵌是否来自子框架（frameset / iframe）：此时忽略网页滚动同步 */
    private var embedFromFrame = false

    /** 覆盖舞台尺寸；0 = 用 SWF 自带值 */
    private var stageW = 0
    private var stageH = 0

    /** 切后台是否保持游戏连接（默认关） */
    private var keepAlive = false

    /** 收藏夹删除前是否弹确认（默认开） */
    private var deleteConfirm = true

    /** 调试功能开关：开启后才显示渲染/缩放的底层旋钮（默认关） */
    private var debugMode = false

    /** 调试开关：音频 / ExternalInterface（用于二分内存增长来源，下次进入生效） */
    private var audioEnabled = true
    private var externalEnabled = true

    /** flash.cn 游戏页最近几次进入的时间，用来识别"登录票据被拒 → 无限回登录页" */
    private val flashPlayTimes = ArrayDeque<Long>()

    /** 每轮循环只提示一次 */
    private var flashLoopHinted = false

    /** 内存看门狗：定期读 RSS，过高就主动降级/关闭内嵌，避免进程被 abort */
    private val memHandler = android.os.Handler(android.os.Looper.getMainLooper())
    private var memHighWarned = false

    private val memWatchdog = object : Runnable {
        override fun run() {
            if (embedView != null) {
                val rss = currentRssMb()
                // 只观察、不干预：用户需要长时间挂机，强行关闭会打断游戏。
                // 内存控制改由引擎侧做（渲染节流 + purge），这里只负责留证据。
                when {
                    rss >= 3200 -> logToFile("内存看门狗: RSS ${rss}MB，已接近系统上限")
                    rss >= 1800 -> if (!memHighWarned) {
                        memHighWarned = true
                        logToFile("内存看门狗: RSS ${rss}MB 偏高（继续观察）")
                    }
                    else -> memHighWarned = false
                }
            }
            memHandler.postDelayed(this, 5_000)
        }
    }

    /** 顶部栏是否 3 秒后自动收起（默认关） */
    private var autoHideToolbar = false

    /** 画面是否锁定 */
    private var locked = false


    private val hideToolbarRunnable = Runnable { setToolbarExpanded(false) }

    /** 顶部栏当前是否处于收起状态（高度动画期间也按目标状态算） */
    private var toolbarCollapsed = false

    /**
     * 唤出顶部栏的手势跟踪。
     *
     * 0=无 1=左边缘 2=右边缘 3=上边缘 4=下边缘 5=顶部区域
     *
     * 系统状态栏的下拉是系统手势，App 无法拦截，所以主路径不依赖"从顶部下拉"，
     * 而是用**边缘内滑**（竖屏左右 / 横屏上下）—— 这正是系统返回手势的区域，
     * 我们抢先消费掉一次，用来唤出顶部栏。
     */
    private var edgeMode = 0
    private var edgeDownX = 0f
    private var edgeDownY = 0f

    /** 沉浸模式（隐藏状态栏/导航栏），默认开启 */
    private var immersive = true

    /**
     * 导航栏不透明度百分比（0..100）。
     *
     * 导航栏是盖在页面上的悬浮层，全不透明会显得"压住"内容；
     * 调低后能透出底下的网页/游戏，更像一层浮窗。
     * 文字始终不透明，所以透明度再低保可读性不受影响。
     */
    private var toolbarAlphaPercent = 100

    /** 当前平台 key（仅用于日志与登录态导出时标注） */
    private var currentPlatform = "xd"

    /** 系统状态栏高度（沉浸模式下为 0，非沉浸模式用来给悬浮栏让位） */
    private var statusBarInset = 0

    /** 是否处于「内嵌播放」的界面状态（此时用悬浮工具栏） */
    private var embedUiActive = false

    /**
     * 导航栏宿主。
     *
     * 整条导航栏放在 **PopupWindow（独立 Window）** 里，而不是 Activity 布局中：
     * SurfaceView 会在 Window 上打洞，同一 Window 内的普通 View 与它重叠
     * 就会被整块挖掉。要让它「浮在游戏画面上」还能随时收起，只能放另一个 Window。
     */
    private lateinit var toolbarView: View
    private lateinit var toolbarBar: View
    private var toolbarPopup: PopupWindow? = null

    /**
     * 导航栏收起后的菜单悬浮球（右上角）。
     *
     * 同样是独立 Window —— 要浮在原生渲染的游戏画面之上，只能这么做。
     * 点击它弹出与 ☰ 完全相同的菜单，视觉上相当于「菜单从导航栏缩成了一个球」。
     */
    private var ballPopup: PopupWindow? = null
    private var ballView: View? = null

    /** 悬浮球当前位置（屏幕坐标）；-1 表示还没摆过，用默认右上角 */
    private var ballX = -1
    private var ballY = -1
    private var ballDownRawX = 0f
    private var ballDownRawY = 0f
    private var ballStartX = 0
    private var ballStartY = 0
    private var ballDragging = false
    private var ballTouchSlop = 0f


    /** 已尝试过 https→http 降级的域名，避免反复跳转 */
    private val downgradedHosts = HashSet<String>()

    /**
     * 首页地址（可在菜单里自定义）。
     *
     * 注意：这里不能是 val —— 用户设完首页后要立刻生效，
     * 而且下次启动要走 SharedPreferences 读回来。
     */
    private var homeUrl = DEFAULT_HOME

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // 日志轮转要放在任何写入之前：只保留最近 5 份
        rotateLogs()

        // 把未捕获异常也写进日志文件 —— 真机闪退时这是唯一能拿到的线索
        // （native crash 抓不到，需要 adb logcat；Java 异常在这里能留住）。
        installCrashLogger()

        setContentView(R.layout.activity_browser)
        loadStageSize()
        loadUiPrefs()
        // 首次启动把内置的联运平台清单导进收藏夹（之后就是一个普通分组）
        importDefaultBookmarksOnce()

        // 版本号写进日志：之后拿到日志就能确认跑的是哪一版，
        // 不用再靠行为反推（这几轮排查吃过亏）。
        logToFile(
            "SXDffle 启动 v${appVersionName()} " +
                "stage=${stageSizeLabel()} density=${resources.displayMetrics.density}"
        )

        webView = findViewById(R.id.webView)
        progress = findViewById(R.id.progress)

        setupWebView()
        setupToolbar()
        setupUi()

        // 导航栏默认显示（它在独立窗口里，必须显式 show 出来）
        findViewById<View>(R.id.contentArea).post {
            memHandler.postDelayed(memWatchdog, 5_000)
            showMemChip()
            updateContentInset(true, animate = false)
            updateGestureExclusion()
            showToolbar()
        }

        // 内容区尺寸一变就重排内嵌视图。
        //
        // 比只在 onConfigurationChanged 里处理更可靠：旋转、工具栏收起/展开、
        // 沉浸模式切换、系统栏出现，最终都会体现为 contentArea 的 bounds 变化。
        // 之前只监听配置变化，遇到「容器是固定像素」的情况就完全没人重算，
        // 于是旋转后画面停在旧尺寸上（错层 / 越转越小）。
        findViewById<View>(R.id.contentArea).addOnLayoutChangeListener { _, l, t, r, b, ol, ot, orr, ob ->
            if (l == ol && t == ot && r == orr && b == ob) return@addOnLayoutChangeListener
            if (embedView != null) relayoutEmbed()
        }

        // 沉浸模式默认开启：状态栏/导航栏隐藏，画面独占整屏
        applyImmersive()

        // 返回键：网页可后退则后退，否则退出
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                // 返回分两级，不再有"先关内嵌"这一步：
                //   ① 导航栏收起 → 第一次返回只把栏唤出来（防止全屏游戏里一按就退）
                //   ② 栏已经显示 → 直接回上一页（短时间内的第二次返回自然落到这里）
                if (toolbarCollapsed) {
                    setToolbarExpanded(true)
                    showStatus("顶部栏已唤出 · 再返回一次可回上一页")
                    return
                }
                if (webView.canGoBack()) {
                    webView.goBack()
                } else {
                    isEnabled = false
                    onBackPressedDispatcher.onBackPressed()
                }
            }
        })

        // 重启进程时会把目标地址带在 extra 里（见 restartWith）
        val pendingUrl = intent?.getStringExtra(EXTRA_OPEN_URL)?.takeIf { it.isNotBlank() }
        val startUrl = pendingUrl
            ?: intent?.dataString?.takeIf { it.isNotBlank() }
            ?: homeUrl
        if (pendingUrl != null) logToFile("重启后恢复地址: $pendingUrl")
        webView.loadUrl(startUrl)
    }

    // ==================================================================
    //  WebView 配置
    // ==================================================================

    @SuppressLint("SetJavaScriptEnabled")
    private fun setupWebView() {
        val s = webView.settings
        s.javaScriptEnabled = true
        s.domStorageEnabled = true
        s.databaseEnabled = true
        s.allowFileAccess = true
        s.allowContentAccess = true
        s.javaScriptCanOpenWindowsAutomatically = true
        s.mediaPlaybackRequiresUserGesture = false
        s.setSupportZoom(true)
        s.builtInZoomControls = true
        s.displayZoomControls = false
        s.useWideViewPort = true
        s.loadWithOverviewMode = true
        s.cacheMode = WebSettings.LOAD_DEFAULT
        s.loadsImagesAutomatically = true
        s.mixedContentMode = WebSettings.MIXED_CONTENT_ALWAYS_ALLOW
        applyUserAgent()

        // 老页游常依赖 DOM Storage / 第三方 Cookie
        CookieManager.getInstance().setAcceptCookie(true)
        CookieManager.getInstance().setAcceptThirdPartyCookies(webView, true)

        // 页游入口普遍用 window.open(gameUrl) 打开游戏（《神仙道》的
        // func_list.play_game 就是 window.open(url)）。
        // setSupportMultipleWindows(false) 会让 window.open 在当前 WebView 内加载，
        // 这样后续的 Flash 侦测与接管才能生效；若设为 true 则必须实现
        // WebChromeClient.onCreateWindow，否则 window.open 会被静默忽略（点了没反应）。
        s.setSupportMultipleWindows(false)
        s.javaScriptCanOpenWindowsAutomatically = true

        webView.addJavascriptInterface(FlashBridge(), "SxdffleBridge")

        // ---- 注册「文档起手」注入：伪造 Flash 插件，必须早于页面脚本执行 ----
        if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT) &&
            flashSpoofScript.isNotBlank()
        ) {
            try {
                WebViewCompat.addDocumentStartJavaScript(webView, flashSpoofScript, setOf("*"))
                documentStartSupported = true
                Log.i(TAG, "Flash 插件伪造脚本已注册（document-start）")
            } catch (e: Exception) {
                Log.e(TAG, "document-start 注入注册失败，将退化为 onPageStarted", e)
            }
        } else {
            Log.w(TAG, "当前 WebView 不支持 document-start 注入，退化为 onPageStarted")
        }

        // ---- 检测脚本也走 document-start ----
        //
        // 关键：addDocumentStartJavaScript 会把脚本注入到**每一个框架文档**
        // （frameset 的 <frame>、页面里的 <iframe> 都算）。联运平台几乎全是
        // 框架套娃，SWF 就在子框架里：
        //   669ye: entergame.php → game/game.php → iframe → login_api.php
        //   7k7k : index5.php(frameset) → s1840.sxd.youxi567.com
        // 只在顶层 onPageFinished 注入的话，这些页面永远检测不到 Flash，
        // 表现就是「页面正常但游戏区一片黑」。
        if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT) &&
            injectScript.isNotBlank()
        ) {
            try {
                WebViewCompat.addDocumentStartJavaScript(webView, injectScript, setOf("*"))
                injectStartSupported = true
                Log.i(TAG, "Flash 检测脚本已注册（document-start，全框架）")
            } catch (e: Exception) {
                Log.e(TAG, "检测脚本 document-start 注册失败，退化为仅顶层注入", e)
            }
        }

        webView.webViewClient = object : WebViewClient() {
            override fun onPageStarted(view: WebView?, url: String?, favicon: android.graphics.Bitmap?) {
                super.onPageStarted(view, url, favicon)
                // 页面跳转：旧的内嵌 Flash 立刻停掉，否则会残留一块黑矩形
                // 盖在新页面上（SurfaceView 是独立图层，不受 WebView 重绘影响）。
                closeEmbedded()
                logToFile("开始加载: $url")
                checkFlashLoginLoop(url)
                checkPromoteBounce(url)
                // 登录域兜底升级（部分站内跳转不会走 shouldOverrideUrlLoading）
                if (url != null && url.startsWith("http://")) {
                    val h = Uri.parse(url).host ?: ""
                    if (needHttps(h)) {
                        val secure = "https://" + url.removePrefix("http://")
                        logToFile("登录域升级 HTTPS(onPageStarted): $secure")
                        view?.loadUrl(secure)
                        return
                    }
                }
                // 兜底：若 WebView 不支持 document-start 注入，
                // 则在页面刚开始加载时立刻注入 Flash 伪造脚本。
                if (!documentStartSupported && flashSpoofScript.isNotBlank()) {
                    view?.evaluateJavascript(flashSpoofScript, null)
                }
            }

            override fun shouldOverrideUrlLoading(
                view: WebView?,
                request: WebResourceRequest?
            ): Boolean {
                val u = request?.url?.toString() ?: return false
                Log.i(TAG, "导航请求: $u")
                // 直接指向 .swf 的链接 → 立刻交给原生引擎
                if (u.contains(".swf", ignoreCase = true)) {
                    openWithFlashEngine(u, view?.url ?: u, 0, 0, "")
                    return true
                }
                // ---- 登录域强制 HTTPS ----
                //
                // YY 的登录链是 udblogin.yy.com(http) → lgn.yy.com(https) → callback → 回到
                // login.do。回调页在 https 下写的 Secure Cookie，http 的 login.do 读不到，
                // 于是它以为没登录、再发一个新 token，形成循环，最后弹
                // "requestTokenSecret is missing"。
                // 把入口升级到 https，整条链就同协议了。
                if (u.startsWith("http://")) {
                    val h = Uri.parse(u).host ?: ""
                    if (needHttps(h)) {
                        val secure = "https://" + u.removePrefix("http://")
                        logToFile("登录域升级 HTTPS: $secure")
                        view?.loadUrl(secure)
                        return true
                    }
                }

                // 这里**不做**页面跳转拦截。
                //
                // 曾经为了"防止误触导致重复加载"拦掉所有跳转，结果是灾难性的：
                // 669ye 首页有个 bcastr3.swf 广告被自动内嵌后，embedView 一直非空，
                // 于是"注册 / 登录 / 找回密码"全被拦，用户根本进不去。
                //
                // 现在改成：跳转一律放行，由 onPageStarted 里的 closeEmbedded()
                // 负责收掉旧会话 —— 既不重复加载，也不挡用户的路。
                return false
            }

            override fun onReceivedError(
                view: WebView?,
                request: WebResourceRequest?,
                error: android.webkit.WebResourceError?
            ) {
                super.onReceivedError(view, request, error)
                val code = error?.errorCode ?: -1
                val desc = error?.description?.toString() ?: ""
                Log.e(TAG, "加载失败(code=$code, $desc): ${request?.url}")
                logToFile("加载失败(code=$code, $desc): ${request?.url}")

                // 明文流量被系统拦截是页游最常见的"网页无法打开"原因
                // （net::ERR_CLEARTEXT_NOT_PERMITTED）。WebViewClient 没有该错误
                // 对应的常量可比较，因此用错误描述判断，给出可操作的提示。
                if (desc.contains("CLEARTEXT", ignoreCase = true)) {
                    showStatus("明文 HTTP 被系统拦截：请确认使用包含 cleartext 配置的版本")
                }

                // https 握手失败 → 自动降级到 http（这类页游站很多根本没配证书）。
                // 只对主文档、且每个域名只降一次，避免来回跳。
                val u = request?.url?.toString() ?: ""
                if (request?.isForMainFrame == true && u.startsWith("https://") &&
                    (desc.contains("SSL", ignoreCase = true) ||
                        code == android.webkit.WebViewClient.ERROR_FAILED_SSL_HANDSHAKE)
                ) {
                    val host = request.url.host ?: ""
                    if (host.isNotBlank() && downgradedHosts.add(host)) {
                        val target = "http://" + u.removePrefix("https://")
                        logToFile("HTTPS 握手失败，降级 HTTP: $target")
                        view?.loadUrl(target)
                    }
                }
            }

            override fun onPageFinished(view: WebView?, url: String?) {
                super.onPageFinished(view, url)
                view?.let {
                    urlBar.setText(it.url ?: "")
                    // 校验 Flash 伪造是否生效，便于真机排查
                    it.evaluateJavascript(
                        "(function(){try{var p=navigator.plugins['Shockwave Flash'];" +
                            "return p?('OK:'+p.description):'MISSING';}catch(e){return 'ERR';}})();"
                    ) { r ->
                        Log.i(TAG, "Flash 探测结果 => $r")
                        logToFile("Flash 探测结果 => $r")
                    }
                    logToFile("页面加载完成: ${it.url}")
                    recordHistory(it.url)
                    // 页面里的脚本（登录成功）会用 document.cookie 写票据。
                    // WebView 默认是"攒着慢慢落盘"，进程一崩就全丢 ——
                    // 之前多次闪退后各平台都要重新登录就是这个原因。
                    runCatching { CookieManager.getInstance().flush() }
                    logCookiesIfNeeded(it.url ?: "")
                    applyViewportMode()
                    applyZoom()
                    // document-start 已覆盖所有框架时，这里只做一次补扫
                    // （应对脚本注册失败的老内核）
                    if (!injectStartSupported) injectDetection(it)
                    warnIfFrameShellCold(it)
                }
            }
        }

        webView.webChromeClient = object : WebChromeClient() {
            override fun onProgressChanged(view: WebView?, newProgress: Int) {
                progress.progress = newProgress
                progress.visibility = if (newProgress in 1..99) View.VISIBLE else View.GONE
            }

            override fun onReceivedTitle(view: WebView?, title: String?) {
                supportActionBar?.subtitle = title
            }
        }
    }

    /**
     * 页面是「框架壳」却始终没起播放时，给一句明确提示。
     *
     * frameset 页面本身没有可见内容，检测失败时用户只看到一片黑，
     * 分不清是"没找到游戏"还是"引擎黑屏"——这两者排查方向完全相反。
     */
    private fun warnIfFrameShellCold(view: WebView) {
        val pageUrl = view.url ?: return
        view.postDelayed({
            if (embedView != null) return@postDelayed
            if (view.url != pageUrl) return@postDelayed
            view.evaluateJavascript(
                "(function(){var S=window.__SXDFFLE__;" +
                    "return S&&S.hasFrameShell?S.hasFrameShell():false;})();"
            ) { r ->
                if (r == "true" && embedView == null) {
                    logToFile("框架壳页面未起播放: $pageUrl")
                    showStatus("页面是框架结构但没找到游戏画面 · 点 ☰ → 扫描 重试")
                }
            }
        }, 6000)
    }

    private fun applyUserAgent() {
        val ua = if (desktopMode) {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 " +
                "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        } else {
            "Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36 " +
                "(KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36"
        }
        webView.settings.userAgentString = ua

        // 只改 UA 不够：现在大量站点（奇趣游这类）PC 和手机返回同一份 HTML，
        // 靠 CSS 媒体查询按**视口宽度**决定排版。WebView 默认视口只有几百 CSS px，
        // 于是明明顶着桌面 UA，页面还是排成手机版。
        // PC 模式把视口固定成 1280（相当于桌面浏览器的窗口宽度），
        // 再借助 loadWithOverviewMode 整体缩放塞进屏幕。
        webView.settings.useWideViewPort = true
        webView.settings.loadWithOverviewMode = true
    }

    /**
     * 按当前浏览模式改写页面的 viewport。
     *
     * 必须在页面加载后执行：站点自己的 `<meta name=viewport>` 会覆盖我们，
     * 所以 onPageFinished 里再改一次，媒体查询才会按 1280 重新计算。
     */
    private fun applyViewportMode() {
        if (!desktopMode) return
        webView.evaluateJavascript(
            "(function(){" +
                "var v=document.querySelector('meta[name=viewport]');" +
                "if(!v){v=document.createElement('meta');v.setAttribute('name','viewport');" +
                "(document.head||document.documentElement).appendChild(v);}" +
                "v.setAttribute('content','width=1280');" +
                "})();",
            null
        )
    }

    /** 浏览模式选择：PC / 移动 */
    private fun showBrowseModeDialog() {
        val labels = arrayOf(
            "PC 浏览（按 1280 宽度排版）",
            "移动浏览（按手机屏宽排版）"
        )
        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("浏览模式")
            .setSingleChoiceItems(labels, if (desktopMode) 0 else 1) { dialog, which ->
                dialog.dismiss()
                val next = which == 0
                if (next == desktopMode) {
                    showStatus("已经是" + (if (next) "PC" else "移动") + "浏览")
                    return@setSingleChoiceItems
                }
                desktopMode = next
                saveUiPrefs()
                applyUserAgent()
                webView.reload()
                showStatus(if (desktopMode) "已切换为 PC 浏览" else "已切换为移动浏览")
            }
            .setNegativeButton("取消", null)
            .show()
    }

    /** 注入 Flash 检测脚本 */
    private fun injectDetection(view: WebView) {
        if (injectScript.isBlank()) {
            showStatus("注入脚本缺失（assets/sxdffle_inject.js）")
            return
        }
        view.evaluateJavascript(injectScript, null)
    }

    // ==================================================================
    //  UI 交互
    // ==================================================================

    /** 导航栏按钮在 setupToolbar() 里绑定，这里没有额外 UI 要装 */
    private fun setupUi() {
    }

    /** 安装一次性的崩溃记录器；保留系统默认行为（照常杀进程）。 */
    private fun installCrashLogger() {
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { t, e ->
            try {
                logToFile("崩溃 [${t.name}]: ${Log.getStackTraceString(e)}")
            } catch (_: Throwable) {
                // 记录失败不能再抛，否则会掩盖原始崩溃
            }
            previous?.uncaughtException(t, e)
        }
    }

    /**
     * 显示下拉菜单。
     *
     * 用 PopupWindow 而非系统 PopupMenu：需要自定义深色主题与分组，
     * 并贴住 ☰ 按钮弹出。
     */
    private fun showMenu(anchor: View) {
        val view = layoutInflater.inflate(R.layout.menu_browser, null)
        // 先按调试开关把用不到的项收起来，再量高度 ——
        // 否则量出来的高度里含着隐藏项，底部会空出一截。
        applyDebugVisibility(view)

        // 菜单项越来越多，横屏时整条菜单会超过屏幕高度被裁掉。
        // 先量一下内容，超过可视区 72% 就固定高度，交给里面的 ScrollView 滚动。
        val screenH = resources.displayMetrics.heightPixels
        val maxH = (screenH * 0.72f).toInt()
        view.measure(
            View.MeasureSpec.makeMeasureSpec(0, View.MeasureSpec.UNSPECIFIED),
            View.MeasureSpec.makeMeasureSpec(0, View.MeasureSpec.UNSPECIFIED)
        )
        val menuH = minOf(view.measuredHeight, maxH)

        val popup = PopupWindow(
            view,
            ViewGroup.LayoutParams.WRAP_CONTENT,
            menuH,
            true
        ).apply {
            setBackgroundDrawable(ColorDrawable(0x00000000))
            isOutsideTouchable = true
            elevation = 16f * resources.displayMetrics.density
        }

        fun bind(id: Int, action: () -> Unit) {
            view.findViewById<View>(id).setOnClickListener {
                popup.dismiss()
                action()
            }
        }

        bind(R.id.menuClearCache) {
            // 页游资源缓存（cacheDir/httpcache）。删掉不影响账号与存档，
            // 只是下次进游戏要重新下载 SWF/图集。
            Thread {
                val dir = File(cacheDir, "httpcache")
                var bytes = 0L
                var files = 0
                dir.walkTopDown().forEach {
                    if (it.isFile) {
                        bytes += it.length()
                        if (it.delete()) files++
                    }
                }
                runOnUiThread {
                    showStatus("已清理资源缓存 ${bytes / 1024 / 1024}MB / $files 个文件")
                }
            }.start()
        }

        // 版本号与内核标识：排查时先确认装的是哪个包
        view.findViewById<android.widget.TextView>(R.id.menuVersion)?.text =
            "版本 ${appVersionName()} · 内核 SXDffle-mod"

        bind(R.id.menuCheckUpdate) { checkForUpdate(manual = true) }

        bind(R.id.menuDesktopMode) { showBrowseModeDialog() }
        bind(R.id.menuAudioToggle) {
            audioEnabled = !audioEnabled
            EmbedFlashController.audioEnabled = audioEnabled
            saveUiPrefs()
            showStatus(if (audioEnabled) "音频已开启（下次进入游戏生效）" else "音频已关闭（下次进入游戏生效）")
        }
        bind(R.id.menuExternalToggle) {
            externalEnabled = !externalEnabled
            EmbedFlashController.externalEnabled = externalEnabled
            saveUiPrefs()
            showStatus(
                if (externalEnabled) "ExternalInterface 已开启（下次进入游戏生效）"
                else "ExternalInterface 已关闭（下次进入游戏生效）"
            )
        }
        bind(R.id.menuDebug) {
            debugMode = !debugMode
            saveUiPrefs()
            showStatus(if (debugMode) "调试功能已开启（重开菜单可见）" else "调试功能已关闭")
        }
        bind(R.id.menuEmbedMode) { toggleEmbedMode() }
        bind(R.id.menuStageSize) { showStageSizeDialog() }
        bind(R.id.menuToolbarAlpha) { showToolbarAlphaDialog() }
        bind(R.id.menuImmersive) {
            immersive = !immersive
            saveUiPrefs()
            applyImmersive()
            showStatus(if (immersive) "沉浸模式：开（边缘滑动可临时调出系统栏）" else "沉浸模式：关")
        }
        bind(R.id.menuKeepAlive) {
            keepAlive = !keepAlive
            saveUiPrefs()
            embedView?.keepAlive = keepAlive
            showStatus(if (keepAlive) "切后台将保持游戏连接" else "切后台将断开游戏连接")
        }
        bind(R.id.menuAutoHide) {
            autoHideToolbar = !autoHideToolbar
            saveUiPrefs()
            if (!autoHideToolbar) setToolbarExpanded(true) else scheduleHideToolbar()
            showStatus(if (autoHideToolbar) "顶部栏 3 秒后自动收起（下拉唤出）" else "顶部栏常驻显示")
        }
        bind(R.id.menuFullscreen) {
            if (embedView != null) {
                // 从内嵌切全屏：参数用当前会话的
                openWithFlashEngine(embedSwfUrl, embedPageUrl, embedRectW, embedRectH, embedVars)
            } else {
                webView.evaluateJavascript(
                    "window.__SXDFFLE__ && window.__SXDFFLE__.openFirst();", null
                )
            }
        }
        bind(R.id.menuEmbedZoomIn) { withEmbed("放大") { it.zoomIn() } }
        bind(R.id.menuEmbedZoomOut) { withEmbed("缩小") { it.zoomOut() } }
        bind(R.id.menuEmbedRotate) { withEmbed("旋转") { it.rotate() } }
        bind(R.id.menuEmbedReset) { withEmbed("复位") { it.resetTransform() } }
        bind(R.id.menuEmbedClose) {
            if (embedView == null) showStatus("当前没有内嵌播放")
            else { closeEmbedded(); showStatus("内嵌播放已关闭") }
        }
        bind(R.id.menuCopyLogin) { copyLoginState() }
        bind(R.id.menuShowToolbar) { setToolbarExpanded(true) }
        bind(R.id.menuScanFlash) { scanForFlash() }
        bind(R.id.menuHome) { openHome() }
        bind(R.id.menuSetHome) { setCurrentAsHome() }
        bind(R.id.menuBookmark) { toggleBookmark() }
        bind(R.id.menuBookmarks) { showBookmarks() }
        bind(R.id.menuExportLog) { exportLog() }

        view.findViewById<TextView>(R.id.menuStageSize).text =
            "渲染分辨率：${stageSizeLabel()}"
        view.findViewById<TextView>(R.id.menuToolbarAlpha).text =
            "导航栏不透明度：" + toolbarAlphaPercent + "%"
        view.findViewById<TextView>(R.id.menuImmersive).text =
            "沉浸模式（隐藏状态栏）：" + (if (immersive) "开" else "关")
        view.findViewById<TextView>(R.id.menuKeepAlive).text =
            "保持后台运行：" + (if (keepAlive) "开" else "关")
        view.findViewById<TextView>(R.id.menuAutoHide).text =
            "顶部栏自动收起：" + (if (autoHideToolbar) "开" else "关")
        view.findViewById<TextView>(R.id.menuDesktopMode).text =
            "浏览模式：" + (if (desktopMode) "PC" else "移动")
        view.findViewById<TextView>(R.id.menuDebug).text =
            "调试功能：" + (if (debugMode) "开" else "关")
        view.findViewById<TextView>(R.id.menuAudioToggle).text =
            "音频：" + (if (audioEnabled) "开" else "关")
        view.findViewById<TextView>(R.id.menuExternalToggle).text =
            "ExternalInterface：" + (if (externalEnabled) "开" else "关")

        // 菜单宽 236dp，让右边缘与屏幕右缘对齐（☰ 在最右侧）
        val menuWidthPx = (236 * resources.displayMetrics.density).toInt()
        val offsetX = -(menuWidthPx - anchor.width)

        if (anchor.rootView === window.decorView) {
            popup.showAsDropDown(anchor, offsetX, 4)
        } else {
            // anchor 在内嵌悬浮栏（另一个 Window）里，showAsDropDown 的坐标系会错，
            // 改用屏幕绝对坐标手动定位。
            val loc = IntArray(2)
            anchor.getLocationOnScreen(loc)
            val screenW = resources.displayMetrics.widthPixels
            val x = (loc[0] + offsetX).coerceIn(0, (screenW - menuWidthPx).coerceAtLeast(0))
            val y = loc[1] + anchor.height + 4
            popup.showAtLocation(
                window.decorView,
                Gravity.TOP or Gravity.START,
                x,
                y
            )
        }
    }

    /**
     * 调试开关关闭时，把这些底层旋钮从菜单里藏起来。
     *
     * 内嵌播放已经打磨到用户不需要手动缩放/旋转；这些入口留着只会增加误操作。
     * 只有需要排查问题时才打开「调试功能」把它们放出来。
     */
    private fun applyDebugVisibility(menu: View) {
        val ids = intArrayOf(
            R.id.menuEmbedMode,
            R.id.menuFullscreen,
            R.id.menuEmbedZoomIn,
            R.id.menuEmbedZoomOut,
            R.id.menuEmbedReset,
            R.id.menuEmbedRotate
        )
        for (id in ids) {
            menu.findViewById<View>(id).visibility = if (debugMode) View.VISIBLE else View.GONE
        }
    }

    private fun navigateFromBar() {
        val input = urlBar.text.toString().trim()
        if (input.isEmpty()) return
        loadSmart(input)
        hideKeyboard()
    }

    private fun hideKeyboard() {
        val imm = getSystemService(INPUT_METHOD_SERVICE) as? android.view.inputmethod.InputMethodManager
        imm?.hideSoftInputFromWindow(urlBar.windowToken, 0)
        urlBar.clearFocus()
    }

    // ==================================================================
    //  缩放 / 显示
    // ==================================================================

    private fun changeZoom(delta: Int) = setZoom(zoomPercent + delta)

    private fun setZoom(percent: Int) {
        zoomPercent = percent.coerceIn(25, 500)
        applyZoom()
        showStatus("缩放：$zoomPercent%")
    }

    /** 用 CSS zoom 缩放整个网页，跨 WebView 版本最稳定 */
    private fun applyZoom() {
        val scale = zoomPercent / 100.0
        webView.evaluateJavascript(
            "(function(){document.documentElement.style.zoom='$scale';" +
                "if(window.__SXDFFLE__&&window.__SXDFFLE__.rescan)window.__SXDFFLE__.rescan();})();",
            null
        )
    }

    /** 网页自适应屏幕宽度 */
    private fun fitPage() {
        zoomPercent = 100
        webView.evaluateJavascript(
            "(function(){document.documentElement.style.zoom='1';" +
                "var v=document.querySelector('meta[name=viewport]');" +
                "if(!v){v=document.createElement('meta');v.setAttribute('name','viewport');" +
                "document.head.appendChild(v);}" +
                "v.setAttribute('content','width=device-width,initial-scale=1.0');" +
                "if(window.__SXDFFLE__&&window.__SXDFFLE__.rescan)window.__SXDFFLE__.rescan();})();",
            null
        )
        webView.settings.loadWithOverviewMode = true
        webView.settings.useWideViewPort = true
        showStatus("已切换为网页自适应")
    }

    private fun scanForFlash() {
        // 除了重扫本页，还主动尝试一次「入口提升」：
        // 有些平台的入口是延迟脚本插入的框架，自动检测那一刻还没出现，
        // 用户手动点一下「扫描」就能把藏在框架里的游戏页揪出来。
        webView.evaluateJavascript(
            "(function(){" +
                "var S=window.__SXDFFLE__;" +
                "var n=S&&S.rescan?S.rescan():0;" +
                "if(n===0&&S&&S.promoteEntry){try{S.promoteEntry();}catch(e){}}" +
                "return n;})();"
        ) { result ->
            val n = result?.trim('"')?.toIntOrNull() ?: 0
            if (n > 0) {
                showStatus("发现 $n 处 Flash 内容，可点击绿色按钮进入引擎播放")
            } else {
                Toast.makeText(this, "本页未直接检测到 Flash，正在尝试进入框架内的游戏", Toast.LENGTH_SHORT).show()
            }
        }
    }

    // ==================================================================
    //  Flash 桥接
    // ==================================================================

    inner class FlashBridge {
        /** 页面上报检测到的 Flash，用于顶部提示 */
        @JavascriptInterface
        fun report(url: String, pageUrl: String, count: Int) {
            // 顶部提示条已移除：它白占一行屏幕，而自动内嵌已经不需要用户确认。
            // 这里只记录日志，便于排查检测是否命中。
            Log.i(TAG, "检测到 Flash（$count 处）：$url")
            logToFile("检测到 Flash（$count 处）：$url")
        }

        /** 用户点击遮罩 → 交给原生引擎（含页面 flashVars） */
        @JavascriptInterface
        fun open(url: String, pageUrl: String, w: Int, h: Int, flashVars: String) {
            runOnUiThread { openWithFlashEngine(url, pageUrl, w, h, flashVars) }
        }

        /**
         * 网页内嵌播放（实验性）。
         *
         * 与 [open] 的区别：不启动 Activity，而是把 Flash 画面作为原生 View
         * 覆盖在网页中该元素的位置上，浏览器 UI 完全保留。
         *
         * [inFrame] = 1 表示这次上报来自子框架（frameset / iframe）：
         * 坐标与尺寸都是相对子框架的，直接拿来定位必然错位，
         * 因此原生侧一律铺满可视区。
         */
        @JavascriptInterface
        fun openEmbed(
            url: String,
            pageUrl: String,
            x: Int,
            y: Int,
            w: Int,
            h: Int,
            flashVars: String,
            inFrame: Int
        ) {
            runOnUiThread { openFlashContent(url, pageUrl, x, y, w, h, flashVars, inFrame == 1) }
        }

        /**
         * 把框架里的游戏入口提升为顶层页面。
         *
         * 由注入脚本在三种页面上调用：自动提交表单 / frameset / 只剩一个大
         * iframe 的框架壳。详情见 assets/sxdffle_inject.js 的 promoteEntry。
         */
        @JavascriptInterface
        fun promoteEntry(url: String, reason: String) {
            runOnUiThread { promoteToTop(url, reason) }
        }

        /** 页面滚动/缩放后，同步 Flash 元素在视口中的位置 */
        @JavascriptInterface
        fun embedRect(x: Int, y: Int, w: Int, h: Int) {
            runOnUiThread { updateEmbedRect(x, y, w, h) }
        }

        /**
         * 网页 JS → SWF 的 ExternalInterface 回调。
         *
         * 页面通过 `<embed>` 元素调用 SWF 注册的方法时，注入脚本会把它转发到这里；
         * 目标是没有真实插件时 DOM 元素上不存在的那些函数（如 function_gameW7）。
         */
        @JavascriptInterface
        fun externalCall(name: String, argsJson: String): String {
            // 注意：这里**同步**返回。调用线程是 WebView 的 JavaBridge 线程，
            // 不是 UI 线程，所以阻塞等待引擎结果是安全的（原生侧最多 300ms）。
            return embedView?.externalCall(name, argsJson) ?: ""
        }

        /**
         * 注入脚本的调试输出。
         *
         * 页面里的判断（有没有 frameset、选中哪个 frame、为什么没触发提升）
         * 在原生侧完全看不到，黑屏时无从判断卡在哪一步，所以给它一条通道。
         */
        @JavascriptInterface
        fun debug(msg: String) {
            Log.i(TAG, "[JS] $msg")
            logToFile("[JS] $msg")
        }
    }

    // ==================================================================
    //  网页内嵌播放（实验性分支 experiment/embedded-loader）
    // ==================================================================

    /**
     * 按当前显示模式分派：网页内嵌 或 全屏播放。
     *
     * 内嵌启动失败（例如 Surface 创建不出来）会自动兜底到全屏，
     * 避免用户因为实验性功能而完全卡住。
     */
    private fun openFlashContent(
        swfUrl: String,
        pageUrl: String,
        x: Int,
        y: Int,
        w: Int,
        h: Int,
        flashVars: String,
        inFrame: Boolean = false
    ) {
        if (!embedMode) {
            openWithFlashEngine(swfUrl, pageUrl, w, h, flashVars)
            return
        }
        // 子框架上报的左上角是相对那个框架的，直接用会错位 → 从 (0,0) 起；
        // 宽高仍用框架自身的 CSS 尺寸（换算比才正确，画面不会被缩小）。
        val fx = if (inFrame) 0 else x
        val fy = if (inFrame) 0 else y
        // 注意：必须在 runEmbedded 之后赋值。
        // runEmbedded 内部第一步就 closeEmbedded()，那里会把 embedFromFrame 复位；
        // 先赋值会被它清掉，后续旋转就会误判成「网页原位嵌入」，
        // 拿子框架的相对坐标去改容器 —— 画面越转越小就是这么来的。
        val ok = runEmbedded(swfUrl, pageUrl, fx, fy, w, h, flashVars, fill = inFrame)
        embedFromFrame = inFrame && ok
        logToFile("内嵌来源: inFrame=$inFrame 铺满=$inFrame ok=$ok")
        if (!ok) {
            Log.w(TAG, "内嵌播放启动失败，回退到全屏")
            openWithFlashEngine(swfUrl, pageUrl, w, h, flashVars)
        }
    }

    /**
     * 把框架套娃里的游戏入口提升为顶层页面。
     *
     * 只做三件事：判重、记录、导航。不做任何"看起来像游戏就跳"的启发式，
     * 入口的判定完全交给注入脚本（只认自动提交表单 / frameset / 框架壳）。
     */
    private fun promoteToTop(url: String, reason: String) {
        if (url.isBlank()) return
        val cur = webView.url ?: ""
        if (url == cur) return
        if (!url.startsWith("http://") && !url.startsWith("https://")) return

        // 限流：20 秒内最多提升 5 次。
        // 站点一旦出现"外层→内层→外层"的乒乓，每次 URL 的 t= 都不同，
        // 单靠 URL 去重拦不住，这里从整体频率上兜底。
        val now = android.os.SystemClock.elapsedRealtime()
        while (promoteTimes.isNotEmpty() && now - promoteTimes.first() > 20_000) {
            promoteTimes.removeFirst()
        }
        if (promoteTimes.size >= 5) {
            if (!promotePaused) {
                promotePaused = true
                logToFile("提升过于频繁，暂停 20 秒（防乒乓）: $url")
            }
            return
        }
        if (promotePaused) promotePaused = false

        val targetHost = runCatching { Uri.parse(url).host ?: "" }.getOrDefault("")
        if (targetHost in promoteBannedHosts) {
            logToFile("入口已回弹过，跳过提升: $url")
            return
        }
        promoteTimes.addLast(now)

        // 去重：同一个地址只提升一次，避免「弹回首页 → 再提升」的死循环
        if (promotedUrls.size > 32) promotedUrls.clear()
        if (!promotedUrls.add(url)) {
            Log.i(TAG, "入口已提升过，跳过: $url")
            return
        }
        logToFile("提升游戏入口($reason): $url")
        showStatus("进入游戏…")
        promoteFromUrl = cur
        promoteTargetUrl = url
        promoteAtMs = now
        webView.loadUrl(url)
    }

    /**
     * 把网页坐标（CSS 像素）换算成 View 坐标（物理像素）。
     *
     * 这一步是「背景错位」的根因：WebView 开了 `useWideViewPort` +
     * `loadWithOverviewMode`，页面按 980 CSS px 布局后整体缩放显示。
     * `getBoundingClientRect()` 给的是 **CSS 像素**，而 View 布局用 **物理像素**，
     * 两者相差 `webView.scale` 倍。不换算就会偏小、偏左上。
     */
    private fun cssToView(v: Int): Int {
        val s = webView.scale
        val factor = if (s > 0.01f) s else 1f
        return (v * factor).toInt()
    }

    /**
     * 在网页原位启动内嵌播放。
     *
     * 内嵌视图是覆盖在 WebView 之上的原生 `SurfaceView`，位置与尺寸对齐网页里的
     * Flash 元素；引擎 viewport 就是该视图尺寸，因此**网页布局完全不被改动**，
     * 这与全屏版最大的区别也在于此。
     */
    private fun runEmbedded(
        swfUrl: String,
        pageUrl: String,
        x: Int,
        y: Int,
        w: Int,
        h: Int,
        flashVars: String,
        fill: Boolean = false
    ): Boolean {
        val content = findViewById<View>(R.id.contentArea)
        val container = findViewById<FrameLayout>(R.id.flashContainer)

        val maxW = if (content.width > 0) content.width else resources.displayMetrics.widthPixels
        // 可用高度按「工具栏收起后的全屏」算：
        // contentArea 的 marginTop 就是工具栏当前占的高度，加回来才是真实可用空间。
        // 这样容器尺寸一次算定，之后工具栏展开只会让画面下移（底部被裁一点），
        // 收起时正好铺满整屏 —— 不会出现「顶部深灰」或「底部深灰」。
        val insetTop = (content.layoutParams as? ViewGroup.MarginLayoutParams)?.topMargin ?: 0
        val maxH = if (content.height > 0) content.height + insetTop
        else resources.displayMetrics.heightPixels
        if (maxW <= 0 || maxH <= 0) return false

        closeEmbedded()

        // JS 上报的是 CSS 像素，这里统一换算成物理像素
        val px = cssToView(x).coerceAtLeast(0)
        val py = cssToView(y).coerceAtLeast(0)
        val pw = cssToView(w)
        val ph = cssToView(h)

        // fill = 子框架上报（frameset / iframe）。
        //
        // 子框架的 innerWidth/innerHeight 不能当整页尺寸用：框架页自身可能被
        // WebView 缩放过（实测有一版报回 1080 CSS，乘 0.72 变成 778 物理），
        // 于是容器只占了屏幕左半边、引擎又被等比缩进这个小框 ——
        // 用户看到的就是「框里有框、右边露出网页白底」。
        // 子框架里的 Flash 实际上就是整个游戏画面，直接铺满内容区。
        val wantW = if (fill) maxW else if (pw > 0) pw else maxW
        val wantH = if (fill) maxH else if (ph > 0) ph else wantW * 3 / 4
        // 页游画面常比手机屏宽，等比缩进可视区，否则只能看到左上角
        val fit = if (fill) 1f else minOf(1f, maxW.toFloat() / wantW, maxH.toFloat() / wantH)
        val vw = (wantW * fit).toInt().coerceAtLeast(64)
        val vh = (wantH * fit).toInt().coerceAtLeast(64)

        // 位置对齐网页中 Flash 元素的左上角；页面滚动后由 embedRect 持续同步
        container.layoutParams = FrameLayout.LayoutParams(vw, vh).apply {
            gravity = Gravity.TOP or Gravity.START
            leftMargin = if (fill) 0 else px
            topMargin = if (fill) 0 else py
        }
        container.removeAllViews()
        container.visibility = View.VISIBLE

        val referer = if (pageUrl.isNotBlank()) pageUrl else swfUrl
        val cookie = try {
            CookieManager.getInstance().getCookie(referer) ?: ""
        } catch (e: Exception) {
            ""
        }

        Log.i(TAG, "内嵌播放: ${vw}x${vh}（原始 ${wantW}x${wantH}）swf=$swfUrl")
        logToFile("内嵌播放: ${vw}x${vh} swf=$swfUrl")
        logMemory("内嵌启动")

        embedSwfUrl = swfUrl
        embedPageUrl = pageUrl
        embedRectW = if (fill) vw else w
        embedRectH = if (fill) vh else h
        embedVars = flashVars
        val view = EmbedFlashView(this).apply {
            onStatus = { msg -> this@BrowserActivity.logToFile("内嵌状态: $msg") }
            // 关闭「保持后台运行」时，切回来不自动重连，只提示一下
            onDisconnected = { runOnUiThread { onConnectionLost() } }
            locked = this@BrowserActivity.locked
            keepAlive = this@BrowserActivity.keepAlive
        }
        view.start(
            EmbedFlashView.Params(
                swfUrl = swfUrl,
                referer = referer,
                cookie = cookie,
                flashVars = flashVars,
                storageDir = filesDir.absolutePath,
                // 引擎 side 的 player trace 关闭（那是 SWF 的 avm trace，
                // 与排查日志不是一回事）；全局 log 由 logFile 承接
                traceOutput = null,
                logFile = logFile().absolutePath,
                // 关键：物理/CSS 的换算比，不是屏幕 density。
                // 传 density(2.75) 会让引擎逻辑视口只有 393，游戏按 393 布局再铺到
                // 1080 上 → 画面明显偏小。这里用实测比例 ≈1.1。
                // 自定义分辨率时引擎走 NoScale，物理=逻辑，比例必须是 1.0
                viewportScale = when {
                    stageW > 0 && stageH > 0 -> 1.0f
                    // 铺满模式：物理尺寸取自屏幕，换算比只能用页面当前的缩放比
                    else -> if (fill) {
                        webView.scale.takeIf { it > 0.1f } ?: 1f
                    } else if (w > 0) {
                        pw.toFloat() / w
                    } else {
                        resources.displayMetrics.density
                    }
                },
                stageWidth = stageW,
                stageHeight = stageH
            )
        )
        container.addView(
            view,
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT
            )
        )
        embedView = view
        enterEmbedUi()
        showStatus("Flash 已内嵌播放 ${vw}×${vh}（可缩放 / 旋转）")
        scheduleHideToolbar()
        return true
    }

    /**
     * 连接已断开。
     *
     * 不做任何遮挡或自动恢复：切回前台**不会**自动重连（避免顶号/重复登录），
     * 画面就停在原处。用户想继续玩，点工具栏 ⟳ 刷新即可重新走一遍内嵌流程。
     */
    private fun onConnectionLost() {
        Log.i(TAG, "连接已断开（不自动重连）")
        logToFile("连接已断开（不自动重连）")
        showStatus("连接已断开 · 点 ⟳ 刷新可重连")
    }

    /** 停止内嵌播放并清空容器 */
    private fun closeEmbedded() {
        embedView?.shutdown()
        embedView = null
        embedFromFrame = false
        val container = findViewById<FrameLayout>(R.id.flashContainer)
        container.removeAllViews()
        container.visibility = View.GONE
        logMemory("内嵌关闭")
        exitEmbedUi()
    }

    // ==================================================================
    //  沉浸模式 / 导航栏（独立窗口）
    // ==================================================================

    /**
     * 沉浸模式：隐藏状态栏与导航栏，画面独占整屏（默认开启）。
     *
     * `BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE`：从边缘滑动时临时浮出系统栏，
     * 几秒后自动隐藏 —— 既能看时间电量，又不常驻占位。
     */
    private fun applyImmersive() {
        val controller = androidx.core.view.WindowInsetsControllerCompat(window, window.decorView)
        if (immersive) {
            controller.systemBarsBehavior =
                androidx.core.view.WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            controller.hide(androidx.core.view.WindowInsetsCompat.Type.systemBars())
            androidx.core.view.WindowCompat.setDecorFitsSystemWindows(window, false)
        } else {
            controller.show(androidx.core.view.WindowInsetsCompat.Type.systemBars())
            androidx.core.view.WindowCompat.setDecorFitsSystemWindows(window, true)
        }
        updateStatusBarInset()
    }

    /** 状态栏高度：沉浸模式下为 0，否则要把这段让给系统 */
    private fun updateStatusBarInset() {
        val id = resources.getIdentifier("status_bar_height", "dimen", "android")
        val h = if (id > 0) resources.getDimensionPixelSize(id) else 0
        statusBarInset = if (immersive) 0 else h

        // 导航栏自身也要避开状态栏
        toolbarView.setPadding(0, statusBarInset, 0, 0)
        toolbarBar.layoutParams = toolbarBar.layoutParams.also {
            it.height = (52 * resources.displayMetrics.density).toInt()
        }
        updateContentInset(!toolbarCollapsed, animate = false)
        // 正在显示时重排一下位置
        if (toolbarPopup?.isShowing == true) {
            try {
                toolbarPopup?.update(0, statusBarInset, -1, -1)
            } catch (_: Throwable) {
            }
        }
    }

    /** 导航栏（含地址栏那一条）的总高度 */
    private fun toolbarHeight(): Int =
        (52 * resources.displayMetrics.density).toInt() + statusBarInset

    /** 创建导航栏 PopupWindow 并绑定按钮 */
    private fun setupToolbar() {
        toolbarView = layoutInflater.inflate(R.layout.toolbar_main, null)
        toolbarBar = toolbarView.findViewById(R.id.toolbarBar)
        urlBar = toolbarView.findViewById(R.id.urlBar)

        toolbarView.findViewById<View>(R.id.btnBack).setOnClickListener {
            if (webView.canGoBack()) webView.goBack()
        }
        toolbarView.findViewById<View>(R.id.btnForward).setOnClickListener {
            if (webView.canGoForward()) webView.goForward()
        }
        toolbarView.findViewById<View>(R.id.btnReload).setOnClickListener { webView.reload() }
        toolbarView.findViewById<View>(R.id.btnGo).setOnClickListener { navigateFromBar() }
        toolbarView.findViewById<View>(R.id.btnMenu).setOnClickListener { showMenu(it) }
        toolbarView.findViewById<View>(R.id.btnLock).setOnClickListener { toggleLock() }

        // 地址栏只当按钮用：点击弹出输入框。
        //
        // 不直接在这个 PopupWindow 里的 EditText 上打字，是因为
        // 非 Activity 窗口要弹输入法必须临时把 popup 设成 focusable，
        // 实测在部分机型上输入法仍然不弹出。改成对话框输入 100% 可靠。
        urlBar.isFocusable = false
        urlBar.isCursorVisible = false
        urlBar.setOnClickListener { showUrlDialog() }

        toolbarPopup = PopupWindow(
            toolbarView,
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply {
            // 默认不可获焦：不拦返回键、不抢 WebView 的输入焦点
            isFocusable = false
            isOutsideTouchable = false
            setBackgroundDrawable(ColorDrawable(android.graphics.Color.TRANSPARENT))
            elevation = 12f * resources.displayMetrics.density
        }

        applyToolbarAlpha()
    }

    /** 弹出网址输入框（在独立窗口里用对话框，输入法百分之百能弹出来） */
    private fun showUrlDialog() {
        val input = EditText(this).apply {
            setText(urlBar.text?.toString() ?: "")
            setSelection(text.length)
            inputType = android.text.InputType.TYPE_TEXT_VARIATION_URI
            imeOptions = EditorInfo.IME_ACTION_GO
            setSingleLine()
            setPadding(36, 28, 36, 28)
        }

        val container = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.VERTICAL
            setPadding(dp(14), dp(6), dp(14), 0)
        }
        container.addView(
            input,
            android.widget.LinearLayout.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT,
                android.view.ViewGroup.LayoutParams.WRAP_CONTENT
            )
        )

        // ---- 最近访问 ----
        // 点地址栏时顺手把历史摆出来，省得每次手打长域名。
        val history = loadEntries(KEY_HISTORY).take(8)
        lateinit var dialog: androidx.appcompat.app.AlertDialog

        if (history.isNotEmpty()) {
            container.addView(TextView(this).apply {
                text = "最近访问（长按可删除单条）"
                setTextColor(0xFF8A8A8A.toInt())
                textSize = 12f
                setPadding(4, dp(12), 0, dp(4))
            })

            val lv = android.widget.ListView(this)
            lv.adapter = android.widget.ArrayAdapter(
                this,
                android.R.layout.simple_list_item_1,
                history.map { (u, t) -> if (t.isBlank()) u else "$t\n$u" }
            )
            lv.layoutParams = android.widget.LinearLayout.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT, dp(220)
            )
            lv.dividerHeight = dp(1)
            container.addView(lv)

            lv.setOnItemClickListener { _, _, pos, _ ->
                dialog.dismiss()
                loadSmart(history[pos].first)
            }
            lv.setOnItemLongClickListener { _, _, pos, _ ->
                val list = loadEntries(KEY_HISTORY)
                list.removeAll { it.first == history[pos].first }
                saveEntries(KEY_HISTORY, list)
                showStatus("已从历史中删除")
                true
            }
        }

        dialog = androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("打开网址")
            .setView(container)
            .setPositiveButton("打开") { _, _ ->
                loadSmart(input.text.toString().trim())
                hideKeyboard()
            }
            .setNegativeButton("取消", null)
            .setNeutralButton("粘贴") { _, _ ->
                val cm = getSystemService(CLIPBOARD_SERVICE) as android.content.ClipboardManager
                val t = cm.primaryClip?.getItemAt(0)?.text?.toString()
                if (!t.isNullOrBlank()) {
                    input.setText(t)
                    input.setSelection(input.text.length)
                }
            }
            .create()

        // 让对话框一出现就聚焦并弹出输入法
        dialog.setOnShowListener {
            input.requestFocus()
            val imm = getSystemService(INPUT_METHOD_SERVICE) as android.view.inputmethod.InputMethodManager
            imm.showSoftInput(input, android.view.inputmethod.InputMethodManager.SHOW_IMPLICIT)
        }
        dialog.show()
    }

    /** 把透明度应用到导航栏背景 */
    private fun applyToolbarAlpha() {
        // 底色固定 #1F1F1F，只改 alpha 通道
        val a = toolbarAlphaPercent * 255 / 100
        toolbarBar.setBackgroundColor(android.graphics.Color.argb(a, 0x1F, 0x1F, 0x1F))
        Log.i(TAG, "导航栏不透明度=${toolbarAlphaPercent}%")
    }

    /** 选择导航栏不透明度 */
    private fun showToolbarAlphaDialog() {
        val percents = intArrayOf(100, 90, 75, 60, 40, 25)
        val labels = percents.map { "$it%" }.toTypedArray()
        // 找最接近当前值的档位
        val checked = percents.indices.minByOrNull {
            kotlin.math.abs(percents[it] - toolbarAlphaPercent)
        } ?: 0

        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("导航栏不透明度")
            .setSingleChoiceItems(labels, checked) { dialog, which ->
                dialog.dismiss()
                toolbarAlphaPercent = percents[which]
                applyToolbarAlpha()
                saveUiPrefs()
                showStatus("导航栏不透明度：${percents[which]}%")
            }
            .setNegativeButton("取消", null)
            .show()
    }

    /** 懒创建悬浮球 */
    private fun ensureBall(): View {
        ballView?.let { return it }
        val v = layoutInflater.inflate(R.layout.menu_ball, null)
        v.findViewById<View>(R.id.ballBtn).setOnTouchListener { view, e ->
            handleBallTouch(view, e)
        }
        ballPopup = PopupWindow(
            v,
            ViewGroup.LayoutParams.WRAP_CONTENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply {
            isFocusable = false
            isOutsideTouchable = false
            setBackgroundDrawable(ColorDrawable(android.graphics.Color.TRANSPARENT))
            elevation = 10f * resources.displayMetrics.density
        }
        ballView = v
        return v
    }

    /** 导航栏收起 → 右上角出现悬浮球 */
    private fun showBall() {
        val v = ensureBall()
        v.animate().cancel()
        v.scaleX = 1f
        v.scaleY = 1f
        v.translationX = 0f
        v.translationY = 0f

        val p = ballPopup ?: return
        if (p.isShowing) return
        resolveBallPos()
        try {
            p.showAtLocation(window.decorView, Gravity.TOP or Gravity.START, ballX, ballY)
        } catch (_: Throwable) {
        }
        v.alpha = 0f
        v.animate().alpha(1f).setDuration(140).start()
    }

    private fun ballSizePx(): Int = ((52 + 12) * resources.displayMetrics.density).toInt()

    /** 计算球的落点：没摆过就默认右上角；摆过就沿用上次位置并夹进屏幕 */
    private fun resolveBallPos() {
        val dm = resources.displayMetrics
        val size = ballSizePx()
        val margin = (8 * dm.density).toInt()
        if (ballX < 0 || ballY < 0) {
            ballX = dm.widthPixels - size - margin
            ballY = statusBarInset + margin
        }
        ballX = ballX.coerceIn(0, (dm.widthPixels - size).coerceAtLeast(0))
        ballY = ballY.coerceIn(statusBarInset, maxBallY())
    }

    private fun maxBallY(): Int {
        val dm = resources.displayMetrics
        return (dm.heightPixels - ballSizePx()).coerceAtLeast(statusBarInset)
    }

    /**
     * 悬浮球触摸：拖动跟手，松手吸附到左右边缘；没拖动则视为点击弹菜单。
     */
    private fun handleBallTouch(view: View, e: MotionEvent): Boolean {
        if (ballTouchSlop == 0f) {
            ballTouchSlop = android.view.ViewConfiguration.get(this).scaledTouchSlop.toFloat()
        }
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                ballDownRawX = e.rawX
                ballDownRawY = e.rawY
                ballStartX = ballX
                ballStartY = ballY
                ballDragging = false
                view.animate().cancel()
                view.alpha = 1f
                return true
            }

            MotionEvent.ACTION_MOVE -> {
                val dx = e.rawX - ballDownRawX
                val dy = e.rawY - ballDownRawY
                if (!ballDragging &&
                    (kotlin.math.abs(dx) > ballTouchSlop || kotlin.math.abs(dy) > ballTouchSlop)
                ) {
                    ballDragging = true
                }
                if (ballDragging) {
                    val dm = resources.displayMetrics
                    ballX = (ballStartX + dx).toInt()
                        .coerceIn(0, (dm.widthPixels - ballSizePx()).coerceAtLeast(0))
                    ballY = (ballStartY + dy).toInt()
                        .coerceIn(statusBarInset, maxBallY())
                    try {
                        ballPopup?.update(ballX, ballY, -1, -1)
                    } catch (_: Throwable) {
                    }
                }
                return true
            }

            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (ballDragging) {
                    snapBallToEdge()
                    saveBallPos()
                } else {
                    showMenu(view)
                }
                return true
            }
        }
        return false
    }

    /** 松手后吸附到最近的左右边缘（垂直位置保留） */
    private fun snapBallToEdge() {
        val dm = resources.displayMetrics
        val size = ballSizePx()
        val margin = (8 * dm.density).toInt()
        val left = margin
        val right = (dm.widthPixels - size - margin).coerceAtLeast(left)
        val targetX = if (ballX + size / 2 < dm.widthPixels / 2) left else right
        animateBallTo(targetX, ballY)
    }

    /** PopupWindow 没有属性动画，只能逐帧 update 位置 */
    private fun animateBallTo(tx: Int, ty: Int) {
        val sx = ballX
        val sy = ballY
        android.animation.ValueAnimator.ofFloat(0f, 1f).apply {
            duration = 160
            addUpdateListener { va ->
                val t = va.animatedValue as Float
                ballX = (sx + (tx - sx) * t).toInt()
                ballY = (sy + (ty - sy) * t).toInt()
                try {
                    ballPopup?.update(ballX, ballY, -1, -1)
                } catch (_: Throwable) {
                }
            }
            start()
        }
    }

    private fun saveBallPos() {
        getSharedPreferences("sxdffle", MODE_PRIVATE).edit()
            .putInt("ball_x", ballX)
            .putInt("ball_y", ballY)
            .apply()
    }

    /** 导航栏展开 → 悬浮球缩小淡出（"收进导航栏"的观感） */
    private fun hideBall(animate: Boolean = true) {
        val v = ballView ?: return
        val p = ballPopup ?: return
        if (!p.isShowing) return
        v.animate().cancel()
        if (!animate) {
            try {
                p.dismiss()
            } catch (_: Throwable) {
            }
            return
        }
        // 只做淡出，不做位移/缩放 —— 拖拽过之后"飞回 ☰"反而会显得莫名其妙
        v.animate()
            .alpha(0f)
            .setDuration(140)
            .withEndAction {
                try {
                    p.dismiss()
                } catch (_: Throwable) {
                }
            }
            .start()
    }

    /** 显示导航栏（悬浮在内容之上，不占布局） */
    private fun showToolbar() {
        toolbarCollapsed = false
        val p = toolbarPopup ?: return
        if (!p.isShowing) {
            try {
                p.showAtLocation(
                    window.decorView,
                    Gravity.TOP or Gravity.START,
                    0,
                    statusBarInset
                )
            } catch (_: Throwable) {
                // Activity 已销毁等边界情况
            }
        }
        updateContentInset(true)
        hideBall(animate = true)
        if (autoHideToolbar) scheduleHideToolbar() else toolbarView.removeCallbacks(hideToolbarRunnable)
    }

    /** 收起导航栏（内容区回到全屏） */
    private fun hideToolbar() {
        toolbarCollapsed = true
        toolbarView.removeCallbacks(hideToolbarRunnable)
        try {
            toolbarPopup?.dismiss()
        } catch (_: Throwable) {
        }
        updateContentInset(false)
        showBall()
    }

    /**
     * 进入内嵌播放的界面状态。
     *
     * 内容区固定全屏（inset = 0）：画面从第一帧起就铺满整屏，
     * 导航栏以悬浮窗形式浮在它上面，显隐都不再推动画面。
     */
    private fun enterEmbedUi() {
        embedUiActive = true
        updateContentInset(false, animate = false)
        showToolbar()
    }

    /** 退出内嵌播放：回到网页，导航栏继续悬浮 */
    private fun exitEmbedUi() {
        if (!embedUiActive) return
        embedUiActive = false
        updateContentInset(!toolbarCollapsed, animate = false)
    }

    // ==================================================================
    //  画面锁定 / 顶部栏自动收起 / 后台保持
    // ==================================================================

    private fun loadUiPrefs() {
        val sp = getSharedPreferences("sxdffle", MODE_PRIVATE)
        keepAlive = sp.getBoolean("keep_alive", false)
        autoHideToolbar = sp.getBoolean("auto_hide_toolbar", false)
        immersive = sp.getBoolean("immersive", true)   // 默认开启
        toolbarAlphaPercent = sp.getInt("toolbar_alpha_pct", 100)
        currentPlatform = sp.getString("platform", "xd") ?: "xd"
        homeUrl = sp.getString("home_url", DEFAULT_HOME) ?: DEFAULT_HOME
        desktopMode = sp.getBoolean("desktop_mode", true)
        deleteConfirm = sp.getBoolean(KEY_BM_CONFIRM, true)
        debugMode = sp.getBoolean("debug_mode", false)
        audioEnabled = sp.getBoolean("audio_enabled", true)
        externalEnabled = sp.getBoolean("external_enabled", true)
        // 原生侧在启动会话时读这两个静态量
        EmbedFlashController.audioEnabled = audioEnabled
        EmbedFlashController.externalEnabled = externalEnabled
        ballX = sp.getInt("ball_x", -1)
        ballY = sp.getInt("ball_y", -1)
    }

    private fun saveUiPrefs() {
        getSharedPreferences("sxdffle", MODE_PRIVATE).edit()
            .putBoolean("keep_alive", keepAlive)
            .putBoolean("auto_hide_toolbar", autoHideToolbar)
            .putBoolean("immersive", immersive)
            .putInt("toolbar_alpha_pct", toolbarAlphaPercent)
            .putString("platform", currentPlatform)
            .putString("home_url", homeUrl)
            .putBoolean("desktop_mode", desktopMode)
            .putBoolean("debug_mode", debugMode)
            .putBoolean("audio_enabled", audioEnabled)
            .putBoolean("external_enabled", externalEnabled)
            .apply()
    }

    private fun toggleLock() {
        locked = !locked
        embedView?.locked = locked
        val btn = toolbarView.findViewById<TextView>(R.id.btnLock)
        btn.text = if (locked) "\uD83D\uDD12" else "\uD83D\uDD13"
        btn.setTextColor(if (locked) 0xFFFFC107.toInt() else 0xFF808080.toInt())
        showStatus(if (locked) "画面已锁定：操作直达游戏" else "画面已解锁：可拖动/缩放")
    }

    /**
     * 展开/收起导航栏。
     *
     * 导航栏是独立 Window（PopupWindow），显隐**不改变任何布局**：
     * 网页模式下内容区自己让出高度，内嵌模式下内容区始终全屏、栏浮在游戏上。
     */
    private fun setToolbarExpanded(expand: Boolean) {
        if (expand) showToolbar() else hideToolbar()
    }

    /**
     * 内容区顶部留白。
     *
     * **只跟状态栏有关，跟导航栏完全无关** —— 导航栏对任何页面都是纯悬浮：
     * 显示时盖在内容上面，收起时内容原样露出来，页面一次都不会被推动。
     *
     * 沉浸模式下状态栏隐藏，这里恒为 0（网页真正从屏幕顶端开始）。
     */
    private var insetAnimator: android.animation.ValueAnimator? = null

    private fun updateContentInset(toolbarVisible: Boolean, animate: Boolean = true) {
        val area = findViewById<View>(R.id.contentArea) ?: return
        val lp = area.layoutParams as? ViewGroup.MarginLayoutParams ?: return

        val target = statusBarInset
        if (lp.topMargin == target) return

        insetAnimator?.cancel()
        if (!animate) {
            lp.topMargin = target
            area.layoutParams = lp
            return
        }
        insetAnimator = android.animation.ValueAnimator.ofInt(lp.topMargin, target).also { anim ->
            anim.duration = 180
            anim.addUpdateListener { va ->
                lp.topMargin = va.animatedValue as Int
                area.layoutParams = lp
            }
            anim.start()
        }
    }

    /**
     * 把顶部一条区域从系统手势里排除。
     *
     * `setSystemGestureExclusionRects` 只对「返回手势」有效，不能阻止状态栏下拉，
     * 但它能避免顶部区域的下滑被系统返回手势抢走。每边上限 200dp。
     */
    private fun updateGestureExclusion() {
        if (android.os.Build.VERSION.SDK_INT < android.os.Build.VERSION_CODES.Q) return
        val root = findViewById<View>(R.id.contentArea) ?: return
        val h = (48 * resources.displayMetrics.density).toInt().coerceAtMost(200)
        val w = if (root.width > 0) root.width else resources.displayMetrics.widthPixels
        root.systemGestureExclusionRects =
            if (w > 0) listOf(android.graphics.Rect(0, 0, w, h)) else emptyList()
    }

    private fun scheduleHideToolbar() {
        if (!::toolbarView.isInitialized) return
        toolbarView.removeCallbacks(hideToolbarRunnable)
        if (autoHideToolbar && !toolbarCollapsed) {
            toolbarView.postDelayed(hideToolbarRunnable, 3000)
        }
    }

    /**
     * 唤出已收起的顶部栏。
     *
     * 系统状态栏的下拉是系统手势（应用层无法拦截），所以主路径是
     * **边缘内滑**：竖屏用左右边缘、横屏用上下边缘 —— 也就是系统返回手势的区域，
     * 我们抢先消费一次来唤出顶部栏，避免"收起后再也拉不出来"。
     * 顶部 48dp 的下滑与单击仍然保留（能收到就用）。
     */
    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        // 任何触摸都重置自动收起计时（收起状态下不重置，否则永远收不起来）
        if (autoHideToolbar && !toolbarCollapsed) scheduleHideToolbar()

        val dm = resources.displayMetrics
        val edgeZone = 28 * dm.density
        // 顶部热区只留「屏幕最上沿」一条，24dp 足够起手下滑，
        // 又不会盖住网页自己的顶部导航（注册 / 登录 / 快速登录都在那一带）。
        // 之前是 56dp，把 669ye 这类页面顶栏整个吞掉了。
        val topZone = 24 * dm.density
        val trigger = 20 * dm.density

        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                edgeMode = 0
                if (!toolbarCollapsed) return super.dispatchTouchEvent(ev)

                val vw = findViewById<View>(R.id.contentArea)?.width ?: 0
                val vh = findViewById<View>(R.id.contentArea)?.height ?: 0
                if (vw <= 0 || vh <= 0) return super.dispatchTouchEvent(ev)
                val landscape = vw > vh

                edgeDownX = ev.x
                edgeDownY = ev.y
                edgeMode = when {
                    // 顶部只认「下滑」：单击必须留给网页（注册 / 登录 就在这儿）
                    ev.y < topZone -> 5
                    // 竖屏：左右边缘
                    !landscape && ev.x < edgeZone -> 1
                    !landscape && ev.x > vw - edgeZone -> 2
                    // 横屏：上下边缘
                    landscape && ev.y < edgeZone -> 3
                    landscape && ev.y > vh - edgeZone -> 4
                    else -> 0
                }
                if (edgeMode != 0) Log.i(TAG, "边缘手势跟踪: mode=$edgeMode at (${ev.x},${ev.y})")
            }

            MotionEvent.ACTION_MOVE -> {
                if (edgeMode == 0) return super.dispatchTouchEvent(ev)
                val dx = ev.x - edgeDownX
                val dy = ev.y - edgeDownY
                val fired = when (edgeMode) {
                    1 -> dx > trigger            // 左边缘 → 右滑
                    2 -> -dx > trigger           // 右边缘 → 左滑
                    3 -> dy > trigger            // 上边缘 → 下滑
                    4 -> -dy > trigger           // 下边缘 → 上滑
                    5 -> dy > trigger            // 顶部 → 下滑
                    else -> false
                }
                if (fired) {
                    edgeMode = 0
                    setToolbarExpanded(true)
                    showStatus("顶部栏已唤出")
                    return true   // 消费掉，不再传给内嵌游戏视图
                }
                // 方向明显不对 → 放弃跟踪，交回正常流程
                if (kotlin.math.abs(dx) > trigger * 3 || kotlin.math.abs(dy) > trigger * 3) {
                    edgeMode = 0
                }
            }

            MotionEvent.ACTION_UP -> {
                // 注意：这里**不能**把顶部区域的"单击"当成唤出信号。
                //
                // 之前 ACTION_UP 一旦落在顶部热区就 return true，事件被吞掉，
                // 网页永远收不到点击 —— 表现就是「点注册没反应，反而弹出导航栏」。
                // 唤出只认下滑手势（ACTION_MOVE 里判定），单击原样放行。
                edgeMode = 0
            }

            MotionEvent.ACTION_CANCEL -> edgeMode = 0
        }
        return super.dispatchTouchEvent(ev)
    }

    // ==================================================================
    //  渲染分辨率（舞台尺寸）
    // ==================================================================

    // ==================================================================
    //  登录态导出
    // ==================================================================

    // ==================================================================
    //  网址：协议探测 / 首页 / 收藏 / 浏览记录
    // ==================================================================

    // ==================================================================
    //  左下角内存指示条
    // ==================================================================

    private var memChip: PopupWindow? = null
    private var memChipView: TextView? = null

    private val memChipTick = object : Runnable {
        override fun run() {
            memChipView?.text = memChipText()
            memHandler.postDelayed(this, 2000)
        }
    }

    /**
     * 左下角常显的半透明内存条：「本软件占用 / 系统剩余」。
     *
     * 必须用 PopupWindow：内嵌 Flash 是 SurfaceView，同一个 Window 里叠在它
     * 上面的普通 View 会被打洞挖掉（工具栏、悬浮球都踩过这个坑）。
     *
     * 同时 `isTouchable = false`，所有触摸都穿透给下面的网页/游戏，
     * 玩家点左下角的按钮不会被它挡住。
     */
    private fun showMemChip() {
        if (memChipView != null) return

        val tv = TextView(this).apply {
            setTextColor(0xE6FFFFFF.toInt())
            textSize = 10.5f
            gravity = Gravity.CENTER
            setPadding(dp(7), dp(3), dp(7), dp(3))
            background = android.graphics.drawable.GradientDrawable().apply {
                cornerRadius = dp(8).toFloat()
                setColor(0x4D000000)
            }
            text = memChipText()
            // 不参与无障碍焦点，避免读屏软件每次刷新都念一遍
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        }

        memChipView = tv
        memChip = PopupWindow(
            tv,
            ViewGroup.LayoutParams.WRAP_CONTENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply {
            isFocusable = false
            isTouchable = false
            isOutsideTouchable = false
            setBackgroundDrawable(ColorDrawable(android.graphics.Color.TRANSPARENT))
        }
        showMemChipAt()
        memHandler.postDelayed(memChipTick, 2000)
    }

    private fun showMemChipAt() {
        val p = memChip ?: return
        if (p.isShowing) p.dismiss()
        try {
            // 底部留 26dp，避开手势导航条；左边缘 10dp
            p.showAtLocation(
                window.decorView,
                Gravity.BOTTOM or Gravity.START,
                dp(10),
                dp(26)
            )
        } catch (_: Throwable) {
        }
    }

    private fun hideMemChip() {
        memHandler.removeCallbacks(memChipTick)
        runCatching { memChip?.dismiss() }
        memChip = null
        memChipView = null
    }

    /** 「1.2G / 2.4G」：前者是本进程 RSS（含 native），后者是系统当前可用内存 */
    private fun memChipText(): String {
        val used = currentRssMb()
        val info = android.app.ActivityManager.MemoryInfo()
        runCatching {
            (getSystemService(ACTIVITY_SERVICE) as android.app.ActivityManager).getMemoryInfo(info)
        }
        val avail = info.availMem / (1024 * 1024)

        fun fmt(mb: Long): String =
            if (mb >= 1024) String.format(java.util.Locale.US, "%.1fG", mb / 1024f)
            else "${mb}M"

        return "${fmt(used)} / ${fmt(avail)}"
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

    /**
     * 智能打开用户输入的地址。
     *
     * 正式浏览器不会无脑补 `https://`：很多页游平台（3fangyuan、7k7k、
     * 各个联运站）根本没有 443，硬走 https 只会得到一个错误页。
     * 这里的做法是先探测 443 通不通，通就走 https，否则退回 http：
     *   zhizhuyx.3fangyuan.com  → http://…
     *   www.qiquyou.com         → https://…
     * 探测在后台线程做，最多等 1.5 秒，不会卡住界面。
     */
    /** 当前进程 RSS（MB），用于切站前判断是否需要等内存归还 */
    private fun currentRssMb(): Long {
        return runCatching {
            java.io.File("/proc/self/status").useLines { lines ->
                lines.firstOrNull { it.startsWith("VmRSS:") }
                    ?.substringAfter("VmRSS:")?.trim()?.substringBefore(" ")?.toLongOrNull()
            }
        }.getOrNull()?.div(1024) ?: 0L
    }

    /**
     * 用户主动切换网址。
     *
     * 直接 loadUrl 会让"上一局引擎还占着 1.2~1.4GB"和"新页面的分配峰值"叠在一起，
     * 实测会在 200ms 内以 SIGABRT 崩掉。所以先把内嵌关掉，内存高的时候
     * 再等一小会儿（系统/分配器会在这段时间把页还回去），然后才真正加载。
     */
    private fun openUrl(target: String) {
        if (target.isBlank()) return
        if (embedView != null) {
            closeEmbedded()
            logMemory("切站")
        }
        waitMemThenLoad(target, 0)
    }

    /**
     * 等内存落下来再加载。
     *
     * 引擎释放后，页并不一定立刻还给内核（要等分配器 purge / GPU 驱动回收），
     * 这时马上下一个页面，新页面的分配峰值会直接顶到 1.5GB 触发 abort。
     * 最多轮询 2.4 秒，降下来就立刻走，降不下来也照样走（不能无限等）。
     */
    private fun waitMemThenLoad(target: String, attempt: Int) {
        val rss = currentRssMb()
        if (rss <= 800) {
            if (attempt > 0) logToFile("切站：等待 ${attempt * 300}ms 后 RSS ${rss}MB，开始加载")
            webView.loadUrl(target)
            return
        }
        // 等 6.6 秒还降不下来，说明这一局的 Player 被挂起的 future 攥着，
        // 再打开新页面只会把峰值叠到 1.5GB 以上（接下来就是 SIGABRT）。
        // 这种时候唯一可靠的办法是重启进程：内存清零，再把地址带回来。
        // 等待时间要略长于引擎侧的收尾等待（6 秒），否则会在它还在收尾时抢跑。
        if (attempt >= 22) {
            logToFile("切站：等待 6.6s 后 RSS 仍为 ${rss}MB，改为重启进程")
            restartWith(target)
            return
        }
        webView.postDelayed({ waitMemThenLoad(target, attempt + 1) }, 300)
    }

    /**
     * 重启进程并带上要打开的地址。
     *
     * 用 AlarmManager 记一个"马上启动 Activity"的闹钟再自杀 —— 进程被杀之后
     * 闹钟仍由系统持有，会以全新进程拉起，内存是干净的。
     * 这是最后手段：只有内存确实回收不掉时才会走到这里。
     */
    private fun restartWith(target: String) {
        runCatching {
            val intent = Intent(this, BrowserActivity::class.java).apply {
                putExtra(EXTRA_OPEN_URL, target)
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
            }
            val flags = android.app.PendingIntent.FLAG_UPDATE_CURRENT or
                android.app.PendingIntent.FLAG_IMMUTABLE
            val pi = android.app.PendingIntent.getActivity(this, 1001, intent, flags)
            val am = getSystemService(ALARM_SERVICE) as android.app.AlarmManager
            am.setWindow(
                android.app.AlarmManager.RTC,
                System.currentTimeMillis() + 500,
                500,
                pi
            )
        }.onFailure {
            logToFile("重启失败: ${it.message}")
            return
        }

        showStatus("内存未能释放，正在重启…")
        android.os.Handler(android.os.Looper.getMainLooper()).postDelayed({
            android.os.Process.killProcess(android.os.Process.myPid())
        }, 300)
    }

    private fun loadSmart(raw: String) {
        var input = raw.trim()
        if (input.isEmpty()) return

        if (input.startsWith("//")) input = "https:$input"
        // 已经写明协议的，尊重用户
        if (Regex("^[a-zA-Z][a-zA-Z0-9+.-]*://").find(input) != null) {
            openUrl(input)
            return
        }
        // 不像网址 → 交给搜索引擎
        if (!input.contains('.') || input.contains(' ')) {
            openUrl("https://www.bing.com/search?q=" + Uri.encode(input))
            return
        }

        showStatus("正在连接…")
        val ua = webView.settings.userAgentString
        Thread {
            val httpsOk = probeHttps(input, ua)
            runOnUiThread {
                val target = (if (httpsOk) "https://" else "http://") + input
                logToFile("协议探测: $input -> $target")
                openUrl(target)
            }
        }.apply { isDaemon = true }.start()
    }

    /** 探测 https 是否可用（能拿到任意 HTTP 响应就算通） */
    private fun probeHttps(input: String, ua: String): Boolean = try {
        val conn = java.net.URL("https://$input").openConnection() as java.net.HttpURLConnection
        conn.connectTimeout = 1500
        conn.readTimeout = 1500
        conn.instanceFollowRedirects = false
        conn.requestMethod = "HEAD"
        conn.setRequestProperty("User-Agent", ua)
        val code = conn.responseCode
        conn.disconnect()
        code in 100..599
    } catch (e: Exception) {
        false
    }

    private fun openHome() {
        // 首页也走一遍协议探测：用户可能填了一个没有 https 的域名
        loadSmart(homeUrl)
    }

    private fun setCurrentAsHome() {
        val url = webView.url
        if (url.isNullOrBlank()) {
            showStatus("当前没有打开的页面")
            return
        }
        homeUrl = url
        saveUiPrefs()
        logToFile("设为首页: $url")
        showStatus("已设为首页：$url")
    }

    private fun loadEntries(key: String): MutableList<Pair<String, String>> {
        val raw = getSharedPreferences("sxdffle", MODE_PRIVATE).getString(key, null)
            ?: return mutableListOf()
        val out = mutableListOf<Pair<String, String>>()
        runCatching {
            val arr = org.json.JSONArray(raw)
            for (i in 0 until arr.length()) {
                val o = arr.optJSONObject(i) ?: continue
                val u = o.optString("url")
                if (u.isNotBlank()) out.add(u to o.optString("title"))
            }
        }
        return out
    }

    private fun saveEntries(key: String, list: List<Pair<String, String>>) {
        val arr = org.json.JSONArray()
        list.forEach { (u, t) ->
            arr.put(org.json.JSONObject().apply {
                put("url", u)
                put("title", t)
            })
        }
        getSharedPreferences("sxdffle", MODE_PRIVATE).edit()
            .putString(key, arr.toString())
            .apply()
    }

    /** 记录一条浏览历史（去重、置顶、限长） */
    private fun recordHistory(url: String?) {
        if (url.isNullOrBlank() || !url.startsWith("http")) return
        val title = webView.title ?: ""
        val list = loadEntries(KEY_HISTORY)
        list.removeAll { it.first == url }
        list.add(0, url to title)
        while (list.size > HISTORY_MAX) list.removeAt(list.size - 1)
        saveEntries(KEY_HISTORY, list)
    }

    /** 收藏 / 取消收藏当前页 */
    // ---- 收藏夹（分组） ----

    private data class BmGroup(
        val name: String,
        val items: MutableList<Pair<String, String>>
    )

    private fun loadGroups(): MutableList<BmGroup> {
        val raw = getSharedPreferences("sxdffle", MODE_PRIVATE)
            .getString(KEY_BOOKMARKS, null) ?: return mutableListOf()
        val out = mutableListOf<BmGroup>()
        runCatching {
            val arr = org.json.JSONArray(raw)
            var legacy = false
            for (i in 0 until arr.length()) {
                val o = arr.optJSONObject(i) ?: continue
                val itemsArr = o.optJSONArray("items")
                if (itemsArr == null) {
                    legacy = true
                    break
                }
                val items = mutableListOf<Pair<String, String>>()
                for (j in 0 until itemsArr.length()) {
                    val it = itemsArr.optJSONObject(j) ?: continue
                    val u = it.optString("url")
                    if (u.isNotBlank()) items.add(u to it.optString("title"))
                }
                out.add(BmGroup(o.optString("name", "收藏"), items))
            }
            if (legacy) {
                // 旧版是扁平数组 [{url,title}]：迁移成单个「收藏」分组
                out.clear()
                val items = mutableListOf<Pair<String, String>>()
                for (i in 0 until arr.length()) {
                    val o = arr.optJSONObject(i) ?: continue
                    val u = o.optString("url")
                    if (u.isNotBlank()) items.add(u to o.optString("title"))
                }
                if (items.isNotEmpty()) out.add(BmGroup("收藏", items))
            }
        }
        return out
    }

    private fun saveGroups(groups: List<BmGroup>) {
        val arr = org.json.JSONArray()
        groups.forEach { g ->
            arr.put(org.json.JSONObject().apply {
                put("name", g.name)
                put("items", org.json.JSONArray().apply {
                    g.items.forEach { (u, t) ->
                        put(org.json.JSONObject().apply {
                            put("url", u)
                            put("title", t)
                        })
                    }
                })
            })
        }
        getSharedPreferences("sxdffle", MODE_PRIVATE).edit()
            .putString(KEY_BOOKMARKS, arr.toString())
            .apply()
    }

    /**
     * 首次启动导入内置的联运平台清单。
     *
     * 导入后就是普通分组：可以改名、增删条目、整个删掉 —— 不做任何"内置不可改"的限制。
     */
    private fun importDefaultBookmarksOnce() {
        val sp = getSharedPreferences("sxdffle", MODE_PRIVATE)
        if (sp.getBoolean(KEY_BM_IMPORTED, false)) return
        val groups = loadGroups()
        val name = "联运平台"
        var added = 0
        if (groups.none { it.name == name }) {
            val items = mutableListOf<Pair<String, String>>()
            runCatching {
                assets.open("default_bookmarks.txt").bufferedReader().forEachLine { line ->
                    val l = line.trim()
                    if (l.isEmpty() || !l.contains('|')) return@forEachLine
                    val n = l.substringBefore('|').trim()
                    val u = l.substringAfter('|').trim()
                    if (u.isNotEmpty()) items.add(u to n)
                }
            }
            if (items.isNotEmpty()) {
                groups.add(0, BmGroup(name, items))
                saveGroups(groups)
                added = items.size
            }
        }
        sp.edit().putBoolean(KEY_BM_IMPORTED, true).apply()
        logToFile("默认收藏夹导入完成：$name +$added 项")
    }

    /** 收藏当前页：已收藏则取消，未收藏则选分组 */
    private fun toggleBookmark() {
        val url = webView.url
        if (url.isNullOrBlank()) {
            showStatus("当前没有打开的页面")
            return
        }
        val groups = loadGroups()

        val existing = groups.firstOrNull { g -> g.items.any { it.first == url } }
        if (existing == null) {
            if (groups.isEmpty()) {
                newGroupDialog { name -> addToGroup(name, url, webView.title ?: url) }
                return
            }
            androidx.appcompat.app.AlertDialog.Builder(this)
                .setTitle("收藏到分组")
                .setItems(groups.map { it.name }.toTypedArray()) { _, which ->
                    addToGroup(groups[which].name, url, webView.title ?: url)
                }
                .setNeutralButton("新建分组") { _, _ ->
                    newGroupDialog { name -> addToGroup(name, url, webView.title ?: url) }
                }
                .setNegativeButton("取消", null)
                .show()
            return
        }

        // 已经收藏过：不再"再点一次就取消"——那太容易误删。
        // 取消收藏统一走「管理」，这里只给出位置提示。
        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("已经收藏")
            .setMessage("该网址已在「${existing.name}」分组中。\n\n如需取消收藏，可在 收藏夹 → 管理 里操作。")
            .setPositiveButton("取消收藏") { _, _ ->
                existing.items.removeAll { it.first == url }
                saveGroups(groups)
                showStatus("已从「${existing.name}」取消收藏")
            }
            .setNegativeButton("知道了", null)
            .show()
    }

    private fun addToGroup(groupName: String, url: String, title: String) {
        val groups = loadGroups()
        val g = groups.firstOrNull { it.name == groupName } ?: return
        if (g.items.none { it.first == url }) g.items.add(0, url to title)
        saveGroups(groups)
        showStatus("已收藏到「$groupName」")
    }

    /** 收藏夹：分组列表 */
    /**
     * 收藏夹：分组列表。
     *
     * 分组对话框不关闭就叠一层组内列表 —— 从分组里点「关闭」应该退回收藏夹，
     * 而不是一路退到网页。所以这里用 dismiss 监听在返回时刷新计数。
     */
    private fun showBookmarks() {
        val labels = mutableListOf<String>()
        val adapter = android.widget.ArrayAdapter(this, android.R.layout.simple_list_item_1, labels)
        val lv = android.widget.ListView(this)
        lv.adapter = adapter
        lv.setSelector(android.R.color.transparent)

        lateinit var dlg: androidx.appcompat.app.AlertDialog
        var ready = false

        fun reload() {
            labels.clear()
            labels.addAll(loadGroups().map { "${it.name}（${it.items.size}）" })
            adapter.notifyDataSetChanged()
            if (ready && labels.isEmpty()) {
                dlg.dismiss()
                newGroupDialog()
            }
        }
        reload()

        dlg = androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("收藏夹")
            .setView(lv)
            .setPositiveButton("设置", null)
            .setNeutralButton("管理", null)
            .setNegativeButton("关闭", null)
            .create()

        lv.setOnItemClickListener { _, _, pos, _ ->
            val groups = loadGroups()
            if (pos in groups.indices) {
                showGroup(
                    groups[pos].name,
                    onClosed = { reload() },
                    // 选中网址后把收藏夹这一层也收掉，否则网页在对话框背后加载
                    onPicked = { dlg.dismiss() }
                )
            }
        }
        dlg.setOnShowListener {
            ready = true
            // 关键：这两个按钮**不能**用 setPositiveButton/setNeutralButton 的默认行为，
            // 那样点完就把对话框关了 —— 在设置/管理里再点关闭就一路退到网页。
            val alert = dlg
            alert.getButton(androidx.appcompat.app.AlertDialog.BUTTON_POSITIVE)
                .setOnClickListener { showBookmarkSettings() }
            alert.getButton(androidx.appcompat.app.AlertDialog.BUTTON_NEUTRAL)
                .setOnClickListener { showManageGroups { reload() } }
        }
        dlg.show()
        if (labels.isEmpty()) {
            dlg.dismiss()
            newGroupDialog()
        }
    }

    /** 分组内容：单击打开；批量操作统一走「管理」 */
    private fun showGroup(
        name: String,
        onClosed: () -> Unit,
        onPicked: () -> Unit = {}
    ) {
        val labels = mutableListOf<String>()
        val adapter = android.widget.ArrayAdapter(this, android.R.layout.simple_list_item_1, labels)
        val lv = android.widget.ListView(this)
        lv.adapter = adapter
        lv.setSelector(android.R.color.transparent)

        fun reload() {
            val g = loadGroups().firstOrNull { it.name == name }
            labels.clear()
            g?.items?.forEach { (u, t) -> labels.add(if (t.isBlank()) u else "$t\n$u") }
            adapter.notifyDataSetChanged()
        }
        reload()
        if (labels.isEmpty()) {
            showStatus("「$name」还没有收藏")
            onClosed()
            return
        }

        val dlg = androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle(name)
            .setView(lv)
            .setNeutralButton("管理", null)
            .setNegativeButton("关闭", null)
            .create()

        lv.setOnItemClickListener { _, _, pos, _ ->
            val u = loadGroups().firstOrNull { it.name == name }
                ?.items?.getOrNull(pos)?.first
            dlg.dismiss()      // 关掉本组
            onPicked()         // 再关掉收藏夹（如果有）
            // 收藏里存的是原始写法（可能没写协议），走一次协议探测
            if (!u.isNullOrBlank()) loadSmart(u)
        }
        dlg.setOnShowListener {
            // 「管理」同样不能用默认行为：点了会连分组页一起关掉
            dlg.getButton(androidx.appcompat.app.AlertDialog.BUTTON_NEUTRAL)
                .setOnClickListener { showManageItems(name) { reload() } }
        }
        dlg.setOnDismissListener { onClosed() }
        dlg.show()
    }

    /**
     * 批量管理分组：多选 → 置顶 / 重命名 / 删除。
     *
     * 置顶按"勾选顺序"排：先勾的排最前。用一个有序列表记录点击顺序，
     * 而不是读 ListView 的 checkedItemPositions（那是按位置排序的）。
     */
    private fun showManageGroups(onChanged: () -> Unit) {
        val groups = loadGroups()
        if (groups.isEmpty()) {
            showStatus("还没有分组")
            return
        }
        val labels = groups.map { "${it.name}（${it.items.size}）" }
        val order = mutableListOf<Int>()
        val adapter = CheckListAdapter(labels, order)

        val lv = android.widget.ListView(this)
        lv.adapter = adapter

        // 构建面板时就要能关掉对话框，先占个引用
        var dlgRef: androidx.appcompat.app.AlertDialog? = null
        val dlg = androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("管理分组")
            .setView(buildManagePanel(lv) { action ->
                when (action) {
                    "置顶" -> {
                        if (order.isEmpty()) { showStatus("先勾选分组"); return@buildManagePanel }
                        val picked = order.mapNotNull { groups.getOrNull(it) }
                        val all = loadGroups()
                        val names = picked.map { it.name }
                        val chosen = names.mapNotNull { n -> all.firstOrNull { it.name == n } }
                        all.removeAll { it.name in names }
                        chosen.asReversed().forEach { all.add(0, it) }
                        saveGroups(all)
                        dlgRef?.dismiss(); onChanged(); showStatus("已置顶 ${names.size} 个分组")
                    }
                    "重命名" -> {
                        if (order.size != 1) { showStatus("重命名只能选一个分组"); return@buildManagePanel }
                        val g = groups.getOrNull(order[0]) ?: return@buildManagePanel
                        dlgRef?.dismiss()
                        renameGroupDialog(g.name)
                    }
                    "删除" -> {
                        if (order.isEmpty()) { showStatus("先勾选分组"); return@buildManagePanel }
                        val names = order.mapNotNull { groups.getOrNull(it)?.name }
                        confirmDelete(names.size) {
                            val all = loadGroups()
                            all.removeAll { it.name in names }
                            saveGroups(all)
                            dlgRef?.dismiss(); onChanged(); showStatus("已删除 ${names.size} 个分组")
                        }
                    }
                }
            })
            .setNegativeButton("完成", null)
            .create()
        dlgRef = dlg
        dlg.show()
    }

    /** 批量管理组内条目：多选 → 置顶 / 删除 */
    private fun showManageItems(groupName: String, onChanged: () -> Unit) {
        val group = loadGroups().firstOrNull { it.name == groupName } ?: return
        val items = group.items.toMutableList()
        if (items.isEmpty()) {
            showStatus("「$groupName」还没有收藏")
            return
        }
        val labels = items.map { (u, t) -> if (t.isBlank()) u else "$t · $u" }
        val order = mutableListOf<Int>()
        val adapter = CheckListAdapter(labels, order)

        val lv = android.widget.ListView(this)
        lv.adapter = adapter

        // 构建面板时就要能关掉对话框，先占个引用
        var dlgRef: androidx.appcompat.app.AlertDialog? = null
        val dlg = androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("管理 · $groupName")
            .setView(buildManagePanel(lv, showRename = false) { action ->
                when (action) {
                    "置顶" -> {
                        if (order.isEmpty()) { showStatus("先勾选条目"); return@buildManagePanel }
                        val chosen = order.mapNotNull { items.getOrNull(it) }
                        val all = loadGroups()
                        val g = all.firstOrNull { it.name == groupName } ?: return@buildManagePanel
                        val urls = chosen.map { it.first }
                        val picked = urls.mapNotNull { u -> g.items.firstOrNull { it.first == u } }
                        g.items.removeAll { it.first in urls }
                        // 逆序插入，保证"先勾的排最前"
                        picked.asReversed().forEach { g.items.add(0, it) }
                        saveGroups(all)
                        dlgRef?.dismiss(); onChanged(); showStatus("已置顶 ${urls.size} 项")
                    }
                    "删除" -> {
                        if (order.isEmpty()) { showStatus("先勾选条目"); return@buildManagePanel }
                        val urls = order.mapNotNull { items.getOrNull(it)?.first }
                        confirmDelete(urls.size) {
                            val all = loadGroups()
                            all.firstOrNull { it.name == groupName }?.items?.removeAll { it.first in urls }
                            saveGroups(all)
                            dlgRef?.dismiss(); onChanged(); showStatus("已删除 ${urls.size} 项")
                        }
                    }
                }
            })
            .setNegativeButton("完成", null)
            .create()
        dlgRef = dlg
        dlg.show()
    }

    /** 多选行：复选框 + 两行文字，点击整行切换勾选 */
    private inner class CheckListAdapter(
        private val labels: List<String>,
        private val order: MutableList<Int>
    ) : android.widget.BaseAdapter() {
        override fun getCount(): Int = labels.size
        override fun getItem(position: Int): Any = labels[position]
        override fun getItemId(position: Int): Long = position.toLong()

        override fun getView(position: Int, convertView: View?, parent: ViewGroup?): View {
            val row = android.widget.LinearLayout(this@BrowserActivity).apply {
                orientation = android.widget.LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(10), dp(6), dp(12), dp(6))
                minimumHeight = dp(54)
            }
            row.addView(android.widget.CheckBox(this@BrowserActivity).apply {
                isChecked = order.contains(position)
                isClickable = false
                isFocusable = false
            })
            row.addView(TextView(this@BrowserActivity).apply {
                text = labels[position]
                setTextColor(0xFFFFFFFF.toInt())
                textSize = 14f
                maxLines = 3
                setPadding(dp(6), 0, 0, 0)
            })
            row.setOnClickListener {
                if (order.contains(position)) order.remove(position) else order.add(position)
                notifyDataSetChanged()
            }
            return row
        }
    }

    /** 列表 + 底部操作条（置顶 / 重命名 / 删除） */
    private fun buildManagePanel(
        lv: android.widget.ListView,
        showRename: Boolean = true,
        onAction: (String) -> Unit
    ): View {
        val root = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.VERTICAL
        }
        val listH = minOf(dp(380), (resources.displayMetrics.heightPixels * 0.52f).toInt())
        root.addView(
            lv,
            android.widget.LinearLayout.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT, listH
            )
        )

        val bar = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.HORIZONTAL
            setPadding(dp(8), dp(4), dp(8), dp(4))
        }
        fun barButton(label: String, color: Int, enabled: Boolean = true) =
            TextView(this).apply {
                text = label
                setTextColor(if (enabled) color else 0xFF666666.toInt())
                textSize = 14f
                gravity = Gravity.CENTER
                setPadding(0, dp(12), 0, dp(12))
                layoutParams = android.widget.LinearLayout.LayoutParams(0, android.view.ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
                isEnabled = enabled
                setOnClickListener { onAction(label) }
            }

        bar.addView(barButton("置顶", 0xFF64B5F6.toInt()))
        if (showRename) bar.addView(barButton("重命名", 0xFFB0BEC5.toInt()))
        bar.addView(barButton("删除", 0xFFE57373.toInt()))
        root.addView(bar)
        return root
    }

    /**
     * 删除确认。
     *
     * 用户可以勾「不再提示」一次性关掉；想恢复就到收藏夹 → 设置 里重新打开。
     */
    private fun confirmDelete(count: Int, onConfirmed: () -> Unit) {
        if (!deleteConfirm) {
            onConfirmed()
            return
        }
        val cb = android.widget.CheckBox(this).apply {
            text = "不再提示"
            setTextColor(0xFF9E9E9E.toInt())
            setPadding(dp(20), dp(8), dp(20), 0)
        }
        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("确认删除")
            .setMessage("将删除 $count 项，此操作不可撤销。")
            .setView(cb)
            .setPositiveButton("删除") { _, _ ->
                if (cb.isChecked) {
                    deleteConfirm = false
                    getSharedPreferences("sxdffle", MODE_PRIVATE).edit()
                        .putBoolean(KEY_BM_CONFIRM, false).apply()
                    showStatus("已关闭删除确认（可在 设置 里恢复）")
                }
                onConfirmed()
            }
            .setNegativeButton("取消", null)
            .show()
    }

    /** 收藏夹设置 */
    private fun showBookmarkSettings() {
        val cb = android.widget.CheckBox(this).apply {
            text = "删除时弹窗确认"
            isChecked = deleteConfirm
            setTextColor(0xFFE0E0E0.toInt())
            setPadding(dp(20), dp(12), dp(20), dp(12))
        }
        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("收藏夹设置")
            .setView(cb)
            .setPositiveButton("保存") { _, _ ->
                deleteConfirm = cb.isChecked
                getSharedPreferences("sxdffle", MODE_PRIVATE).edit()
                    .putBoolean(KEY_BM_CONFIRM, deleteConfirm).apply()
                showStatus(if (deleteConfirm) "删除时会先确认" else "删除不再确认")
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun renameGroupDialog(oldName: String) {
        val et = EditText(this).apply {
            setText(oldName)
            setSelection(text.length)
            setSingleLine()
            setPadding(48, 32, 48, 32)
        }
        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("重命名分组")
            .setView(et)
            .setPositiveButton("确定") { _, _ ->
                val v = et.text.toString().trim()
                if (v.isEmpty()) {
                    showStatus("名称不能为空")
                    return@setPositiveButton
                }
                val groups = loadGroups()
                val g = groups.firstOrNull { it.name == oldName } ?: return@setPositiveButton
                groups.removeAll { it.name == v && it !== g }
                groups[groups.indexOfFirst { it === g }] = BmGroup(v, g.items)
                saveGroups(groups)
                showStatus("已重命名为「$v」")
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun newGroupDialog(onCreated: (String) -> Unit = {}) {
        val et = EditText(this).apply {
            hint = "分组名称"
            setSingleLine()
            setPadding(48, 32, 48, 32)
        }
        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("新建分组")
            .setView(et)
            .setPositiveButton("创建") { _, _ ->
                val v = et.text.toString().trim()
                if (v.isEmpty()) {
                    showStatus("名称不能为空")
                    return@setPositiveButton
                }
                val groups = loadGroups()
                if (groups.any { it.name == v }) {
                    showStatus("分组「$v」已存在")
                    return@setPositiveButton
                }
                groups.add(BmGroup(v, mutableListOf()))
                saveGroups(groups)
                onCreated(v)
                showStatus("已创建分组「$v」")
            }
            .setNegativeButton("取消", null)
            .show()
    }

    /**
     * 导出当前登录态（Cookie）。
     *
     * 用途：把一个平台上「已登录」的状态交给外部复现，不需要交出账号密码。
     * 覆盖范围：当前页、它的上级域、以及心动体系常用的几个域。
     */
    private fun copyLoginState() {
        val url = webView.url
        if (url.isNullOrBlank()) {
            showStatus("当前没有打开的页面")
            return
        }
        val host = runCatching { Uri.parse(url).host }.getOrNull() ?: ""
        val parts = host.split(".")

        val targets = LinkedHashSet<String>()
        targets.add(url)
        if (parts.size >= 2) {
            targets.add("https://" + parts.takeLast(2).joinToString("."))
        }
        if (parts.size >= 3) {
            targets.add("https://" + parts.takeLast(3).joinToString("."))
        }
        // 心动体系常见的几个域，跨站登录态通常落在这里
        targets.add("https://sxd.xd.com/")
        targets.add("https://api.xd.com/")
        targets.add("https://uri.xdcdn.net/")

        val cm = CookieManager.getInstance()
        val arr = org.json.JSONArray()
        var count = 0
        for (t in targets) {
            val c = runCatching { cm.getCookie(t) }.getOrNull()
            if (c.isNullOrBlank()) continue
            arr.put(org.json.JSONObject().apply {
                put("target", t)
                put("cookie", c)
            })
            count++
        }

        if (count == 0) {
            showStatus("当前页面没有可导出的 Cookie（先登录）")
            return
        }

        val payload = org.json.JSONObject().apply {
            put("platform", currentPlatform)
            put("url", url)
            put("exported_at", System.currentTimeMillis())
            put("cookies", arr)
        }.toString(2)

        val cb = getSystemService(CLIPBOARD_SERVICE) as android.content.ClipboardManager
        cb.setPrimaryClip(android.content.ClipData.newPlainText("SXDffle 登录态", payload))

        // ---- 同时落盘成文件 ----
        // 剪贴板在部分机型/部分输入法下会被截断或丢失，长 Cookie 串尤其容易出问题；
        // 写成 .json 文件既能直接分享，也便于用电脑复现。
        val file = writeLoginStateFile(payload)
        logToFile("已导出登录态：平台=$currentPlatform 域数=$count url=$url file=${file?.absolutePath ?: "-"}")

        if (file == null) {
            showStatus("已复制登录态（$count 个域），但文件写入失败")
            return
        }
        showLoginStateDialog(file, count)
    }

    /** 把登录态 JSON 写进 Android/data/<包名>/files/login/ 下 */
    private fun writeLoginStateFile(payload: String): File? = try {
        val dir = File(getExternalFilesDir(null) ?: filesDir, "login").apply { mkdirs() }
        val ts = SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(Date())
        val f = File(dir, "login_${currentPlatform}_$ts.json")
        f.writeText(payload, Charsets.UTF_8)
        f
    } catch (e: Exception) {
        Log.e(TAG, "写登录态文件失败", e)
        null
    }

    /** 导出完成后的操作面板：分享文件 / 只复制路径 */
    private fun showLoginStateDialog(file: File, count: Int) {
        val path = file.absolutePath
        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("登录态已导出")
            .setMessage("$count 个域已保存为文件：\n\n$path\n\n剪贴板里也有一份，可直接粘贴发送。")
            .setPositiveButton("分享文件") { _, _ -> shareFile(file, "application/json", "SXDffle 登录态") }
            .setNeutralButton("复制路径") { _, _ ->
                val cb = getSystemService(CLIPBOARD_SERVICE) as android.content.ClipboardManager
                cb.setPrimaryClip(android.content.ClipData.newPlainText("路径", path))
                showStatus("文件路径已复制")
            }
            .setNegativeButton("知道了", null)
            .show()
    }

    /** 通过系统分享面板把一个文件发出去 */
    private fun shareFile(file: File, mime: String, subject: String) {
        try {
            val uri = FileProvider.getUriForFile(this, "$packageName.fileprovider", file)
            val intent = Intent(Intent.ACTION_SEND).apply {
                type = mime
                putExtra(Intent.EXTRA_STREAM, uri)
                putExtra(Intent.EXTRA_SUBJECT, subject)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            startActivity(Intent.createChooser(intent, subject))
        } catch (e: Exception) {
            Log.e(TAG, "分享失败", e)
            showStatus("分享失败：${e.message}")
        }
    }

    private fun stageSizeLabel(): String =
        if (stageW <= 0 || stageH <= 0) "自动（SWF 原生）" else "$stageW × $stageH"

    private fun loadStageSize() {
        val sp = getSharedPreferences("sxdffle", MODE_PRIVATE)
        stageW = sp.getInt("stage_w", 0)
        stageH = sp.getInt("stage_h", 0)
    }

    private fun saveStageSize() {
        getSharedPreferences("sxdffle", MODE_PRIVATE).edit()
            .putInt("stage_w", stageW)
            .putInt("stage_h", stageH)
            .apply()
    }

    /**
     * 选择渲染分辨率（舞台尺寸）。
     *
     * SWF 头里的 stage 尺寸（《神仙道》是 1250×650）决定了游戏认为的"画布"大小。
     * 部分功能需要更大的画布才排得开，这里允许手动放大 —— 游戏会按新的
     * `Stage.stageWidth/Height` 重新布局。
     */
    private fun showStageSizeDialog() {
        val labels = arrayOf(
            "自动（SWF 原生）",
            "1360 × 768",
            "1600 × 900",
            "1920 × 1080",
            "2560 × 1440",
        )
        val sizes = arrayOf(0 to 0, 1360 to 768, 1600 to 900, 1920 to 1080, 2560 to 1440)
        val checked = sizes.indexOfFirst { it.first == stageW && it.second == stageH }
            .coerceAtLeast(0)

        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("渲染分辨率")
            .setSingleChoiceItems(labels, checked) { dialog, which ->
                dialog.dismiss()
                applyStageSize(sizes[which].first, sizes[which].second)
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun applyStageSize(w: Int, h: Int) {
        stageW = w
        stageH = h
        saveStageSize()

        val v = embedView
        if (v == null || !v.isActive) {
            showStatus("已设为 ${stageSizeLabel()}，下次内嵌播放生效")
            return
        }

        // 分辨率一变，Surface 的物理尺寸就得跟着变（这是 NoScale 生效的前提），
        // 而 Surface 尺寸改变必然重建 —— 只能重开一次会话。
        showStatus("正在按 ${stageSizeLabel()} 重建…")
        val url = embedSwfUrl
        val page = embedPageUrl
        val rw = embedRectW
        val rh = embedRectH
        val vars = embedVars
        val fromFrame = embedFromFrame
        closeEmbedded()
        openFlashContent(url, page, 0, 0, rw, rh, vars, fromFrame)
        webView.postDelayed({
            webView.evaluateJavascript(
                "window.__SXDFFLE__ && window.__SXDFFLE__.pushRect && window.__SXDFFLE__.pushRect();",
                null
            )
        }, 600)
    }

    /**
     * 对内嵌播放执行一个操作；没有内嵌时给出提示而不是静默失败。
     * （底部工具条已移除，操作统一走 ☰ 菜单，所以需要这个守卫。）
     */
    private inline fun withEmbed(name: String, action: (EmbedFlashView) -> Unit) {
        val v = embedView
        if (v == null || !v.isActive) {
            showStatus("当前没有内嵌播放，无法$name")
            return
        }
        action(v)
        showStatus("$name：${v.describeTransform()}")
    }

    /**
     * 页面滚动时同步位置。
     *
     * 只改 margin、**不改尺寸** —— 尺寸变化会让 Surface 重建，
     * 滚动时每秒几十次重建会直接把画面卡死。
     */
    private fun updateEmbedRect(x: Int, y: Int, w: Int, h: Int) {
        if (embedView == null) return
        // 子框架里播的 Flash 不跟随网页滚动（坐标基准不同，跟了反而乱跳）
        if (embedFromFrame) return
        val container = findViewById<FrameLayout>(R.id.flashContainer)
        val lp = container.layoutParams as? FrameLayout.LayoutParams ?: return
        val px = cssToView(x).coerceAtLeast(0)
        val py = cssToView(y).coerceAtLeast(0)
        val pw = cssToView(w).coerceAtLeast(64)
        val ph = cssToView(h).coerceAtLeast(64)

        // 位置总是跟随；尺寸只在明显变化时更新（避免滚动时反复重排）。
        // 注意：容器尺寸变化**不会**重建引擎 surface —— SurfaceView 已是固定像素，
        // 只是 applyTransform 会重新算一次 base 缩放把画面塞进新容器。
        val sizeChanged = abs(lp.width - pw) > 8 || abs(lp.height - ph) > 8
        if (lp.leftMargin == px && lp.topMargin == py && !sizeChanged) return
        lp.leftMargin = px
        lp.topMargin = py
        if (sizeChanged) {
            lp.width = pw
            lp.height = ph
        }
        container.layoutParams = lp
    }

    /**
     * 屏幕旋转（陀螺仪自动旋转）。
     *
     * 已声明 `configChanges`，Activity 不重建，但布局会重新测量。
     * 内嵌视图是固定像素尺寸的，需要按新窗口重新算一次显示变换，
     * 否则旋转后画面会停在旧比例上（看起来像"丢了"）。
     */
    override fun onConfigurationChanged(newConfig: android.content.res.Configuration) {
        super.onConfigurationChanged(newConfig)
        Log.i(TAG, "屏幕配置变化: ${newConfig.orientation}")
        logToFile("屏幕配置变化: orientation=${newConfig.orientation}")
        // 导航栏在独立窗口里，方向变了要重新算状态栏高度与位置
        updateStatusBarInset()
        // 内嵌视图是固定像素尺寸的，必须按新窗口重算一次：
        // 等 contentArea 完成新方向的测量后再动手，否则读到的是旧宽高。
        val content = findViewById<View>(R.id.contentArea)
        content.doOnLayout { relayoutEmbed() }
        content.postDelayed({ relayoutEmbed() }, 500)
        // 内存条钉在屏幕角落：PopupWindow 的位置不会自己跟着旋转走，
        // 等新方向测量完再重新贴一次左下角。
        memHandler.postDelayed({ showMemChipAt() }, 300)
    }

    /**
     * 旋转后重排内嵌视图。
     *
     * 两件事：
     *  1) 容器尺寸跟着新的内容区走（原来是固定像素，旋转后还留在旧尺寸上 → 错层）；
     *  2) 无自定义分辨率时让 Surface 也换成新尺寸（引擎视口 = Surface 尺寸）。
     *
     * 原位模式（网页里某块 Flash）则交给页面重新上报位置：
     * 旋转会让 WebView 重新排版，元素的 CSS 坐标已经变了。
     */
    private fun relayoutEmbed() {
        val view = embedView ?: return
        val content = findViewById<View>(R.id.contentArea)
        val container = findViewById<FrameLayout>(R.id.flashContainer)

        val maxW = if (content.width > 0) content.width else resources.displayMetrics.widthPixels
        val insetTop = (content.layoutParams as? ViewGroup.MarginLayoutParams)?.topMargin ?: 0
        val maxH = if (content.height > 0) content.height + insetTop
        else resources.displayMetrics.heightPixels
        if (maxW <= 0 || maxH <= 0) return

        if (embedFromFrame) {
            val lp = container.layoutParams as? FrameLayout.LayoutParams
            val same = lp != null && lp.width == maxW && lp.height == maxH &&
                lp.leftMargin == 0 && lp.topMargin == 0
            if (!same) {
                container.layoutParams = FrameLayout.LayoutParams(maxW, maxH).apply {
                    gravity = Gravity.TOP or Gravity.START
                    leftMargin = 0
                    topMargin = 0
                }
                logToFile("重排铺满: ${maxW}x$maxH（原 ${lp?.width}x${lp?.height}）")
            }
            // 尺寸没变也要调一次：引擎视口可能还停在旧值（旋转后 Surface 未必重建过）
            view.onContainerResized(maxW, maxH)
        } else {
            // 原位嵌入：旋转会让 WebView 重新排版，元素的 CSS 坐标已经变了，
            // 让页面按当前布局重新上报一次矩形。
            webView.evaluateJavascript(
                "window.__SXDFFLE__ && window.__SXDFFLE__.pushRect && window.__SXDFFLE__.pushRect();",
                null
            )
            view.requestLayout()
        }
    }

    /**
     * 系统内存吃紧时（Android 主动回调），让引擎跑一次 GC 并把空闲页还给内核。
     *
     * 页游挂机时内存会持续累积，系统在后台开始杀进程之前会先发这个回调 ——
     * 这时主动回收一次，往往能避免被 LMK 直接干掉（表现就是"玩着玩着闪退"）。
     */
    override fun onTrimMemory(level: Int) {
        super.onTrimMemory(level)
        if (level < android.content.ComponentCallbacks2.TRIM_MEMORY_RUNNING_LOW) return
        val v = embedView ?: return
        logToFile("系统内存紧张(level=$level)，请求引擎回收")
        v.trimMemory()
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQ_WRITE_STORAGE) return
        if (grantResults.firstOrNull() == android.content.pm.PackageManager.PERMISSION_GRANTED) {
            exportLog()
        } else {
            Toast.makeText(this, "没有存储权限，无法写入下载目录", Toast.LENGTH_LONG).show()
        }
    }

    override fun onDestroy() {
        memHandler.removeCallbacks(memWatchdog)
        hideMemChip()
        // 内嵌模式下引擎线程持有 Surface，必须在这里收尾，
        // 否则 Activity 销毁后线程仍在 present 一个已失效的窗口 → 崩溃
        closeEmbedded()
        // 导航栏与悬浮球都在独立窗口里，不主动关会 WindowLeaked
        try {
            toolbarPopup?.dismiss()
        } catch (_: Throwable) {
        }
        try {
            ballPopup?.dismiss()
        } catch (_: Throwable) {
        }
        super.onDestroy()
    }

    /** 切换「网页内嵌 / 全屏」显示方式 */
    private fun toggleEmbedMode() {
        embedMode = !embedMode
        closeEmbedded()
        showStatus(
            if (embedMode) "Flash 显示：网页内嵌（实验性）" else "Flash 显示：全屏播放"
        )
    }

    /**
     * 启动原生 Flash 引擎播放。
     *
     * 关键：把 WebView 当前的 **页面地址** 作为 Referer、
     * 把该域的 **Cookie**、以及页面的 **flashVars** 一并带给引擎，
     * 让引擎的网络请求与网页登录态一致、并拿到游戏启动参数。
     *
     * flashVars 是页游的命脉：以《神仙道》为例，它的 `ip`/`port`
     * 指明游戏 TCP 服务器为 `9x050.sxdweb.xd.com:8200`，
     * `hash_code` 则是进入游戏的凭证 —— 缺一项游戏都起不来。
     */
    private fun openWithFlashEngine(
        swfUrl: String,
        pageUrl: String,
        w: Int,
        h: Int,
        flashVars: String = ""
    ) {
        // 关键：内嵌会话必须先停掉。
        // 否则 PlayerActivity 的 GameActivity 会再起一个 Player，
        // 同进程两个引擎各自持有 EGL surface 抢占 GPU 资源，
        // 表现为白屏→错位→闪退。
        closeEmbedded()

        val referer = if (pageUrl.isNotBlank()) pageUrl else swfUrl
        val cookie = try {
            CookieManager.getInstance().getCookie(referer) ?: ""
        } catch (e: Exception) {
            ""
        }

        Log.i(TAG, "交给引擎: swf=$swfUrl")
        Log.i(TAG, "  referer=$referer")
        Log.i(TAG, "  cookie=${cookie.length} 字节")
        Log.i(TAG, "  flashVars=${flashVars.length} 字节")
        logToFile("交给引擎: swf=$swfUrl")
        logToFile("  referer=$referer")
        logToFile("  cookie=${cookie.length} 字节, flashVars=${flashVars.length} 字节")

        val intent = Intent(this, PlayerActivity::class.java).apply {
            data = Uri.parse(swfUrl)
            putExtra(PlayerActivity.EXTRA_REFERER, referer)
            putExtra(PlayerActivity.EXTRA_COOKIE, cookie)
            putExtra(PlayerActivity.EXTRA_FLASH_VARS, flashVars)
            if (w > 0 && h > 0) {
                putExtra(PlayerActivity.EXTRA_WIDTH, w)
                putExtra(PlayerActivity.EXTRA_HEIGHT, h)
            }
        }
        startActivity(intent)
    }

    private fun shorten(s: String) = if (s.length > 60) s.take(57) + "..." else s

    // ==================================================================
    //  日志：与引擎日志写同一个文件，便于一键导出
    // ==================================================================

    /** 日志文件（与 PlayerActivity 传给原生层的是同一个路径） */
    /**
     * 日志轮转：只保留最近 5 份（当前 + 4 份历史）。
     *
     * 页游会把 ExternalInterface 当轮询用，一天下来日志能到几 MB。
     * 每次启动检查一次，超过 256KB 就滚动：
     *   sxdffle.3 → .4、.2 → .3、.1 → .2、当前 → .1，最老的 .4 直接删。
     *
     * 必须放在 onCreate 最前面 —— 此时原生侧的日志句柄还没打开，
     * 改名不会让写入落到旧 inode 上。
     */
    private fun rotateLogs() {
        runCatching {
            val dir = getExternalFilesDir(null) ?: filesDir

            // ---- 版本隔离 ----
            // 轮转只按大小滚动，不看内容，所以升级后旧版本的日志会排在新日志前面
            // （实测导出包里混着前一天 20:36 的 0.261092 记录，排查前要手工过滤）。
            // 版本一变就清空全部日志；同版本内继续正常轮转，不更新时不丢历史。
            val prefs = getSharedPreferences("sxdffle_log", MODE_PRIVATE)
            val version = appVersionName()
            val lastVersion = prefs.getString("version", null)
            // 注意：lastVersion 为 null 也要清一次。
            // 0.261096 是第一个带版本隔离的版本，它启动时 prefs 还是空的，
            // 所以「升级到 0.261096」那一次不会触发清理 —— 结果旧版本日志全都留着。
            // 把 null 也当成「上一版」，第一次启用这个机制时就顺手清干净。
            if (lastVersion != version) {
                var removed = 0
                dir.listFiles()
                    ?.filter { it.isFile && it.name.startsWith("sxdffle") && it.name.endsWith(".log") }
                    ?.forEach { if (it.delete()) removed++ }
                logToFile("日志已按版本清理：${lastVersion ?: "(首次)"} -> $version，删除 $removed 份")
            }
            prefs.edit().putString("version", version).apply()

            // ---- 大小轮转 ----
            val current = File(dir, "sxdffle.log")
            if (!current.exists() || current.length() < 256 * 1024) return

            File(dir, "sxdffle.4.log").delete()
            for (i in 3 downTo 1) {
                val from = File(dir, "sxdffle.$i.log")
                if (from.exists()) from.renameTo(File(dir, "sxdffle.${i + 1}.log"))
            }
            current.renameTo(File(dir, "sxdffle.1.log"))
        }
    }

    private fun logFile(): File = File(
        getExternalFilesDir(null) ?: filesDir,
        "sxdffle.log"
    )

    /** 应用版本号（BuildConfig 在 AGP8 默认关闭，这里直接问系统） */
    // ---- 内测包更新 ----

    /**
     * 查最新构建。manual=true 时无论结果如何都给反馈；
     * 自动检查（manual=false）只在「确实有新版本」时弹窗，不打扰日常使用。
     */
    private fun checkForUpdate(manual: Boolean) {
        if (manual) showStatus("正在检查更新…")
        Thread {
            val latest = UpdateChecker.fetchLatest()
            val current = appVersionName()
            runOnUiThread {
                if (latest == null) {
                    if (manual) {
                        androidx.appcompat.app.AlertDialog.Builder(this)
                            .setTitle("检查更新失败")
                            .setMessage("无法连接更新服务器（api.github.com 与镜像都不可达）。\n\n可稍后再试，或直接在浏览器打开发布页手动下载。")
                            .setPositiveButton("知道了", null)
                            .setNeutralButton("打开发布页") { _, _ ->
                                try {
                                    startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse("https://github.com/Rosariasofari/sxdffile-mod/releases")))
                                } catch (_: Exception) {
                                }
                            }
                            .show()
                    }
                    return@runOnUiThread
                }
                if (UpdateChecker.compare(latest.version, current) <= 0) {
                    if (manual) {
                        androidx.appcompat.app.AlertDialog.Builder(this)
                            .setTitle("已是最新版本")
                            .setMessage("当前版本 $current\n远端版本 ${latest.version}")
                            .setPositiveButton("好", null)
                            .show()
                    }
                    return@runOnUiThread
                }
                androidx.appcompat.app.AlertDialog.Builder(this)
                    .setTitle("发现新版本 ${latest.version}")
                    .setMessage(
                        "当前 $current → 最新 ${latest.version}\n\n" +
                            "下载后会交给系统安装器覆盖安装，账号与配置不受影响。"
                    )
                    .setPositiveButton("下载并安装") { _, _ -> startUpdateDownload(latest) }
                    .setNeutralButton("打开发布页") { _, _ ->
                        try {
                            startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(latest.pageUrl)))
                        } catch (e: Exception) {
                            showStatus("无法打开浏览器")
                        }
                    }
                    .setNegativeButton("取消", null)
                    .show()
            }
        }.start()
    }

    private fun startUpdateDownload(r: UpdateChecker.Release) {
        if (UpdateChecker.isDownloading()) {
            showStatus("已有下载在进行中，请稍候")
            return
        }
        showStatus("开始下载 ${r.version}…")
        UpdateChecker.download(
            this,
            r,
            onProgress = { pct -> showStatus("下载中 $pct%") },
            onDone = { file, err ->
                if (file != null) {
                    if (!UpdateChecker.canInstall(this)) {
                        androidx.appcompat.app.AlertDialog.Builder(this)
                            .setTitle("需要安装权限")
                            .setMessage("Android 8 以上要求单独授权「安装未知应用」。\n\n点『去授权』打开系统页面，允许本应用安装后返回，再点一次『检查更新 → 下载并安装』即可。")
                            .setPositiveButton("去授权") { _, _ -> UpdateChecker.requestInstallPermission(this) }
                            .setNegativeButton("取消", null)
                            .show()
                    } else {
                        showStatus("下载完成，请在系统安装界面确认")
                        try {
                            UpdateChecker.install(this, file)
                        } catch (e: Exception) {
                            showStatus("调起安装失败：${e.message}")
                        }
                    }
                } else {
                    showStatus("下载失败：$err")
                }
            }
        )
    }

    private fun appVersionName(): String = try {
        packageManager.getPackageInfo(packageName, 0).versionName ?: "?"
    } catch (e: Exception) {
        "?"
    }

    /**
     * flash.cn 这类站点把登录票据写在 `document.cookie`（域 `.flash.cn`），
     * 登录页 reload 后再读。读不到就再弹一次登录页 —— 表现是"选完区又跳回登录页"。
     *
     * 这里只记 Cookie 的**名字**（不记值），下一次日志就能判断：
     * 到底是票据没写进去，还是写进去了但读不到。
     */
    private fun logCookiesIfNeeded(url: String) {
        val host = runCatching { Uri.parse(url).host ?: "" }.getOrDefault("")
        if (host.isEmpty() || !host.endsWith("flash.cn")) return
        val raw = runCatching { CookieManager.getInstance().getCookie(url) }.getOrNull() ?: ""
        val names = raw.split(';').mapNotNull { p ->
            p.substringBefore('=').trim().takeIf { it.isNotEmpty() }
        }
        val key = listOf("flashTokenyyds", "sitePlay2023").filter { it in names }
        logToFile("Cookie[$host] 共 ${names.size} 个，登录票据=${if (key.isEmpty()) "无" else key.joinToString()}")
    }

    /**
     * 记录一次 Java 堆占用。
     *
     * 压测时连续切几十个平台，每次都会新建一整局引擎；只要有一次没释放干净，
     * 内存就会一路涨到系统把进程杀掉（表现是"无崩溃日志的闪退/手机变卡重启"）。
     * 把占用写进日志，下一次就能看到是"每次 +100MB"还是"稳定"。
     */
    private fun logMemory(tag: String) {
        val rt = Runtime.getRuntime()
        val used = (rt.totalMemory() - rt.freeMemory()) / (1024 * 1024)
        val heap = rt.totalMemory() / (1024 * 1024)
        val max = rt.maxMemory() / (1024 * 1024)
        // Ruffle 的纹理/字体都在 native 堆，Java 堆看不出来，补一行 RSS
        val rss = runCatching {
            java.io.File("/proc/self/status").useLines { lines ->
                lines.firstOrNull { it.startsWith("VmRSS:") }
                    ?.substringAfter("VmRSS:")?.trim()
            }
        }.getOrNull() ?: "-"
        logToFile("内存[$tag]: Java ${used}/${heap}MB（上限 ${max}MB）· RSS $rss")
    }

    /**
     * flash.cn 的"选完区又跳回登录页"其实是它自己在循环：
     * 游戏页发现登录票据无效 → 载入登录 iframe → 登录 iframe 又 `top.location.reload()`。
     *
     * 票据无效这种事我们修不了（服务端校验），但可以在日志和界面上说清楚，
     * 免得用户以为是浏览器坏了、反复重试。
     */
    private fun checkFlashLoginLoop(url: String?) {
        if (url == null) return
        val uri = runCatching { Uri.parse(url) }.getOrNull() ?: return
        val host = uri.host ?: return
        if (!host.endsWith("flash.cn") || !uri.path.orEmpty().contains("/game/play/")) return

        val now = android.os.SystemClock.elapsedRealtime()
        while (flashPlayTimes.isNotEmpty() && now - flashPlayTimes.first() > 30_000) {
            flashPlayTimes.removeFirst()
        }
        flashPlayTimes.addLast(now)
        if (flashPlayTimes.size < 3) return
        if (!flashLoopHinted) {
            flashLoopHinted = true
            logToFile("flash.cn 游戏页 30 秒内进入 ${flashPlayTimes.size} 次：登录票据被服务端拒绝")
            showStatus("flash.cn 登录已失效，请点右上角菜单 → 收藏夹旁的地址栏重新登录")
        }
    }

    /**
     * 识别"提升后被弹回"。
     *
     * 51 的 webgameapi iframe 提到顶层后，发现上下文不对又自己跳回外层，
     * 外层再提升 → 每秒一轮的乒乓。以前是按"同域"一刀切禁止提升，
     * 但那会把 flash.cn 的登录 bridge 也一起禁掉（它是同域 iframe，
     * 必须提到顶层才能正常登录）。
     *
     * 改成自适应：提升后 5 秒内又回到了原页面，就认定这个 host 会回弹，
     * 本次进程内不再提升它 —— 其它站点不受影响。
     */
    private fun checkPromoteBounce(url: String?) {
        val from = promoteFromUrl ?: return
        val target = promoteTargetUrl ?: return
        if (android.os.SystemClock.elapsedRealtime() - promoteAtMs > 5000) {
            promoteFromUrl = null
            promoteTargetUrl = null
            return
        }
        if (!samePage(url, from)) return

        val host = runCatching { Uri.parse(target).host ?: "" }.getOrDefault("")
        if (host.isNotEmpty()) promoteBannedHosts.add(host)
        promoteFromUrl = null
        promoteTargetUrl = null
        logToFile("提升回弹：$host 在 5 秒内被弹回，本次运行不再提升该域")
    }

    private fun samePage(a: String?, b: String?): Boolean {
        if (a == null || b == null) return false
        val ua = runCatching { Uri.parse(a) }.getOrNull() ?: return false
        val ub = runCatching { Uri.parse(b) }.getOrNull() ?: return false
        return ua.host == ub.host && ua.path == ub.path
    }

    /** 追加一行浏览器侧日志（引擎侧日志由 Rust 写入同一文件） */
    private fun logToFile(msg: String) {
        try {
            val ts = SimpleDateFormat("MM-dd HH:mm:ss.SSS", Locale.US).format(Date())
            logFile().appendText("$ts [UI] $msg\n")
        } catch (e: Exception) {
            Log.w(TAG, "写日志失败", e)
        }
    }

    /** 导出日志：通过系统的分享面板发给任意应用 */
    /**
     * 导出日志：把当前 + 4 份历史打包成一个 zip，存到手机的「下载」目录。
     *
     * 之前只发当前那一份，压测崩溃的记录会被轮转滚到 .1~.4 里，发出来就断档。
     * 现在整包给出，且落在公共下载目录 —— 不需要任何第三方 App 中转，
     * 文件管理器 / 微信 / QQ 都能直接找到。
     */
    private fun exportLog() {
        val dir = logFile().parentFile ?: filesDir
        val logs = ArrayList<File>()
        File(dir, "sxdffle.log").takeIf { it.exists() && it.length() > 0 }?.let { logs.add(it) }
        for (i in 1..4) {
            File(dir, "sxdffle.$i.log").takeIf { it.exists() && it.length() > 0 }?.let { logs.add(it) }
        }
        if (logs.isEmpty()) {
            Toast.makeText(this, "还没有日志，请先在游戏里操作一会儿", Toast.LENGTH_LONG).show()
            return
        }

        // Android 8/9 写公共下载目录要运行时权限；10 起走 MediaStore，不需要
        if (android.os.Build.VERSION.SDK_INT < android.os.Build.VERSION_CODES.Q &&
            checkSelfPermission(android.Manifest.permission.WRITE_EXTERNAL_STORAGE) !=
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(
                arrayOf(android.Manifest.permission.WRITE_EXTERNAL_STORAGE),
                REQ_WRITE_STORAGE
            )
            return
        }

        val totalBytes = logs.sumOf { it.length() }
        logToFile("用户导出日志：${logs.size} 份，共 $totalBytes 字节")

        val stamp = java.text.SimpleDateFormat("yyyyMMdd_HHmmss", java.util.Locale.US)
            .format(java.util.Date())
        val name = "sxdffle-log-$stamp.zip"
        val tmp = File(cacheDir, name)

        val saved = try {
            java.util.zip.ZipOutputStream(
                java.io.BufferedOutputStream(tmp.outputStream())
            ).use { zos ->
                logs.forEach { f ->
                    zos.putNextEntry(java.util.zip.ZipEntry(f.name))
                    f.inputStream().use { it.copyTo(zos) }
                    zos.closeEntry()
                }
            }
            saveToDownloads(name, tmp)
        } catch (e: Exception) {
            Log.e(TAG, "导出日志失败", e)
            Toast.makeText(this, "导出失败：${e.message}", Toast.LENGTH_LONG).show()
            return
        }

        logToFile("日志已导出：$saved")
        androidx.appcompat.app.AlertDialog.Builder(this)
            .setTitle("日志已导出")
            .setMessage("$saved\n\n共 ${logs.size} 份，${totalBytes / 1024} KB。")
            .setPositiveButton("分享") { _, _ -> shareFile(tmp, "application/zip") }
            .setNegativeButton("知道了", null)
            .show()
    }

    /**
     * 把文件写进公共「下载」目录，返回给人看的路径。
     *
     * Q 及以上用 MediaStore（不需要任何权限，卸载 App 也不会删）；
     * 8/9 直接写外部公共目录并通知媒体库扫描。
     */
    private fun saveToDownloads(name: String, src: File): String {
        if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.Q) {
            val values = android.content.ContentValues().apply {
                put(android.provider.MediaStore.Downloads.DISPLAY_NAME, name)
                put(android.provider.MediaStore.Downloads.MIME_TYPE, "application/zip")
                put(
                    android.provider.MediaStore.Downloads.RELATIVE_PATH,
                    android.os.Environment.DIRECTORY_DOWNLOADS
                )
                put(android.provider.MediaStore.Downloads.IS_PENDING, 1)
            }
            val uri = contentResolver.insert(
                android.provider.MediaStore.Downloads.EXTERNAL_CONTENT_URI, values
            ) ?: throw java.io.IOException("无法在下载目录创建文件")

            try {
                contentResolver.openOutputStream(uri)?.use { out ->
                    src.inputStream().use { it.copyTo(out) }
                } ?: throw java.io.IOException("无法写入下载目录")
                values.clear()
                values.put(android.provider.MediaStore.Downloads.IS_PENDING, 0)
                contentResolver.update(uri, values, null, null)
            } catch (e: Exception) {
                // 写失败别留一个 0 字节的坏条目在下载列表里
                runCatching { contentResolver.delete(uri, null, null) }
                throw e
            }
            return "下载/$name"
        }

        val dir = android.os.Environment.getExternalStoragePublicDirectory(
            android.os.Environment.DIRECTORY_DOWNLOADS
        )
        if (!dir.exists()) dir.mkdirs()
        val out = File(dir, name)
        src.inputStream().use { input -> out.outputStream().use { input.copyTo(it) } }
        android.media.MediaScannerConnection.scanFile(
            this, arrayOf(out.absolutePath), arrayOf("application/zip"), null
        )
        return out.absolutePath
    }

    /** 分享已生成的 zip（走 FileProvider，其它 App 才有读权限） */
    private fun shareFile(f: File, mime: String) {
        runCatching {
            val uri = FileProvider.getUriForFile(this, "$packageName.fileprovider", f)
            val send = Intent(Intent.ACTION_SEND).apply {
                type = mime
                putExtra(Intent.EXTRA_STREAM, uri)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            startActivity(Intent.createChooser(send, "分享日志"))
        }.onFailure {
            Toast.makeText(this, "分享失败：${it.message}", Toast.LENGTH_LONG).show()
        }
    }

    /**
     * 状态提示改为 Toast（独立窗口）。
     *
     * 之前是布局里的一个 TextView：它一显示就会把 contentArea 压矮一段，
     * 内嵌的 Flash 视图跟着重新测量，于是**游戏画面会随着每条提示抖动/缩放**。
     * Toast 走系统窗口，完全不参与布局，画布尺寸恒定。
     */
    private var statusToast: Toast? = null

    private fun showStatus(msg: String) {
        Log.i(TAG, "状态: $msg")
        try {
            val t = statusToast ?: Toast.makeText(this, "", Toast.LENGTH_SHORT).also { statusToast = it }
            // 复用同一个实例：show() 会重置计时，不会自己排队
            t.setText(msg)
            t.show()
        } catch (_: Throwable) {
            // Activity 已销毁时 show 会抛，忽略即可
        }
    }


    companion object {
        private const val TAG = "SXDffle"

        /** 默认首页（用户没自定义时打开这个） */
        private const val DEFAULT_HOME = "https://sxd.xd.com"

        /** 最多保留多少条浏览记录 */
        private const val HISTORY_MAX = 100

        /** SharedPreferences 里的两个 JSON 数组 */
        private const val KEY_HISTORY = "history"
        private const val KEY_BOOKMARKS = "bookmarks"

        /** 内置收藏夹是否已导入（只导一次，之后完全由用户支配） */
        private const val KEY_BM_IMPORTED = "bm_default_imported_v1"

        /** 收藏夹删除时是否弹确认 */
        private const val KEY_BM_CONFIRM = "bm_confirm_delete"

        /** 重启进程时带过来的目标地址 */
        private const val EXTRA_OPEN_URL = "open_url"

        /** 写公共下载目录的运行时权限请求码（仅 Android 8/9 需要） */
        private const val REQ_WRITE_STORAGE = 1002

        /**
         * 登录域强制走 HTTPS。
         *
         * 只放登录 / OAuth 这几个域：游戏服务器（s745.sxd.yy.com 之类）大多只有 http，
         * 升级了反而进不去。
         */
        /**
         * 该主机是否要强制走 HTTPS。
         *
         * 只做**精确匹配**：flash.cn 下还有 open./passport. 这类只有 80 端口的登录域，
         * 一刀切按后缀升级会把登录页直接打死（连接被拒）。
         */
        private fun needHttps(host: String): Boolean = host in FORCE_HTTPS_HOSTS

        private val FORCE_HTTPS_HOSTS = setOf(
            // YY：OAuth 回调写的 Secure Cookie 必须由 https 页面读取
            "udblogin.yy.com",
            "lgn.yy.com",
            "login.yy.com",
            // Flash 中心：play-game 是 http 页面，而登录票据由 https 侧写，
            // 混用会让它反复判定"未登录"→ 弹回登录页
            "game.flash.cn",
            "play-game.flash.cn",
            "www.flash.cn"
        )
    }
}
