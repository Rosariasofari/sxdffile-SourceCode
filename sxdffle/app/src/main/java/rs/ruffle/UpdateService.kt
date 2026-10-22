package rs.ruffle

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.FileProvider
import java.io.File
import java.net.HttpURLConnection
import java.net.URL

/**
 * SXDffle：用前台服务承载 APK 下载。
 *
 * 为什么必须用 Service：下载跑在 Activity 的普通线程里时，APP 一进后台
 * 就被 Android 12+ 的 cached-process freezer 冻住，socket 直接断。
 * 前台服务（dataSync 类型）会给进程一个 foreground 身份，切后台/锁屏都能继续。
 *
 * 通知是前台服务的必要载体，顺带就成了下载进度条 —— 这两件事本来就是一件。
 */
class UpdateService : Service() {

    companion object {
        const val CHANNEL_ID = "sxdffle_update"
        const val NOTI_ID = 0x5DD
        const val ACTION_START = "rs.ruffle.UPDATE_START"
        const val EXTRA_URL = "url"
        const val EXTRA_VERSION = "version"

        /** 下载镜像前缀，与 UpdateChecker 保持一致。 */
        private val MIRRORS = listOf(
            "https://gh-proxy.com/",
            "https://ghfast.top/",
            "https://ghproxy.net/",
            ""
        )
    }

    @Volatile
    private var running = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val url = intent?.getStringExtra(EXTRA_URL)
        val version = intent?.getStringExtra(EXTRA_VERSION) ?: ""
        if (url.isNullOrEmpty() || running) {
            stopSelf()
            return START_NOT_STICKY
        }
        ensureChannel()
        startForeground(NOTI_ID, buildNotification(version, 0, "准备下载…", true))
        UpdateChecker.log(this, "前台服务启动 · 版本 $version · 通知已挂出")
        running = true
        Thread { download(url, version) }.start()
        return START_NOT_STICKY
    }

    private fun ensureChannel() {
        if (Build.VERSION.SDK_INT >= 26) {
            val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            if (nm.getNotificationChannel(CHANNEL_ID) == null) {
                val ch = NotificationChannel(
                    CHANNEL_ID, "应用更新", NotificationManager.IMPORTANCE_LOW
                ).apply {
                    description = "内测包下载进度"
                    setShowBadge(false)
                }
                nm.createNotificationChannel(ch)
            }
        }
    }

    private fun buildNotification(
        version: String,
        pct: Int,
        text: String,
        indeterminate: Boolean,
        apk: File? = null
    ): Notification {
        val b = NotificationCompat.Builder(this, CHANNEL_ID)
            // 必须用本包资源：android.R.drawable 的 ID 在本包里解析不到，
            // 轻则图标空白，重则整条通知不显示。
            .setSmallIcon(R.drawable.ic_stat_download)
            .setContentTitle("SXDffle 更新 $version")
            .setContentText(text)
            .setOngoing(apk == null)
            .setOnlyAlertOnce(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)

        if (apk == null) {
            if (indeterminate) b.setProgress(0, 0, true) else b.setProgress(100, pct, false)
        } else {
            // 下载完成：点击直接拉起安装器
            val uri: Uri = FileProvider.getUriForFile(this, packageName + ".fileprovider", apk)
            val i = Intent(Intent.ACTION_VIEW).apply {
                setDataAndType(uri, "application/vnd.android.package-archive")
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }
            val pi = PendingIntent.getActivity(
                this, 1, i,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            )
            b.setContentIntent(pi).setAutoCancel(true).setOngoing(false)
        }
        return b.build()
    }

    private fun notify(n: Notification) {
        try {
            val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            nm.notify(NOTI_ID, n)
        } catch (_: Exception) {
        }
    }

    private fun download(rawUrl: String, version: String) {
        val dest = File(cacheDir, "update-$version.apk")
        val part = File(cacheDir, "update-$version.apk.part")
        if (part.exists()) part.delete()

        var lastErr = "未知错误"
        for (prefix in MIRRORS) {
            try {
                val u = if (prefix.isEmpty()) rawUrl else prefix + rawUrl
                val c = (URL(u).openConnection() as HttpURLConnection).apply {
                    connectTimeout = 15000
                    readTimeout = 15000
                    instanceFollowRedirects = true
                    setRequestProperty("User-Agent", "SXDffle-mod-updater")
                }
                if (c.responseCode !in 200..299) {
                    lastErr = "HTTP ${c.responseCode}"
                    UpdateChecker.log(this, "镜像 ${prefix.ifEmpty { "直连" }} 返回 $lastErr")
                    c.disconnect()
                    continue
                }
                val total = c.contentLengthLong
                if (part.exists()) part.delete()
                c.inputStream.use { input ->
                    part.outputStream().use { out ->
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
                                    notify(
                                        buildNotification(
                                            version, pct,
                                            "${got / 1024 / 1024}MB / ${total / 1024 / 1024}MB",
                                            false
                                        )
                                    )
                                }
                            }
                        }
                    }
                }
                c.disconnect()
                val ok = part.length() > 1024 * 1024 && (total <= 0 || part.length() == total)
                if (ok) {
                    if (dest.exists()) dest.delete()
                    part.renameTo(dest)
                    notify(buildNotification(version, 100, "下载完成，点击安装", false, dest))
                    UpdateChecker.log(this, "下载完成 ${dest.length() / 1024 / 1024}MB → ${dest.name}")
                    running = false
                    stopForeground(false)
                    stopSelf()
                    return
                }
                lastErr = if (total > 0) "不完整 ${part.length()}/$total" else "文件不完整"
                part.delete()
            } catch (e: Exception) {
                lastErr = e.message ?: e.javaClass.simpleName
                if (part.exists()) part.delete()
            }
        }
        running = false
        UpdateChecker.log(this, "下载失败：$lastErr")
        notify(
            NotificationCompat.Builder(this, CHANNEL_ID)
                .setSmallIcon(R.drawable.ic_stat_download)
                .setContentTitle("SXDffle 更新失败")
                .setContentText(lastErr)
                .setAutoCancel(true)
                .build()
        )
        stopForeground(false)
        stopSelf()
    }
}
