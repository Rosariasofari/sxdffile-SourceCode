# SXDffle 源码学习指南（截至 0.261036）

> 这份文档面向"想读懂这套代码"的人。先讲结构，再讲一次完整的调用链，最后是这一路踩过的坑——
> 坑比结构更值钱：每个坑都对应一段真实日志和一次根因定位。

---

## 一、工程结构

```
sxdffle/
├── src/                         Rust 侧（编译成 libsxdffle.so）
│   ├── lib.rs                   全屏模式宿主（PlayerActivity 用的那套，最早期的实现）
│   ├── embed.rs                 ★内嵌模式宿主：BrowserActivity 用的渲染线程
│   ├── embed_jni.rs             ★Kotlin ↔ Rust 的 JNI 边界（只做参数搬运，不持 JNI 引用）
│   ├── ui.rs                    设备字体（fontdb 扫 /system/fonts，缺了中文就是方块）
│   ├── audio.rs                 AAudio 输出后端
│   ├── navigator.rs             HTTP/TCP 导航器（无 CORS、真 TCP、Cookie/Referer 注入）
│   ├── keycodes.rs              按键映射
│   ├── trace.rs                 SWF trace → logcat + 文件双写
│   └── crash.rs                 ★致命信号捕获：崩溃前把"死在哪一步/哪个线程"写进日志
│
├── app/src/main/
│   ├── java/rs/ruffle/
│   │   ├── BrowserActivity.kt   ★浏览器主界面：WebView、工具栏、悬浮球、收藏夹、日志
│   │   ├── EmbedFlashView.kt    ★SurfaceView 容器：手势分流、Surface 生命周期、显示变换
│   │   ├── EmbedFlashController.kt  JNI 声明（与 embed_jni.rs 一一对应）
│   │   ├── PlayerActivity.kt    全屏播放 Activity（老路径，仍保留）
│   │   └── PanicScreen.kt       Rust panic 时弹出的崩溃页（仅主线程）
│   ├── assets/
│   │   ├── sxdffle_inject.js    ★注入脚本：检测 Flash、入口提升、ExternalInterface 代理
│   │   ├── sxdffle_flash_spoof.js  document-start 伪造 navigator.plugins['Shockwave Flash']
│   │   └── default_bookmarks.txt   内置联运平台清单（首次启动导入成收藏夹分组）
│   ├── res/xml/network_security_config.xml  明文 HTTP 放行名单
│   └── jniLibs/arm64-v8a/libsxdffle.so      构建产物（源码包里没有，用 scripts/build-rust.sh 生成）
│
├── scripts/
│   ├── build-rust.sh            交叉编译（cargo build --target aarch64-linux-android）
│   ├── build-apk.sh             gradle assembleRelease + 校验产物
│   └── fetch-deps.sh            拉取 Ruffle 源码与 vendor crates（离线环境要预先跑）
│
├── docs/                        设计与踩坑文档
└── vendor-crates/               被 patch 的第三方 crate（regress / jpegxr / nellymoser 等）
```

依赖关系上有一个容易忽略的点：`Cargo.toml` 里的 Ruffle 是 **path 依赖**（`../ruffle/core` 等），
也就是说这个工程必须和 Ruffle 源码放在同级目录：

```
work/
├── ruffle/      Ruffle 源码（fork 自 ruffle-rs/ruffle v0.7.1）—— 源码包里已附带
└── sxdffle/     本工程
```

> 源码压缩包把两者放在同一层：解压后直接就是 `sxdffle/` + `ruffle/`，
> 满足 `Cargo.toml` 里的 `../ruffle/...` 路径。`ruffle/tests`（约 230MB 的
> SWF 测试素材）没有打进去，构建不需要它。

---

## 二、两条宿主路径，别混

| | 全屏模式 `lib.rs` | 内嵌模式 `embed.rs` |
|---|---|---|
| 入口 | `PlayerActivity` + `GameActivity` | `BrowserActivity` + `EmbedFlashView` |
| 线程模型 | android-activity 主循环 | 自建 `sxdffle-embed` 渲染线程 + mpsc 消息 |
| Surface | GameActivity 提供 | `ANativeWindow_fromSurface` 从 SurfaceView 取 |
| 用途 | 早期实现/兜底 | 现在实际在用的路径 |

读代码建议**只看 embed 那条线**：`lib.rs` 里很多逻辑是历史遗留，理解成本高、收益低。

---

## 三、一次"网页 → 游戏画面"的完整调用链

```
① 页面加载
   WebViewClient.onPageStarted
     └─ (所有 frame 都会跑) WebViewCompat.addDocumentStartJavaScript
          ├─ sxdffle_flash_spoof.js   伪造 Flash 插件（很多页游靠这个判断"环境支持"）
          └─ sxdffle_inject.js        document-start 注入，每个 frame 各跑一份

② 检测
   sxdffle_inject.js: SXD.rescan()
     ├─ 扫 embed/object/iframe/a，取 SWF 地址
     ├─ 判断是不是"整页主画面"（宽 ≥60% 视口、高 ≥20%）→ fill
     └─ SxdffleBridge.openEmbed(url, pageUrl, x, y, w, h, flashVars, inFrame)

③ 进入原生
   BrowserActivity.FlashBridge.openEmbed  (@JavascriptInterface)
     └─ openFlashContent → runEmbedded
          ├─ 计算容器尺寸（CSS px → 物理 px，cssToView）
          ├─ new EmbedFlashView(...)   ← SurfaceView，加到 flashContainer
          └─ nativeEmbedStart(...)  → embed_jni.rs

④ 引擎启动（渲染线程内）
   embed::start → render_thread
     ├─ set_stage(1..8) 逐个打点（崩溃时能看到死在哪一步）
     ├─ WgpuRenderBackend::for_window_unsafe(ANativeWindow)   ← EGL surface
     ├─ PlayerBuilder::new()
     │    .with_ui(AndroidUiBackend)     字体
     │    .with_renderer(wgpu 后端)
     │    .with_audio(AAudioAudioBackend)
     │    .with_navigator(ExternalNavigatorBackend)  真 TCP / 无 CORS
     │    .with_external_interface(HostExternalInterface)
     │    .build()
     ├─ fetch_root_movie(swf, flashVars)   ← flashVars 必须 URL 解码
     └─ loop { tick(dt) → render() → recv_timeout(下一帧时间) }

⑤ 输入
   EmbedFlashView.onTouchEvent
     ├─ 单指快速拖动 = 平移（改 SurfaceView 的 translation）
     ├─ 短按 = 点击（换算成 SWF 坐标）
     └─ 长按 = 交给游戏拖拽
     → nativeEmbedTouch(phase, x, y) → PlayerEvent::MouseDown/Move/Up

⑥ 收尾
   closeEmbedded()
     ├─ nativeEmbedSetRenderPaused(h, true)   ← 先停 present
     ├─ nativeEmbedStop(h)                     ← 发 Stop，join 渲染线程
     └─ container.removeAllViews()
```

---

## 四、渲染与坐标（最容易出错的部分）

### 1. 三个尺寸概念

| 名字 | 含义 | 典型值 |
|---|---|---|
| Surface 物理尺寸 | `ANativeWindow` 的像素大小，**引擎视口就是它** | 1600×900（自定义分辨率时） |
| 容器尺寸 | `flashContainer` 的布局大小 | 1080×2320（内容区） |
| CSS 尺寸 | 网页里元素的 `getBoundingClientRect()` | 980 宽的布局视口里量出来的 |

`cssToView()` 用 `webView.scale` 换算，不能用 `density`（2.75 会把 980 算成 2695）。

### 2. 缩放为什么不能改引擎

`scale_factor` 一变，Ruffle 会重新布局游戏（文字变小、按钮错位）。
所以缩放实现为 **显示层变换**：改 `SurfaceView.scaleX/scaleY + translation`，
引擎内部尺寸保持不变。`displayScale()` 里横屏取 `max`（铺满）、竖屏取 `min`（完整显示）。

### 3. 自定义分辨率为什么要 NoScale

`ShowAll` 模式下 `set_movie_size()` 不会触发 RESIZE，游戏看到的舞台尺寸不变。
所以自定义分辨率走 `NoScale`：**物理视口 = 目标分辨率**，`scale_factor = 1.0`，
舞台尺寸就等于视口尺寸，游戏自己按新尺寸重排。

---

## 五、注入脚本的四个机制

1. **document-start 注入**
   `WebViewCompat.addDocumentStartJavaScript(webView, script, setOf("*"))` 会注入到**每个 frame**。
   页游的 SWF 几乎都在 iframe/frameset 里，只在顶层注入等于白做。

2. **入口提升（promoteEntry）**
   三种页面形态会被识别并"提到顶层"：
   - 自动提交表单（7k7k 的 `togame.php`）→ 抓 hidden input 里带 `hash` 的 URL
   - frameset（669ye / 智珠）→ 取最后一个有效 frame
   - 只剩一个大 iframe 的框架壳 → 取那个 iframe
   
   **回弹拉黑**：提升后 5 秒内又回到原页面，就把该 host 记进黑名单，本次运行不再提升
   （51 的 ping-pong 就是这么治的；而 flash.cn 的登录 bridge 需要提升，所以不能按"同域"一刀切）。

3. **Flash 元素代理**
   给 `<embed>/<object>` 套 `Proxy`，并接管 `document.getElementById`：
   页面访问 `el.function_xxx()`（SWF 通过 `ExternalInterface.addCallback` 注册的回调）时，
   转发到 `SxdffleBridge.externalCall(name, argsJson)` → JNI → `player.call_internal_interface`，
   同步返回结果（300ms 超时）。51 的 `swf_is_load()` 轮询就靠这条链。

4. **FlashVars 收集**
   从 `<param name="flashvars">`、`<embed flashvars>`、URL query 里取，再交给引擎。
   注意引擎侧要用 `url::form_urlencoded::parse` 解码（`server_url` 里有 `%3A%2F%2F`）。

---

## 六、踩坑清单（按时间顺序，每条都有日志依据）

| 现象 | 根因 | 修法 |
|---|---|---|
| `ERR_CLEARTEXT_NOT_PERMITTED` | Android 9+ 默认禁明文 | `network_security_config.xml` 放行 |
| 中文变方块 | Ruffle 调 `UiBackend::load_device_font`，我们没实现 | `ui.rs` 用 fontdb 扫系统字体 + CJK 回退链 |
| 内嵌必崩 | 没建 tokio runtime / rustls provider | `net_runtime()` + `install_default()` |
| 黑屏 + `EGL BadAlloc` | `lockCanvas` 把 Surface 切成 CPU 模式 | 删掉，改 `set_background_color(黑)` |
| 点击坐标整体偏 | 把 CSS px 当物理 px（980 vs 1080） | `cssToView()` 乘 `webView.scale` |
| 画面偏小 | `scale_factor` 传了 density(2.75) | 传 CSS→物理的真实比例（≈1.1） |
| 分辨率改不动 | ShowAll 下 `set_movie_size` 不 fire RESIZE | 改 NoScale + 物理视口 |
| 旋转后错层 | 容器/Surface 是固定像素，旋转没人重算 | 监听 `contentArea` 尺寸变化重排 |
| 旋转后误判为"网页原位" | `embedFromFrame` 被 `closeEmbedded()` 复位 | 赋值移到 `runEmbedded` 之后 |
| 关内嵌后再加载新站 → SIGABRT | 渲染线程还在 present 已销毁的 Surface | 停之前先 `SetRenderPaused(true)` |
| 压测越测越卡、手机重启 | 退出标记只跳出内层循环，每局泄漏一整个 player | `quitting` 标记传到外层 |
| flash.cn 选服后回登录页 | 同域 iframe 被禁止提升，登录 bridge 拿不到 top 上下文 | 恢复提升 + 回弹拉黑 |
| 登录态重启就丢 | `document.cookie` 未落盘，进程一崩全丢 | `onPageFinished` 里 `CookieManager.flush()` |
| 切站立刻闪退 | 上一局引擎 1.4GB 没释放，新页面分配叠上去 | 切站前先关引擎并等 RSS 回落 |

---

## 七、怎么调试

### 日志

一行 `logToFile()` 同时写 logcat 和
`Android/data/com.sxd.sxdffle.internal/files/sxdffle.log`，
Rust 侧 `log::info!` 也写同一个文件（`android_logger` + 自定义 tee）。
菜单 → 导出日志 可以直接分享。

**先看第一行**：`SXDffle 启动 v0.261036-internal` —— 确认跑的是哪一版。

### 崩溃

`crash.rs` 注册 `SIGSEGV/SIGABRT/SIGBUS/SIGILL/SIGFPE` handler，
在进程死掉前用 `write(2)` 写常量字符串（signal handler 里不能分配、不能加锁）：

```
[SXDffle] crash at stage 8/8: shutting down
  tid: 26974
  thread: sxdffle-embed
  signal: SIGABRT (abort / C++ assert)
```

- `stage 1..8` 对应启动的每一步（1=取 ANativeWindow，3=建 EGL，4=建 Player，5=加载 SWF，6=首帧）
- `stage 9 / crash while idle` = 引擎已经销毁，崩在别处（WebView、Java 线程）
- `thread:` 是新增的，用来区分"引擎析构崩的"和"UI/WebView/网络线程崩的"

### 内存

```
内存[内嵌启动/关闭]: Java 7/29MB · RSS 1398776 kB
SXDffle embed: 运行中 RSS 1452MB · 已渲染 860 帧
SXDffle embed: 内存归还(会话结束) RSS 1389MB → 1390MB · trim=None purge=Some(1)
```

- `RSS` 来自 `/proc/self/status VmRSS`，含 native（Ruffle 的纹理/字体都在这里）
- `trim=None` 表示系统没导出 `malloc_trim`（走 `dlsym` 查的）；`purge=1` 表示 `mallopt(M_PURGE)` 成功
- 归还后数值不动 → 内存还在被引用，不是分配器囤页

---

## 八、建议的阅读顺序

1. `app/src/main/java/rs/ruffle/BrowserActivity.kt` 的 `setupWebView()` / `openFlashContent()` /
   `runEmbedded()` / `closeEmbedded()` —— 先把"壳"看懂
2. `app/src/main/assets/sxdffle_inject.js` 的 `rescan()` / `promoteEntry()` / `openEmbedded()` ——
   理解"网页里怎么找到 Flash"
3. `src/embed.rs` 的 `render_thread()` —— 引擎怎么起来、怎么循环、怎么收尾
4. `src/embed_jni.rs` —— Kotlin 与 Rust 的边界（注意它刻意不持有 JNI 引用）
5. `src/ui.rs` / `src/audio.rs` / `src/navigator.rs` —— 三个"能力补全"的典型例子
6. `src/crash.rs` —— 怎么在 signal handler 里安全地留下线索

> 说明：源码包里**不包含**签名用的 `keystore/` 与 `local.properties`；
> `jniLibs/*.so` 也不在包里，运行 `scripts/build-rust.sh aarch64-linux-android` 会重新生成。
> `ruffle/` 只去掉了 `tests/`（测试素材），其余源码完整。
