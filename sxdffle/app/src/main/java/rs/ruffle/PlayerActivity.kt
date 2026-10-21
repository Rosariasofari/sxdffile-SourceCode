package rs.ruffle

import android.annotation.SuppressLint
import android.content.Intent
import android.content.res.Configuration
import android.net.Uri
import android.os.Build
import android.os.Build.VERSION_CODES
import android.os.Bundle
import android.util.Log
import android.view.Menu
import android.view.MenuItem
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.Window
import android.view.WindowManager
import android.widget.Button
import android.widget.PopupMenu
import android.widget.TextView
import androidx.constraintlayout.widget.ConstraintLayout
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.google.androidgamesdk.GameActivity
import java.io.DataInputStream
import java.io.File
import java.io.IOException

class PlayerActivity : GameActivity() {
    @Suppress("unused")
    // Used by Rust
    private val swfBytes: ByteArray?
        get() {
            val uri = intent.data
            if (uri?.scheme == "content") {
                try {
                    contentResolver.openInputStream(uri).use { inputStream ->
                        if (inputStream == null) {
                            return null
                        }
                        val bytes = ByteArray(inputStream.available())
                        val dataInputStream = DataInputStream(inputStream)
                        dataInputStream.readFully(bytes)
                        return bytes
                    }
                } catch (ignored: IOException) {
                }
            }
            return null
        }

    @Suppress("unused")
    // Used by Rust
    private val swfUri: String?
        get() {
            return intent.dataString
        }

    @Suppress("unused")
    // Used by Rust
    private val traceOutput: String?
        get() {
            return intent.getStringExtra("traceOutput")
        }

    /**
     * SXDffle：把发起播放的 **网页地址** 作为 Referer 交给原生引擎。
     * 页游服务器普遍用 Referer 做防盗链校验，缺失会导致 SWF 与资源 403。
     */
    @Suppress("unused")
    // Used by Rust
    private val referer: String?
        get() {
            return intent.getStringExtra(EXTRA_REFERER)
        }

    /**
     * SXDffle：把 WebView 在该域下的 **Cookie** 交给原生引擎，
     * 使引擎发出的 HTTP 请求与网页登录态保持一致
     * （这是「先在网页登录、再进入游戏」链路能打通的关键）。
     */
    @Suppress("unused")
    // Used by Rust
    private val cookie: String?
        get() {
            return intent.getStringExtra(EXTRA_COOKIE)
        }

    /**
     * SXDffle：页面的 flashVars，格式为 `k1=v1&k2=v2`。
     *
     * 页游的全部启动参数都走这里（《神仙道》有 170+ 项，其中 `ip`/`port`
     * 是游戏 TCP 服务器地址与端口）。引擎侧会做 URL 解码后交给
     * LoaderInfo.parameters，等价于真实 Flash Player 的行为。
     */
    @Suppress("unused")
    // Used by Rust
    private val flashVars: String?
        get() {
            return intent.getStringExtra(EXTRA_FLASH_VARS)
        }

    @Suppress("unused")
    // Used by Rust
    private fun navigateToUrl(url: String?) {
        startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
    }

    private var loc = IntArray(2)

    @Suppress("unused")
    // Handle of an EventLoopProxy over in rust-land
    private val eventLoopHandle: Long = 0

    @Suppress("unused")
    // Used by Rust
    private val locInWindow: IntArray
        get() {
            mSurfaceView.getLocationInWindow(loc)
            return loc
        }

    @Suppress("unused")
    // Used by Rust
    private val surfaceWidth: Int
        get() = mSurfaceView.width

    @Suppress("unused")
    // Used by Rust
    private val surfaceHeight: Int
        get() = mSurfaceView.height

    private external fun keydown(keyTag: String)
    private external fun keyup(keyTag: String)
    private external fun requestContextMenu()
    private external fun runContextMenuCallback(index: Int)
    private external fun clearContextMenu()

    @Suppress("unused")
    // Used by Rust
    private fun showContextMenu(items: Array<String>) {
        runOnUiThread {
            val popup = PopupMenu(this, findViewById(R.id.button_cm))
            val menu = popup.menu
            if (Build.VERSION.SDK_INT >= VERSION_CODES.P) {
                menu.setGroupDividerEnabled(true)
            }
            var group = 1
            for (i in items.indices) {
                val elements = items[i].split(" ".toRegex(), limit = 4).toTypedArray()
                val enabled = elements[0].toBoolean()
                val separatorBefore = elements[1].toBoolean()
                val checked = elements[2].toBoolean()
                val caption = elements[3]
                if (separatorBefore) group += 1
                val item = menu.add(group, i, Menu.NONE, caption)
                item.setEnabled(enabled)
                if (checked) {
                    item.setCheckable(true)
                    item.setChecked(true)
                }
            }
            val exitItemId: Int = items.size
            menu.add(group, exitItemId, Menu.NONE, "Exit")
            popup.setOnMenuItemClickListener { item: MenuItem ->
                if (item.itemId == exitItemId) {
                    finish()
                } else {
                    runContextMenuCallback(item.itemId)
                }
                true
            }
            popup.setOnDismissListener { clearContextMenu() }
            popup.show()
        }
    }

    @Suppress("unused")
    // Used by Rust
    private fun getAndroidDataStorageDir(): String {
        // TODO It can also be placed in an external storage path in the future to share archived content
        val storageDirPath = "${filesDir.absolutePath}/ruffle/shared_objects"
        val storageDir = File(storageDirPath)
        if (!storageDir.exists()) {
            storageDir.mkdirs()
        }
        return storageDirPath
    }

    override fun onCreateSurfaceView() {
        val inflater = layoutInflater

        @SuppressLint("InflateParams")
        val layout = inflater.inflate(R.layout.keyboard, null) as ConstraintLayout

        contentViewId = View.generateViewId()
        layout.id = contentViewId
        setContentView(layout)
        mSurfaceView = InputEnabledSurfaceView(this)

        mSurfaceView.contentDescription = "Ruffle Player"

        val placeholder = findViewById<View>(R.id.placeholder)
        val pars = placeholder.layoutParams as ConstraintLayout.LayoutParams
        val parent = placeholder.parent as ViewGroup
        val index = parent.indexOfChild(placeholder)
        parent.removeView(placeholder)
        parent.addView(mSurfaceView, index)
        mSurfaceView.setLayoutParams(pars)
        val keys = gatherAllDescendantsOfType<Button>(
            layout.getViewById(R.id.keyboard),
            Button::class.java
        )
        for (b in keys) {
            b.setOnTouchListener { view: View, motionEvent: MotionEvent ->
                val tag = view.tag as String
                if (motionEvent.action == MotionEvent.ACTION_DOWN) keydown(tag)
                if (motionEvent.action == MotionEvent.ACTION_UP) keyup(tag)
                view.performClick()
                false
            }
        }
        layout.findViewById<View>(R.id.button_kb).setOnClickListener {
            val keyboard = layout.getViewById(R.id.keyboard)
            if (keyboard.visibility == View.VISIBLE) {
                keyboard.visibility = View.GONE
            } else {
                keyboard.visibility = View.VISIBLE
            }
        }
        layout.findViewById<View>(R.id.button_cm)
            .setOnClickListener { requestContextMenu() }
        layout.requestLayout()
        layout.requestFocus()
        mSurfaceView.holder.addCallback(this)
        ViewCompat.setOnApplyWindowInsetsListener(mSurfaceView, this)

        // SXDffle：在原生渲染层之上叠加一条半透明返回栏。
        // 游戏是全屏渲染的，若不提供退路，用户会以为"浏览器 UI 被吞掉了"。
        addTopOverlayBar()
    }

    /**
     * 顶部悬浮栏：返回浏览器。
     *
     * 用 `addContentView` 叠加而不用改原布局，避免影响 GameActivity 的
     * SurfaceView 初始化流程。返回时 BrowserActivity 仍在后台，
     * 网页与登录态原样保留。
     */
    private fun addTopOverlayBar() {
        val bar = layoutInflater.inflate(R.layout.player_overlay_bar, null)
        addContentView(
            bar,
            ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            )
        )

        bar.findViewById<View>(R.id.btnBackToBrowser).setOnClickListener {
            finish()
        }

        val title = intent.dataString
            ?.substringAfterLast('/')
            ?.substringBefore('?')
            ?.takeIf { it.isNotBlank() }
            ?: "Flash 引擎"
        bar.findViewById<TextView>(R.id.playerTitle).text = title

        // 全屏切换暂隐藏：当前已是沉浸式全屏，避免功能重复
        bar.findViewById<View>(R.id.btnPlayerFullscreen).visibility = View.GONE
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        val keyboard = findViewById<View>(R.id.keyboard)
        val isLandscape = newConfig.orientation == Configuration.ORIENTATION_LANDSCAPE
        keyboard.visibility = if (isLandscape) View.GONE else View.VISIBLE
    }

    private fun hideSystemUI() {
        // This will put the game behind any cutouts and waterfalls on devices which have
        // them, so the corresponding insets will be non-zero.
        if (Build.VERSION.SDK_INT >= VERSION_CODES.R) {
            window.attributes.layoutInDisplayCutoutMode =
                WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }
        // From API 30 onwards, this is the recommended way to hide the system UI, rather than
        // using View.setSystemUiVisibility.
        val decorView = window.decorView
        val controller = WindowInsetsControllerCompat(
            window,
            decorView
        )
        controller.hide(WindowInsetsCompat.Type.systemBars())
        controller.hide(WindowInsetsCompat.Type.displayCutout())
        controller.systemBarsBehavior =
            WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        // SXDffle：把日志文件路径交给原生层，使引擎日志能与 logcat 双写。
        // 放在应用外部目录（Android/data/<包名>/files/），便于导出。
        val logPath = File(
            getExternalFilesDir(null) ?: filesDir,
            "sxdffle.log"
        ).absolutePath

        nativeInit({ message ->
            Log.e("ruffle", "Handling panic: $message")
            startActivity(
                Intent(this, PanicActivity::class.java).apply {
                    putExtra("message", message)
                }
            )
        }, logPath)
        // When true, the app will fit inside any system UI windows.
        // When false, we render behind any system UI windows.
        WindowCompat.setDecorFitsSystemWindows(window, false)
        hideSystemUI()
        // You can set IME fields here or in native code using GameActivity_setImeEditorInfoFields.
        // We set the fields in native_engine.cpp.
        // super.setImeEditorInfoFields(InputType.TYPE_CLASS_TEXT,
        //     IME_ACTION_NONE, IME_FLAG_NO_FULLSCREEN );
        requestNoStatusBarFeature()
        supportActionBar?.hide()
        super.onCreate(savedInstanceState)
    }

    // Used by Rust
    @Suppress("unused")
    val isGooglePlayGames: Boolean
        get() {
            val pm = packageManager
            return pm.hasSystemFeature("com.google.android.play.feature.HPE_EXPERIENCE")
        }

    private fun requestNoStatusBarFeature() {
        // Hiding the status bar this way makes it see through when pulled down
        requestWindowFeature(Window.FEATURE_NO_TITLE)
        WindowInsetsControllerCompat(
            window,
            mSurfaceView
        ).hide(WindowInsetsCompat.Type.statusBars())
    }

    companion object {
        /** SXDffle：由浏览器外壳传入的上下文参数 */
        const val EXTRA_REFERER = "sxdffle.referer"
        const val EXTRA_COOKIE = "sxdffle.cookie"
        const val EXTRA_WIDTH = "sxdffle.width"
        const val EXTRA_HEIGHT = "sxdffle.height"
        /** SXDffle：页面的 flashVars（k=v&k=v 形式） */
        const val EXTRA_FLASH_VARS = "sxdffle.flashvars"

        init {
            // load the native activity (SXDffle 引擎: libsxdffle.so)
            System.loadLibrary("sxdffle")
        }

        @JvmStatic
        private external fun nativeInit(crashCallback: CrashCallback, logPath: String)

        private fun <T> gatherAllDescendantsOfType(v: View, t: Class<*>): List<T> {
            val result: MutableList<T> = ArrayList()
            @Suppress("UNCHECKED_CAST")
            if (t.isInstance(v)) result.add(v as T)
            if (v is ViewGroup) {
                for (i in 0 until v.childCount) {
                    result.addAll(gatherAllDescendantsOfType(v.getChildAt(i), t))
                }
            }
            return result
        }
    }

    fun interface CrashCallback {
        fun onCrash(message: String)
    }
}
