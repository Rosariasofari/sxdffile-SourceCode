# 实验分支：`experiment/embedded-loader`

> 目标：让 Flash **原位显示在网页里**（浏览器 UI 保留），并支持**缩放**与**旋转**；引擎侧要求**无损移植**。

---

## 一、为什么要重写「引擎宿主」

上游 `ruffle-android` 的宿主是 `GameActivity`：

```
GameActivity (Java)
   └── ANativeActivity_onCreate
         └── android_main(AndroidApp)          ← android-activity crate
               └── run(app)                    ← 事件循环
                     └── MainEvent::InitWindow
                           └── app.native_window()   ← 系统给的整屏窗口
                                 └── wgpu surface
```

这套结构有三个死结，导致「嵌进网页」不可能：

| 死结 | 原因 |
|---|---|
| 窗口是 Activity 级的 | `Activity` 的窗口矩形由 WindowManager 决定，`LayoutParams` 对它无效（只有 Dialog/悬浮窗能自定义） |
| 渲染与事件循环绑定 | `run()` 的 tick/render 由 android-activity 的 `PollEvent` 驱动，窗口一换就断 |
| 输入由系统分发 | 触摸事件经 `InputEvent::MotionEvent` 进 `run()`，与具体 View 无关 |

**结论：要嵌进网页，必须绕开 `GameActivity` 与 `run()`，自己拿 `ANativeWindow`、自己驱动 `tick/render`。**

---

## 二、新宿主架构

```
Kotlin (SurfaceView)                        Rust (src/embed.rs)
  │                                            │
  ├─ surfaceCreated ──JNI───────────────────► nativeEmbedStart
  │   Surface + 宽高 + SWF 参数 + 登录态        │  ① ANativeWindow_fromSurface
  │                                            │  ② spawn "sxdffle-embed" 线程
  │                                            │     线程内：wgpu surface + Player
  │                                            │  ③ 立即返回 handle（不阻塞 UI）
  │                                            │
  ├─ onTouch ─────────JNI───────────────────► nativeEmbedTouch  → mpsc
  ├─ surfaceChanged ──JNI───────────────────► nativeEmbedResize → recreate_surface
  ├─ 工具条 −/＋ ─────JNI───────────────────► nativeEmbedSetZoom → set_viewport_dimensions
  └─ surfaceDestroyed JNI───────────────────► nativeEmbedStop   → 退出循环 + release
```

### 关键实现点

**1. 窗口来源换成 Kotlin 的 Surface**

```rust
let ptr = ANativeWindow_fromSurface(env, surface);
RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(ptr))
```

不用 `ndk::native_window::NativeWindow`，因为它依赖 jni-sys 0.3，而本项目用 jni 0.22（jni-sys 0.4），`JNIEnv` 类型不兼容。直接声明三个 C 函数、用 `*mut c_void` 传 env 即可绕开版本错配。

**2. 自己驱动帧循环**

```rust
loop {
    while let Ok(msg) = rx.try_recv() { handle(msg) }   // 排空事件
    { let mut p = player.lock().unwrap();               // 短锁
      p.tick(dt); next = now + p.time_til_next_frame();
      if p.needs_render() { p.render(); } }
    rx.recv_timeout(next - now)                         // 睡到下一帧或被唤醒
}
```

`mpsc::Sender::send` 会直接打断 `recv_timeout`，因此**不需要 android-activity 的 waker**，比全屏版还简单。

**3. 异步任务调度**

Ruffle 的 `FutureSpawner` 要求把 future 轮询回宿主线程。这里沿用 `async_task`：

```rust
let scheduler = move |task| { tx.send(EmbedMsg::Task(PlayerRunnable::from_runnable(task))) };
```

⚠️ **这里有个必须踩对的坑**：`task.run()` 内部会自己 `player.lock()`。如果主循环在调用它时**已经持有**同一把锁，同线程不可重入的 `Mutex` 会**直接死锁**（表现为画面永远停在第一帧）。所以所有 `lock()` 都必须包在极小的作用域里，绝不能跨过 `task.run()`。

**4. 缩放实现**

不改渲染目标尺寸，改 `scale_factor`：

```rust
player.set_viewport_dimensions(ViewportDimensions {
    width, height,
    scale_factor: density * zoom,   // zoom: 0.4 ~ 4.0
});
```

Ruffle 的逻辑视口 = 物理尺寸 / `scale_factor`。增大 `scale_factor` → 逻辑视口变小 → SWF 按 `scale_mode` 重新缩放 → 视觉上就是放大。这样**不用动渲染管线**，也不会有任何分辨率损失。

**5. 旋转实现**

纯显示层：`SurfaceView.rotation = 90°` + 按包围盒等比 `scaleX/scaleY`。引擎完全无感（它始终认为自己在渲染 W×H），因此**游戏布局不会被破坏**。

触摸坐标必须做**逆变换**（先逆旋转、再逆缩放），否则旋转后点哪儿都偏：

```kotlin
val rx = dx * cos(-θ) - dy * sin(-θ)
val ry = dx * sin(-θ) + dy * cos(-θ)
return (rx / fit + w/2) to (ry / fit + h/2)
```

---

## 三、两个 Android 特有的坑（都已处理）

### 1. SurfaceView 会「打洞」，把重叠的普通 View 一起挖掉

`SurfaceView` 在 Window 上挖掉自己矩形对应的区域，由独立图层填充。**同一 Window 内与它重叠的普通 View 会被一起挖掉**——最开始我把缩放/旋转按钮放在 `EmbedFlashView` 内部，结果按钮**完全不可见**。

解决：工具条 `embedBar` 放在 `contentArea` **之外**（布局底部），与 SurfaceView 矩形不重叠。

### 2. 页面滚动后画面会「漂」

内嵌视图是原生 View，不会跟着 WebView 滚动。注入脚本因此加了位置同步（60ms 节流）：

```js
window.addEventListener('scroll', scheduleRect, true);  // → SxdffleBridge.embedRect(x,y,w,h)
```

同步时**只改 margin，不改尺寸**——尺寸变化会触发 `recreate_surface`，滚动时每秒几十次重建会直接把画面卡死。

---

## 四、引擎无损的证明

改动全部集中在新增文件，引擎与既有全屏路径**一行未动**：

| 文件 | 性质 |
|---|---|
| `src/embed.rs` | 新增（宿主层） |
| `src/embed_jni.rs` | 新增（JNI 边界） |
| `src/lib.rs` | 仅加 3 行：`mod embed; mod embed_jni;`、`PlayerId` 可见性、`PlayerRunnable::run()` |
| `app/.../EmbedFlashView.kt` | 新增 |
| `app/.../EmbedFlashController.kt` | 新增 |
| `app/.../BrowserActivity.kt` | 新增内嵌分支，原全屏路径保留 |
| `ruffle_core` / `ruffle_render_wgpu` / navigator / 字体 / 音频 | **零改动** |

符号表实测：

```
新增 8 个：Java_rs_ruffle_EmbedFlashController_nativeEmbed{Start,Stop,Resize,Touch,Scroll,Key,SetZoom,Status}
原有保留：14 个 Java_rs_ruffle_*（PlayerActivity / 输入 / 生命周期）全部在
```

因此，「能跑什么 Flash」与全屏版完全一致：AVM1/AVM2、TCP socket（`SocketMode::Allow`）、Referer/Cookie、设备字体，全部沿用。

---

## 五、使用方式

安装 `SXDffle-embed-arm64-v8a-debug.apk`（包名 `com.sxd.sxdffle.embed`，**可与稳定版共存**）。

1. 打开 `sxd.xd.com`，登录，点「测试1服」
2. 页面中检测到大尺寸 Flash 后**自动内嵌播放**，位置对齐网页里 Flash 元素
3. 底部工具条：`−` `＋` `⟳`（旋转 90°）`⤢`（切全屏）`✕`（关闭）
4. 也可双指捏合缩放

菜单 ☰ →「Flash 显示」可在**网页内嵌 / 全屏**之间切换；内嵌启动失败会**自动兜底**到全屏。

---

## 六、已知限制（请务必阅读）

1. **未在真机验证**。沙箱没有 Android 模拟器（无嵌套虚拟化），我只验证到：Rust 编译通过、JNI 符号导出、APK 结构与签名正确。
2. **内嵌画面会遮挡该区域的网页内容**（SurfaceView 打洞的必然结果）。这是「用 Flash 画面替换网页元素」的正确表现，但如果页面在该位置有需要交互的 HTML 控件，会被挡住。
3. **页面滚动同步有 60ms 延迟**，快速甩动时画面可能短暂错位。
4. **旋转后自动缩小**以完整显示（`fitScale`）。横版游戏旋转到竖屏会更小，此时建议用 `＋` 放大，或直接切全屏。
5. 引擎的 `show_context_menu`（右键菜单）在内嵌模式下不可用——渲染线程刻意不持有任何 JNI 引用。

---

## 七、回退

- 运行期：菜单切「Flash 显示：全屏播放」
- 构建期：`git checkout master`（`a0c3b07` 为稳定版基线）
