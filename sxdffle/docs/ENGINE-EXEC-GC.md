# 引擎改造：AVM2 脚本执行中的增量 GC

## 背景

战报加载时，游戏会在**一次 `socketData` 回调里**把整包数据解析成对象树。
实测这场解析会：

- 创建约 2000 万个 GC 对象；
- 让 RSS 从 1.8GB 冲到 7.8GB；
- 期间渲染循环一次都不转（25 秒只渲染 64 帧），所以**读取条根本画不出来**。

原因是结构性的：Ruffle 的 GC 全部挂在渲染循环上，而脚本执行时：

1. `Player::run_frame` 用 `arena.mutate(...)` 把整个 arena 可变借用走；
2. `gc_arena::Arena::collect_debt` 需要 `&mut Arena`，这时候拿不到；
3. AVM2 值栈原本被排除在 GC 根之外（`StackData::trace` 直接断言"回收时栈必须为空"）。

所以之前的做法（常态回收、GC 泵、冲击检测、对象熔断）全都只能"事后"或"阻断"，
无法在解析过程中释放垃圾。熔断虽然把峰值压到 2.5GB，但会打断脚本，战报永远加载不出来。

## 这次改了什么

### 1. gc-arena：允许在 `Mutation` 期间回收

本地 fork：`/root/work/gc-arena-sxd`（版本 0.7.0），由 `Cargo.toml` 的
`[patch.crates-io]` 接入。

- `Context` 增加 `root_ptr` / `root_collect`，记录当前 root 与一个单态化的回收函数；
- `Arena::mutate` / `mutate_root` 在进入闭包前登记 root；
- 新增 `Mutation::collect_debt_reentrant()`：在 `&Mutation` 上执行一次完整周期
  （mark + sweep）。

```rust
// 用法（unsafe）：调用方必须保证所有活跃 Gc 都能从 root 或堆内部到达
unsafe { mc.collect_debt_reentrant() };
```

### 2. Ruffle：让 AVM2 值栈参与标记

`core/src/avm2/stack.rs`：

```rust
fn trace<C: Trace<'gc>>(&self, cc: &mut C) {
    let top = self.stack_pointer.get();
    for cell in &self.stack[..top] {
        cc.trace(&cell.get());
    }
}
```

原实现是 `assert_eq!(self.stack_pointer.get(), 0)`——它依赖"只在帧边界回收"。
现在栈本身就是回收时的主要根。

### 3. Ruffle：在指令边界触发回收

`core/src/avm2/activation.rs` 的 `timeout_check()`（每 1 万条指令执行一次）：

```rust
let debt = self.context.gc_context.metrics().allocation_debt();
if debt >= 2_000_000.0 {
    unsafe { self.context.gc_context.collect_debt_reentrant() };
}
```

债务单位是**对象数**（gc-arena 的 `allocated_gcs - wakeup_amount`）。
积压 200 万就回收一次，回收后债务归零。

日志：

```
SXDffle 执行中 GC：12345678 → 2345678 个对象（债务 2000012，耗时 180ms）
```

## 安全性前提与已知风险

`gc_arena` 的 `do_collection` 文档写得很清楚：

> at the time of call no `Gc` pointers can be live that are not reachable from the given root object.

我们满足它的方式是：

| 活跃对象 | 是否在根里 |
|---|---|
| AVM2 操作数栈 / 局部变量 | ✅ `StackData` 已参与 trace |
| 舞台、显示列表、类表、方法表 | ✅ 从 `Stage` root 可达 |
| `Activation.outer`（ScopeChain） | ⚠️ 依赖方法的 scope 从类表可达 |
| `Activation.caller_domain` / `caller_movie` / `bound_superclass_object` | ⚠️ 同上 |

⚠️ 的三项**没有显式根**。它们大概率能从类/域/加载器到达，但没有被证明。
如果 re-entrant GC 触发后出现：

- `SIGSEGV`（stage 7/8，渲染线程）
- `Error #1009: null object reference` 突然集中在某个类
- 随机 `Gc` 已释放访问

那就是漏根，需要把 `Activation` 的这些字段注册进 `DynamicRootSet`（gc-arena 已提供）。

## 回退方式

- 关掉执行中 GC：删除 `activation.rs` 中 `debt >= 2_000_000.0` 那段；
- 恢复栈断言：把 `StackData::trace` 改回 `assert_eq!(stack_pointer, 0)`；
- 移除 `Cargo.toml` 的 `[patch.crates-io] gc-arena` 即可回到上游 gc-arena。

## 后续

1. 真机验证：战报能否在 2~3GB 内完成加载；
2. 若漏根：给 `Activation` 的 Gc 字段加动态根；
3. 若回收停顿明显：把 `Stop::FinishCycle` 改成按阶段推进（先 mark，再分几次 sweep）；
4. 继续做对象瘦身：字符串内联、`Array` 存储、`RegExp` 编译缓存。

## 补充：标记阶段遇到借用冲突（0.261058）

真机验证时出现过：

```
SXDffle PANIC at gc-arena/src/lock.rs:292: RefCell already mutably borrowed
```

原因是 `gc_arena` 的 `RefLock::trace` 直接 `borrow()`，而某些对象在标记期间
正被 `borrow_mut` 持有（脚本执行中触发回收时尤其容易撞上）。这会把整个渲染线程打死。

现在的处理：

- `RefLock::trace` 改用 `try_borrow()`；
- 失败时调用 `Trace::note_trace_conflict()`，由 `Context` 置位 `trace_conflict`；
- `do_collection` 在标记结束、准备进入 sweep 前检查该标志：
  **本轮有冲突就放弃 sweep**（不清任何对象），债务清零，等下一轮再试。

也就是说：宁可这一轮不回收，也不 panic、不误回收。

日志上表现为只有 `执行中 GC 开始` 而没有 `执行中 GC 完成`。
如果这种情况频繁出现，说明"安全点"还不够安全，需要缩小触发范围（例如只在
最外层 `run_actions` 的栈深度为 1 时回收）。

## 补充：AVM2 实际上有三套栈不在 GC 堆里（0.261059）

0.261058 的日志已经证明执行中回收能工作：

```
执行中 GC 开始：5813898 个对象（债务 2004027）
执行中 GC 完成：5813898 → 1533242 个对象（耗时 411ms）   ← 回收 73%
```

但紧接着 SIGSEGV。原因是 AVM2 有三个运行时结构**存放在 `Avm2` 结构里（Rust 内存）**，
而不是 GC 堆里：

| 结构 | 内容 | 是否参与 trace |
|---|---|---|
| `Avm2.stack` | 操作数栈 / 局部变量 | 否（`StackData` 是 Gc，但指针本身不在根里） |
| `Avm2.scope_stack` | 作用域栈 | 否 |
| `Avm2.call_stack` | 方法调用栈 | 否 |

上游之所以不需要 trace 它们，是因为**只在帧边界回收**，那时三者都为空。
脚本执行中回收时，它们就是根，必须显式注册。

实现：gc-arena 新增 `Mutation::push_extra_root_traced(&T)`，把任意 `Collect`
值里的顶层 Gc 指针收进 `Context.extra_roots`，标记时一并处理。
Ruffle 在每次执行中回收前注册：

```rust
mc.clear_extra_roots();
mc.push_extra_root_traced(&avm2.stack);
mc.push_extra_root_traced(&avm2.scope_stack);
mc.push_extra_root_traced(&avm2.call_stack);
mc.push_extra_root_traced(&self.outer);
mc.push_extra_root_traced(&self.caller_domain);
mc.push_extra_root_traced(&self.caller_movie);
mc.push_extra_root_traced(&self.bound_superclass_object);
unsafe { mc.collect_debt_reentrant() };
mc.clear_extra_roots();
```

如果仍然 SIGSEGV，说明还有父 Activation 的字段没覆盖（当前帧之外的调用者），
下一步是把 `Activation` 的 Gc 字段在构造时注册进 `DynamicRootSet`。

## 补充：整体注册 `Avm2`（0.261060）

0.261059 只注册了 `stack` / `scope_stack` / `call_stack`，仍然 SIGSEGV。
说明缺口不在这三套栈，而是 `Avm2` 的其它字段：`playerglobals_domain`、
`stage_domain`、`system_classes`、`system_class_defs`、`globals` 等——
它们同样只在 Rust 内存里，帧边界 GC 时通过 `Stage` 间接可达，
但脚本执行中回收时不一定。

现在改为整体注册：

```rust
mc.push_extra_root_traced(&*self.context.avm2);
```

`Avm2` 已经 `#[derive(Collect)]`，一次覆盖所有字段。当前 Activation 的
`outer` / `caller_domain` / `caller_movie` / `bound_superclass_object` 仍然单独注册。

若仍崩溃，最后一块是**父级 Activation**：递归调用时父帧的字段只在 Rust 栈上，
需要在 `Activation` 构造时注册进 `DynamicRootSet`，在 `cleanup()` 时释放。

## 补充：`UpdateContext` 的就近引用（0.261061）

整体注册 `Avm2` 之后仍然 SIGSEGV，说明还有"就近引用"没进根。
`UpdateContext` 里有两个典型的：

```rust
pub library: &'gc mut Library<'gc>,
pub root_swf: &'gc mut SwfMovie<'gc>,
```

它们是 Gc 包装，但只在这一帧的上下文里被直接持有；帧边界 GC 时能通过
`Stage` 间接到达，脚本执行中回收时不一定。

现已注册：

```rust
mc.push_extra_root_traced(&*self.context.library);
mc.push_extra_root_traced(&*self.context.root_swf);
```

`StringContext` 没有实现 `Collect`，暂时无法注册（它的 interner 随 `Avm2`
可达）。如果仍然崩溃，下一个嫌疑是父级 `Activation` 的 `outer`。
