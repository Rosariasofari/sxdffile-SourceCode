# SXDffle-mod 交接事项

> 用途：新会话开工前读这一页即可接上，不需要翻聊天记录。
> 最后更新：2026-10-22 15:20，对应版本 0.261134。

---

## 1. 项目与仓库

| 项 | 值 |
|---|---|
| 应用 | 枝枝工具箱 · 内测魔改版（神仙道等 Flash 页游容器） |
| 包名 | `com.sxd.sxdffle.mod` |
| 代码树 | `/root/work/sxdffle`（宿主）、`/root/work/ruffle-mod`（引擎 fork）、`/root/work/gc-arena-sxd`（GC fork） |
| 分发仓库 | `Rosariasofari/sxdffile-mod`（**公开**，Release 放 APK） |
| 源码仓库 | `Rosariasofari/sxdffile-SourceCode`（公开，`publish.sh` 自动同步三棵树） |
| 令牌 | `/root/.sxd_mod_token`、`/root/.sxd_src_token`（600，勿外传） |
| 发布命令 | `RELEASE_NOTES=xxx.md bash /root/work/scripts/publish.sh <版本>` |
| 构建 | `cd /root/work/sxdffle && cargo build --release --target aarch64-linux-android && bash scripts/build-apk.sh release` |
| 注意 | `scripts/build-rust.sh` 在本机 bash 5.2 下会因 `export "CC_aarch64-..."` 报错，直接用 `cargo build`（`.cargo/config.toml` 已注入工具链） |

---

## 2. 版本改动时间线（只记有实际改动的）

| 版本 | 改动 | 效果 |
|---|---|---|
| 0.261094~097 | gc-arena slab 分配器（16B 对齐 63 size class，1MB slab bump） | 1779 万对象时分配器放大 4.7×→1.1× |
| 0.261105 | mimalloc 强制归还 | 一次削 1346MB |
| 0.261106 | 归还指数退避失效修复 | 峰值 6.6G→5.7G |
| 0.261111 | `mark_loaded` 判据修正（`start==0 && end>=full_len`） | 修掉「子 sprite 先跑完→满屏 #1009」 |
| 0.261117 | slab 版上线 | 峰值 7.4G 仍存活，稳态 4239MB |
| 0.261118~120 | 内存归因体系（块头 16B 携带 tag、12 类归属、爆发快照、大块 TOP） | 首次看清峰值构成 |
| 0.261120 | 静态图降采样 512→384→320 | 纹理 1199MB→328MB，RSS −985MB |
| 0.261121 | GC slab 单列标签 + SWF 解压打标 + **慢帧看门狗** | 首次抓到长帧现场 |
| 0.261122 | 大块 TOP 带标签 + 字符串/ByteArray/Array 扩容打标 | 定位 2.2GB 字符串垃圾 |
| 0.261123 | **字符串原地追加**（宽缓冲 ← 窄 ASCII） | `AS3字串` 2232MB→203MB，峰值 6348→3485MB |
| 0.261126 | **整条电影库摘除**（idle 180s + 根电影白名单） | 229 部/841MB，稳态 3925→3081MB |
| 0.261128 | 大块 TOP 剔除 slab | 真实 1~8MB 块首次露脸 |
| 0.261129 | 静止检测接通（`touch_active` → `note_advance`），静止 20s 后 idle 降 45s | 摘库等待 195s→65s |
| 0.261130 | 下载改 `.part` + 长度校验 + 启动清理 | 修复残包被安装 |
| 0.261131 | 大块直方图扣减兜底（tag 不匹配时按尺寸扣） | `2048KB×1130` → 与实际一致 |
| 0.261132 | 下载单飞锁（AtomicBoolean）+ 安装入口加锁 | 修复并发下载互相挤占 |
| 0.261133 | **前台服务承载下载**（dataSync）+ 通知进度 + 完成通知点击安装 | 修复切后台断连 |
| 0.261134 | AS3 调用深度统计（`CallDepthGuard`），慢帧日志带 `(深度N)` | 为帧内回收设闸 |

---

## 3. 已验证的收益（真机数据）

| 指标 | 起点 | 现在 |
|---|---|---|
| 峰值 RSS（短战报） | 7115MB（被杀） | **3485MB** |
| 峰值 RSS（长战报，1895 万对象） | — | 4866MB（仍存活） |
| 退宗门稳态 | 4190MB | **3081MB**（摘库后大块 1123MB） |
| 字符串垃圾峰值 | 2232MB | **203MB** |
| SWF 字节（摘库后） | 1231MB | **409MB** |
| slab | 723MB 从不归还 | 存活 242MB / 已归还 120MB |
| 纹理（降采样） | 1199MB | 328MB |

---

## 4. 已确认的关键结论（别再重复验证）

1. **峰值不是泄漏，是单帧内的临时分配**。长战报一帧内 1895 万对象，帧结束 GC 后掉到 180 万。
2. **大块内存的真凶排序**：字符串（已解决）> SWF 字节（已解决大半）> GC slab（已可归还）> 位图（仅 100~150MB，不用管）。
3. **`1M~8M` 档位里的「无归属」都是 slab 的 2MB 对齐块**，不是脚本缓冲。
4. **冷电影单字节回收对战斗 SWF 无效**（都带 DefineSprite），必须整条库摘除。
5. **摘库判据**：`try_release_data` 能成功 = 已 preload 完 + `last_active`/`last_used` 都超时；此时摘库最安全。
6. **字符串暴涨的根因在引擎**：`try_append_inline` 遇宽窄不一致直接放弃原地追加，每次 `+=` 新建整块。
7. **脚本内回收（`EXEC_GC_ENABLED`）仍是关的**，因为父级 Activation 的 `outer`/`local_register` 只在 Rust 栈上，根集不全 → 历史上 SIGSEGV。

---

## 5. 当前卡点（最重要）

**现象**：进游戏 → 宗门切磋 → 等约 20 秒 → **没有战报进度条、没进场景**；日志：167 次 SWF 载入、峰值 **6710MB**、**0 条慢帧现场**、0 条摘库。

**判断**：卡在 **preload（SWF 解析）**，不是脚本执行。看门狗的“当前方法/指令”打点在解释器里，preload 期间根本不进解释器，所以一条现场都抓不到。主线程被 preload 占死 → 进度条画不出来 → 看起来“没反应”。

**与历史数据对比**：上一轮 398 部 SWF → 4.3GB；这次 167 部 → 6.7GB。**片子更少、内存更高**，说明瓶颈在单次解析/解码单元特别大，而不是数量。

**两个候选原因**：
- 超大位图解码（`DefineBitsJPEG3`/`Lossless2` 解出完整 RGBA，8192² 就是 256MB，且“压缩流 + 全尺寸 + 降采样后”三份并存）；
- `DefineSprite` 深嵌套一次性展开。

---

## 6. 下一步（0.261135，改动已设计好，直接照做）

### 6.1 `core/src/sxd_watch.rs`

```rust
static PRELOAD_TAG: AtomicU64 = AtomicU64::new(0);   // 0 = 不在 preload
static PRELOAD_DONE: AtomicU64 = AtomicU64::new(0);

#[inline] pub fn note_preload(tag: u16, done: u32) {
    PRELOAD_TAG.store(tag as u64, Ordering::Relaxed);
    PRELOAD_DONE.store(done as u64, Ordering::Relaxed);
}
#[inline] pub fn clear_preload() { PRELOAD_TAG.store(0, Ordering::Relaxed); }
```

`SlowFrame` 增加 `stage: &'static str`、`tag: u16`、`done: u32`；`tick()` 保留原逻辑。

### 6.2 `core/src/display_object/movie_clip.rs` preload 循环

在 `read_tag()` 之后、`match tag` 之前插一行：

```rust
crate::sxd_watch::note_preload(tag.code(), done as u32);
```

结束/出错时 `clear_preload()`。

### 6.3 `src/embed.rs` 慢帧日志

`PRELOAD_TAG != 0` 时输出：

```
SXDffle 慢帧 4200ms · 阶段=preload · tag=DefineBitsJPEG3(35) · 已解析 1234 · RSS 5120MB · slab …
```

### 6.4 验收

复现「切磋 20 秒无进度条」，日志出现 `阶段=preload · tag=xxx`：
- `tag=35/21/20/36`（位图类）→ 做“解码前先按 320 降采样”（路线 7.5a）
- `tag=39`（DefineSprite）→ 提前做惰性展开（路线 9）

---

## 7. 路线表（以本表为准）

| # | 项目 | 状态 |
|---|---|---|
| 4 | 冷电影字节回收 | 已被 0.261126 整库摘除取代 |
| 5 | 内存归因 | 完成 |
| 6 | BitmapData 单份化 | 暂缓（位图仅 100~150MB） |
| 7 | SWF mmap + 窗口解压 | 部分（字节回收已做，窗口解压未做） |
| **7.5** | **preload 阶段归因 + 切片** | **下一步（当前卡点）** |
| 8 | ASTC 磁盘缓存 | 未开始 |
| 9 | 惰性符号解析（时间轴按需展开） | 未开始（若 7.5 指向 DefineSprite 则提前） |
| 10 | 解释器打磨（GetProperty 内联缓存等） | 未开始；已有 2.3s 慢帧 `Coerce` 3.7s 待处理 |
| 11 | 字节码预编译 / JIT | 长期 |

工程化支线（不占主线编号）：包解析工具 → 配置外置化（降采样阈值/摘库 idle/开关）→ 一键构建平台。

---

## 8. 发布与文案约定

- Release 说明**只写事实**，三段式：`更新` / `修复` / `加入`，不写成因、不写下一步。
- `gh_release.sh` 已支持 `RELEASE_NOTES=<file>` 注入 body；`PATCH /releases/tags/{tag}` 会 404，必须 `GET tags/{tag}` 拿 id 再 `PATCH /releases/{id}`。
- GitHub 推送经常被掐，失败后加 `-c http.version=HTTP/1.1` 重试，通常第二次成功。

---

## 9. 已知坑（编译/API）

1. `Player::enter_arena()` 给的是 `&GcRootData`（不可变），要改库表必须走 `Player::update()`（给 `&mut UpdateContext`）。
2. `uc.root_swf` 是 `&mut SwfMovie`，要 `Some(*uc.root_swf)`，且不要先 `let root = uc.root_swf;`（会 move）。
3. `cargo build ... | grep -cE "^error"` 在 0 错误时返回 exit 1，串 `&&` 会误判失败。
4. gc-arena 是 `no_std`，不能用 `thread_local!`；要线程局部语义得用“宿主注入函数指针”（见 `SLAB_HOOK`）。
5. slab 为了 1MB 对齐按 2MB 申请，所有报表口径要除 2（`tag_bytes` / `slab_bytes` 已处理）。
6. `Method` 在 `avm2.rs` 里是私有 re-export，跨模块用需要 `pub(crate) use`。
7. 慢帧看门狗只在解释器打点，**preload / 渲染阶段抓不到**（本次卡点的直接原因）。

---

## 10. 一句话状态

> 内存主线已从 7.1GB 被杀的版本推进到「短战报 3.5GB、退宗门 3.1GB」；当前唯一阻塞是 **战报 preload 阶段 6.7GB 且无现场日志**，解法是先给 preload 插桩（§6），再按 tag 类型决定走位图降采样还是惰性展开。
