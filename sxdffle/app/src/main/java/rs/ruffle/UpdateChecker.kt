package rs.ruffle

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.os.Looper
import androidx.core.content.FileProvider
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL

/**
 * SXDffle：内测包更新通道。
 *
 * 只做三件事：查 GitHub Releases 上的最新 tag、下载对应 APK、交给系统安装器。
 *
 * 为什么要做「备用镜像」：api.github.com 与 github.com 在部分网络下会超时
 * （本项目发布时也反复遇到 TLS 被掐），所以下载链接会依次尝试
 * 直连 → ghfast.top → ghproxy.net → gh-proxy.com 这几个公益加速前缀。
 * 版本号本身优先走 API；API 也不通时退化为「抓 releases/latest 页面的跳转地址」。
 */
object UpdateChecker {

    private const val REPO = "Rosariasofari/sxdffile-mod"
    private const val API = "https://api.github.com/repos/$REPO/releases/latest"
    private const val LATEST_PAGE = "https://github.com/$REPO/releases/latest"

    /** 下载镜像前缀（拼在原 URL 前面即可）。直连失败后按顺序尝试。 */
    private val MIRRORS = listOf(
        "",
        "https://ghfast.top/",
        "https://ghproxy.net/",
        "https://gh-proxy.com/"
    )

    data class Release(
        val version: String,
        val assetUrl: String,
        val pageUrl: String = LATEST_PAGE
    )

    private fun open(url: String, timeoutMs: Int = 8000): HttpURLConnection {
        val c = URL(url).openConnection() as HttpURLConnection
        c.connectTimeout = timeoutMs
        c.readTimeout = timeoutMs
        c.instanceFollowRedirects = true
        c.setRequestProperty("User-Agent", "SXDffle-mod-updater")
        c.setRequestProperty("Accept", "application/vnd.github+json")
        return c
    }

    /** 查最新版本；网络不通返回 null（调用方负责提示）。 */
    fun fetchLatest(): Release? {
        // 1) 官方 API
        for (prefix in listOf("", "https://ghfast.top/", "https://ghproxy.net/")) {
            try {
                val c = open(prefix + API)
                if (c.responseCode in 200..299) {
                    val body = c.inputStream.bufferedReader().use { it.readText() }
                    val o = JSONObject(body)
                    val tag = o.optString("tag_name").removePrefix("v")
                    val assets = o.optJSONArray("assets")
                    var url = ""
                    if (assets != null && assets.length() > 0) {
                        url = assets.getJSONObject(0).optString("browser_download_url")
                    }
                    if (tag.isNotEmpty() && url.isNotEmpty()) {
                        return Release(tag, url, o.optString("html_url", LATEST_PAGE))
                    }
                }
                c.disconnect()
            } catch (_: Exception) {
                // 换下一个源
            }
        }
        // 2) API 全不通：抓 releases/latest 的跳转，从 /tag/vX 里取版本号
        for (prefix in listOf("", "https://ghfast.top/", "https://ghproxy.net/")) {
            try {
                val c = open(prefix + LATEST_PAGE)
                val finalUrl = c.url.toString()
                c.disconnect()
                val idx = finalUrl.indexOf("/tag/")
                if (idx > 0) {
                    val tag = finalUrl.substring(idx + 5).trimEnd('/').removePrefix("v")
                    val url =
                        "https://github.com/$REPO/releases/download/v$tag/sxdffle-$tag-mod-arm64.apk"
                    if (tag.isNotEmpty()) return Release(tag, url)
                }
            } catch (_: Exception) {
            }
        }
        return null
    }

    /** 版本号比较：a>b 返回正数。只比较数字段，忽略 -mod 之类的后缀。 */
    fun compare(a: String, b: String): Int {
        fun parts(s: String) = s.substringBefore('-').split('.').map { it.toIntOrNull() ?: 0 }
        val x = parts(a)
        val y = parts(b)
        for (i in 0 until maxOf(x.size, y.size)) {
            val d = (x.getOrNull(i) ?: 0) - (y.getOrNull(i) ?: 0)
            if (d != 0) return d
        }
        return 0
    }

    /**
     * 下载 APK（后台线程），完成后回主线程回调本地文件。
     * 每个镜像都失败才返回错误字符串。
     */
    fun download(
        activity: Activity,
        release: Release,
        onProgress: (Int) -> Unit,
        onDone: (File?, String?) -> Unit
    ) {
        val main = Handler(Looper.getMainLooper())
        Thread {
            val dest = File(activity.cacheDir, "update-${release.version}.apk")
            if (dest.exists() && dest.length() > 1024 * 1024) {
                main.post { onDone(dest, null) }
                return@Thread
            }
            var lastErr = "未知错误"
            for (prefix in MIRRORS) {
                try {
                    val url = if (prefix.isEmpty()) release.assetUrl else prefix + release.assetUrl
                    val c = open(url, 15000)
                    if (c.responseCode !in 200..299) {
                        lastErr = "HTTP ${c.responseCode}"
                        c.disconnect()
                        continue
                    }
                    val total = c.contentLengthLong
                    c.inputStream.use { input ->
                        dest.outputStream().use { out ->
                            val buf = ByteArray(64 * 1024)
                            var got = 0L
                            var lastPct = -1
                            while (true) {
                                val n = input.read(buf)
                                if (n < 0) break
                                out.write(buf, 0, n)
                                got += n
                                if (total > 0) {
                                    val pct = (got * 100 / total).toInt()
                                    if (pct != lastPct) {
                                        lastPct = pct
                                        main.post { onProgress(pct) }
                                    }
                                }
                            }
                        }
                    }
                    c.disconnect()
                    if (dest.length() > 1024 * 1024) {
                        main.post { onDone(dest, null) }
                        return@Thread
                    }
                    lastErr = "文件不完整"
                } catch (e: Exception) {
                    lastErr = e.message ?: e.javaClass.simpleName
                    if (dest.exists()) dest.delete()
                }
            }
            main.post { onDone(null, lastErr) }
        }.start()
    }

    /** 拉起系统安装器。 */
    fun install(activity: Activity, apk: File) {
        val uri: Uri = FileProvider.getUriForFile(
            activity,
            activity.packageName + ".fileprovider",
            apk
        )
        val i = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, "application/vnd.android.package-archive")
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        activity.startActivity(i)
    }
}
