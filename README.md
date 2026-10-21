# SXDffle-mod

《神仙道》页游的 Android 容器，基于 Ruffle（Flash/AVM2 运行时）+ wgpu。
本仓库是「内测魔改版」的源码与发布包，目标是把 ruffle 改到能扛住大体量 Flash 页游
（十万级显示对象、单帧千万级临时对象、战报场景 3~5GB 峰值）。

## 目录

| 目录 | 内容 |
|---|---|
| `sxdffle/` | Android 宿主：JNI、渲染主循环、内存监控、ExternalInterface、打包脚本 |
| `sxdffle/app/` | Android 壳（Kotlin）、内嵌 WebView、菜单、日志轮转 |
| `ruffle-mod/` | Ruffle 引擎 fork（AVM2、显示列表、渲染后端、库模型） |
| `gc-arena-sxd/` | gc-arena fork（GC 堆统计、slab 分配器、额外根注册） |

## 相对上游的改动（按时间累积）

### 规模假设类（原版按 2011 年体量写的，页游规模下会退化）

- GcBox 改用自研 **slab 分配器**（16B 分级 + 1MB slab + 空闲链），
  消除每对象一次 malloc 造成的 4~5 倍内存放大；
- 大块分配统计从「指针哈希表」改为 **块头携带标签**，避免开放寻址删除后表失效；
- 显示对象「孤儿」列表：去掉遍历期的 `Rc::make_mut` 深拷贝，去重改尾部 256 项；
- `preload` 分片到 16ms/帧，避免单帧解析几百毫秒导致进度条卡死；
- 逐帧 GC 泵 + 紧急增量回收（切片执行，避免整帧停顿）。

### 内存

- 静态图（`DefineBits*`）解码后按整数倍盒式平均降到 ≤320 再上传 GPU；
- `SwfMovieData.data` 改为可释放（`Mutex<Option<Arc<Vec<u8>>>>`），
  只对「库中无 DefineSprite」的纯资源 SWF 生效（结构上绝对安全）；
- RSS/纹理/位图/GC/slab/大块归属/映射 TOP 的全套采样与爆发快照。

### 诊断

- 帧内分段计时：`enter / construct / scripts / exit`，以及 enter 内的孤儿/舞台/推进；
- AVM2 指令热点采样（按名字与耗时两个维度）；
- 日志按版本隔离，带本地时间戳。

## 构建

见 `sxdffle/scripts/build-rust.sh`（交叉编译 aarch64）与 `sxdffle/scripts/build-apk.sh`（打包 APK）。
需要 Android NDK、SDK 与 Rust 的 `aarch64-linux-android` target。

## 已知限制

- 内核 < 5.4 的设备不支持 `MADV_PAGEOUT`，后台无法把匿名页压进 zram；
- 战报加载峰值仍在 4~5GB（游戏单帧创建千万级对象），继续压缩需要重写时间轴解析时机。
