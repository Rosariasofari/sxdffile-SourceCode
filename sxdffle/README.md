# SXDffle 闪客浏览器

一个把 **Flash 运行时直接内嵌进 App 本体** 的 Android 浏览器，用于在现代安卓设备上重新运行 Flash 游戏与 Flash 页游（如《神仙道》）。

---

## 一、先说清楚：为什么不能"把 Adobe Flash 塞进 Chrome 内核"

这是本项目所有技术决策的起点，必须先讲明白，否则方向会错。

| 传言 | 事实 |
|---|---|
| 把 Adobe Flash Player 插件内嵌进 Chrome/Edge 内核 | **不可能**。Google 在 Chrome 88（2021-01）彻底移除了 PPAPI 插件接口，Edge 同源同命。Android 端自 4.1 起从未支持过 PPAPI Flash。 |
| 用 WebView + Ruffle(WASM) 就能跑页游 | **跑不全**。浏览器沙箱有三道硬墙：① 无法建立原始 TCP 连接（Flash 的 `Socket`/`XMLSocket` 直接废掉）；② 所有跨域请求受 CORS 约束，而原版 Flash 插件是浏览器厂商开的**特权后门，不受 CORS 限制**；③ 843 端口的 crossdomain 策略无法由页面自行完成。 |
| 复刻一个 Ruffle 用不了多少工作量 | Ruffle 约 **45~50 万行 Rust**，10 余位核心开发者自 2020 年持续开发至今。这是数十年人年量级的工程量，任何"几天复刻"的说法都不成立。 |

**所以 SXDffle 选择了唯一真正可行的架构：绕开浏览器沙箱本身。**

> 顺带一提，这也是《神仙道》官方「微端」的做法 —— 官方公告里那句"由于 Flash 框架限制…"，背后就是一个**独立 Flash 运行时 + 宿主外壳**的客户端。

---

## 二、架构

```
┌───────────────────────── SXDffle APK ─────────────────────────┐
│ Kotlin 层 —— 浏览器外壳（本项目新增）                            │
│                                                                │
│   BrowserActivity                                               │
│     ├─ 地址栏 / 前进后退 / 刷新 / 主页                          │
│     ├─ 缩放（放大·缩小·100%·网页自适应）与桌面模式               │
│     ├─ WebView 渲染普通网页（HTML/JS 部分）                     │
│     ├─ 注入 sxdffle_inject.js 侦测 <embed>/<object>/<iframe>     │
│     │   并在 Flash 位置叠加「▶ 用引擎运行」可点击遮罩             │
│     └─ 点击遮罩 → 携带 SWF地址 + Referer + Cookie 跳转 ↓         │
│                                                                │
│   PlayerActivity (GameActivity + SurfaceView)                   │
│     └─ 把 Surface 与上下文参数交给原生层                        │
├────────────────────────────────────────────────────────────────┤
│ Rust 层 —— libsxdffle.so（引擎内核，基于 Ruffle 定制）           │
│                                                                │
│   ruffle_core          AVM1/AVM2 虚拟机 —— Flash 运行时内核      │
│   ruffle_render_wgpu   GPU 渲染（Vulkan/GLES），Letterbox 自适应  │
│   ruffle_video_softw.  视频解码（H.264/VP6/FLV）                │
│                                                                │
│   navigator ★本项目定制重点★                                    │
│     ├─ HTTP : reqwest + rustls   → 无 CORS 限制  ← 页游HTTP通信 │
│     ├─ TCP  : tokio::TcpStream   → 真实 TCP 长连接 ← 页游Socket │
│     ├─ SocketMode::Allow         → 绕过 843 端口策略            │
│     ├─ Referer 注入              → 通过防盗链校验               │
│     ├─ Cookie 注入               → 打通网页登录态               │
│     └─ UA 伪装（可配置）          → 通过服务器 UA 校验            │
└────────────────────────────────────────────────────────────────┘
```

### 为什么这条路走得通

| 障碍 | WebView + WASM 方案 | SXDffle（原生）方案 |
|---|---|---|
| TCP Socket（页游长连接） | 不支持（浏览器禁止） | 支持（`tokio::TcpStream` 真连接） |
| 843 端口 crossdomain 策略 | 无法满足 | `SocketMode::Allow` 直接放行 |
| 跨域资源 / CORS | 受 CORS 约束 | `reqwest` 不受浏览器约束 |
| 防盗链 Referer | 受限 | 从浏览器侧显式注入 |
| 网页登录态 | 难打通 | Cookie 桥接 |
| 自适应分辨率 | 需额外处理 | 引擎 `Letterbox` + 前端多档缩放 |

---

## 三、目录结构

```
sxdffle/
├── src/                        # Rust 引擎层
│   ├── lib.rs                  # 主循环；★定制：注入 Referer/Cookie
│   ├── navigator.rs            # ★定制：Socket 放行与日志
│   ├── java.rs                 # ★定制：新增 getReferer()/getCookie() JNI
│   ├── audio.rs / trace.rs / keycodes.rs / custom_event.rs
├── app/src/main/
│   ├── java/rs/ruffle/
│   │   ├── BrowserActivity.kt  # ★新增：浏览器主界面
│   │   ├── PlayerActivity.kt   # ★改造：接收上下文 + 加载 libsxdffle.so
│   │   ├── MainActivity.kt     # 保留：本地 SWF 选择页
│   │   └── SelectSwfScreen.kt 等
│   ├── assets/sxdffle_inject.js# ★新增：页面 Flash 侦测与接管脚本
│   ├── res/layout/activity_browser.xml  # ★新增：浏览器界面
│   └── AndroidManifest.xml     # ★改造：BrowserActivity 为主入口
├── scripts/
│   ├── build-rust.sh           # 交叉编译引擎为 .so
│   ├── fetch-deps.sh           # 拉取 Ruffle 源码与本地化依赖
│   └── build-apk.sh            # 一键出 APK
├── Cargo.toml                  # ★改造：路径依赖 + 离线 patch
└── rust-toolchain.toml
```

---

## 四、构建

### 依赖

- JDK 17+、Android SDK（Platform 36、Build-Tools 34+）
- Android NDK r26+（推荐 r27c）
- Rust stable + `cargo-ndk`

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi
cargo install cargo-ndk
```

### 一键构建

```bash
# 1) 拉取引擎源码与离线依赖（约 300MB）
bash scripts/fetch-deps.sh

# 2) 编译原生引擎 → app/src/main/jniLibs/
bash scripts/build-rust.sh

# 3) 打包 APK
bash scripts/build-apk.sh
# 产物：app/build/outputs/apk/debug/app-arm64-v8a-debug.apk
```

### 关于离线构建

本项目在上游基础上做了一处关键改造：**把 Ruffle 的 git 依赖全部重定向到本地副本**
（`Cargo.toml` 中的 `[patch.*]` 段），这样在无法访问 `github.com` 的网络环境下也能完整构建。
`scripts/fetch-deps.sh` 会通过 `codeload.github.com` 把这些源码拉下来。

被本地化的依赖：`regress`、`jpegxr`、`nellymoser-rs`、`flash-lso`、`h263-rs*`、`nihav_*`。

---

## 五、使用

1. 启动 App，地址栏输入游戏网址（默认主页为 `https://sxd.xd.com`）。
2. 页面加载后，脚本会自动侦测 Flash 内容，并在其位置显示绿色边框遮罩
   **「▶ SXDffle 引擎运行 Flash」**；顶部同时出现提示条与「用 Flash 引擎打开」按钮。
3. 点击遮罩 → 进入原生引擎全屏渲染，此时 SWF 以 `Letterbox` 方式自适应屏幕，
   触摸事件会正确映射为鼠标事件。
4. **缩放**：控制条的「缩小 −／放大 ＋／100%」按整个网页缩放；
   「网页自适应」让页面按屏幕宽度重排；「桌面模式」切换 UA（页游建议保持桌面模式，
   否则服务器可能返回移动版页面导致找不到 Flash 入口）。
5. 「扫描Flash」可随时重新侦测（应对 JS 延迟渲染的页面）。

### 针对页游的登录流程

页游通常是「先在网页上登录 → 再进入游戏」。SXDffle 的处理是：

1. 在 `BrowserActivity` 的 WebView 中完成登录（Cookie 由 `CookieManager` 持有）；
2. 点击 Flash 遮罩时，把 **当前页面地址作为 Referer**、**该域 Cookie 原文**一并传给 `PlayerActivity`；
3. `PlayerActivity` 通过 JNI 把二者交给 Rust 层的 `ExternalNavigatorBackend`；
4. 引擎此后所有 HTTP 请求自动携带该 Cookie 与 Referer，Cookie 商店（`cookie_store(true)`）
   还会持久化后续 Set-Cookie。

---

## 六、已知限制（请务必阅读）

1. **需要真机验证**。《神仙道》能否完整跑通取决于其具体实现，本项目打通了
   HTTP / TCP / Referer / Cookie 四项基础设施，但不能保证一次成功，需要按实际日志调优。
2. **`XMLSocket` / 自定义二进制协议**：已支持（走真 TCP），但若服务端对包序、
   心跳间隔敏感，可能需要按日志调整超时。
3. **`NetConnection`（RTMP / AMF Remoting）不支持**。这是上游 Ruffle 的现状
   （见 ruffle-rs/ruffle#265、#3317），不是本项目引入的问题。
4. **Stage3D 重度内容**对 GPU 有要求，低端设备或软件渲染环境可能报
   `wgpu: Validation Error`。
5. **首次加载较慢**：页游 SWF 与资源体积大，建议在 Wi-Fi 下运行。
6. 引擎为 CPU/GPU 双重负载，低端机可能掉帧。

---

## 七、上游与许可

- 引擎内核派生自 [Ruffle](https://ruffle.rs/)（`ruffle-rs/ruffle`），**MIT OR Apache-2.0**。
- Android 宿主层派生自 [ruffle-android](https://github.com/ruffle-rs/ruffle-android)。
- 本项目（SXDffle 增量部分）同样以 **MIT OR Apache-2.0** 发布，详见 `LICENSE.md`。

按许可要求，分发时须保留上游版权声明。本项目的定制点均在源码中以
`SXDffle` 注释标记，便于对照上游 diff。

---

## 八、想读源码

`docs/ARCHITECTURE-LEARNING.md` 是面向学习的版本：工程结构、完整调用链、
渲染/坐标的三个尺寸概念、注入脚本的四个机制，以及按时间排列的踩坑清单
（每条都对应真实日志里的现象与根因）。
