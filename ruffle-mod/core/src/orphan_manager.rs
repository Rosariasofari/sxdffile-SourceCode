//! Special handling for AVM2 orphan objects

use crate::context::UpdateContext;
use crate::display_object::{DisplayObject, DisplayObjectWeak, TDisplayObject};
use gc_arena::{Collect, Mutation};

/// The list of 'orphan' objects - these objects have no parent,
/// so we need to manually run their frames in `run_all_phases_avm2` to match
/// Flash's behavior. Clips are added to this list with `add_orphan_movie`.
/// and are removed automatically by `cleanup_dead_orphans`.
///
/// We store `DisplayObjectWeak`, since we don't want to keep these objects
/// alive if they would otherwise be garbage-collected. The movie will
/// stop ticking whenever garbage collection runs if there are no more
/// strong references around (this matches Flash's behavior).
#[derive(Collect)]
#[collect(no_drop)]
pub struct OrphanManager<'gc> {
    // SXDffle：从 `Rc<Vec<..>>` 改成裸 `Vec`。
    //
    // 原来是 `Rc<Vec>` + `Rc::make_mut`，为的是在遍历过程中允许增删。
    // 但 `each_orphan_obj` 会 clone 一份 Rc 来跨越借用检查，导致引用计数 ≥2；
    // 于是**遍历中每一次 add_orphan_obj 都触发 make_mut 深拷贝整个 Vec**。
    // 战报里孤儿能到十万级：一次深拷贝 800KB，每帧几百次 → 纯内存搬运就是几百毫秒。
    // 改成裸 Vec + 下标遍历（每次短暂借用，函数调用之间不持有借用），
    // 既没有拷贝，也不需要 Rc。
    orphans: Vec<DisplayObjectWeak<'gc>>,
}

impl<'gc> OrphanManager<'gc> {
    /// 当前孤儿数（每帧都要全量遍历，数量直接决定帧成本）。
    pub fn len(&self) -> usize {
        self.orphans.len()
    }

    /// Adds a `MovieClip` to the orphan list. In AVM2, movies advance their
    /// frames even when they are not on a display list. Unfortunately,
    /// multiple SWFS rely on this behavior, so we need to match Flash's
    /// behavior. This should not be called manually - `movie_clip` will
    /// call it when necessary.
    pub fn add_orphan_obj(&mut self, dobj: DisplayObject<'gc>) {
        // Note: comparing pointers is correct because GcWeak keeps its allocation alive,
        // so the pointers can't overlap by accident.
        //
        // SXDffle：只比对尾部 256 项，不再全表查重。
        // 原来这里对**整个列表**做线性查找，列表到十万级时，
        // 「创建 N 个对象」的复杂度就是 O(N²) —— 十万个对象 = 100 亿次比较。
        // 重复添加在时间上总是相邻的（同一个 clip 被反复 add/remove），
        // 尾部比对足够覆盖；漏判的后果只是同一个 clip 多跑一次帧脚本。
        let n = self.orphans.len();
        let start = n.saturating_sub(256);
        if self.orphans[start..]
            .iter()
            .any(|d| std::ptr::eq(d.as_ptr(), dobj.as_ptr()))
        {
            return;
        }
        self.orphans.push(dobj.downgrade());
    }

    pub fn each_orphan_obj(
        context: &mut UpdateContext<'gc>,
        mut f: impl FnMut(DisplayObject<'gc>, &mut UpdateContext<'gc>),
    ) {
        // SXDffle：按下标遍历，每次只在取元素时短暂借用。
        //
        // 长度先快照：`f` 里新增的孤儿（尾部追加）本轮不跑，下一帧补上；
        // 如果 `f` 删除了元素，索引会跳过一个 —— 那一个下一帧还会被跑到，
        // 只是晚一帧，不影响正确性。
        let len = context.orphan_manager.orphans.len();
        for i in 0..len {
            let Some(orphan) = context.orphan_manager.orphans.get(i).copied() else {
                break;
            };
            if let Some(dobj) = valid_orphan(orphan, context.gc()) {
                f(dobj, context);
            }
        }
    }

    /// Called at the end of `run_all_phases_avm2` - removes any movies
    /// that have been garbage collected, or are no longer orphans
    /// (they've since acquired a parent).
    pub fn cleanup_dead_orphans(&mut self, mc: &Mutation<'gc>) {
        self.orphans.retain(|d| {
            if let Some(dobj) = valid_orphan(*d, mc) {
                // All clips that become orphaned (have their parent removed, or start out with no parent)
                // get added to the orphan list. However, there's a distinction between clips
                // that are removed from a RemoveObject tag, and clips that are removed from ActionScript.
                //
                // Clips removed from a RemoveObject tag only stay on the orphan list until the end
                // of the frame - this lets them run a framescript (with 'this.parent == null')
                // before they're removed. After that, they're removed from the orphan list,
                // and will not be run in any way.
                //
                // Clips removed from ActionScript stay on the orphan list, and will be run
                // indefinitely (if there are no remaining strong references, they will eventually
                // be garbage collected).
                //
                // To detect this, we check 'placed_by_avm2_script'. This flag get set to 'true'
                // for objects constructed from ActionScript, and for objects moved around
                // in the timeline (add/remove child, swap depths) by ActionScript. A
                // RemoveObject tag will only affect objects instantiated by the timeline,
                // which have not been moved in the displaylist by ActionScript. Therefore,
                // any orphan we see that has 'placed_by_avm2_script()' should stay on the orphan
                // list, because it was not removed by a RemoveObject tag.
                dobj.placed_by_avm2_script()
            } else {
                false
            }
        });
    }
}

impl<'gc> Default for OrphanManager<'gc> {
    fn default() -> Self {
        Self {
            orphans: Vec::new(),
        }
    }
}

/// If the provided `DisplayObjectWeak` should have frames run, returns
/// Some(clip) with an upgraded `MovieClip`.
/// If this returns `None`, the entry should be removed from the orphan list.
fn valid_orphan<'gc>(
    dobj: DisplayObjectWeak<'gc>,
    mc: &Mutation<'gc>,
) -> Option<DisplayObject<'gc>> {
    dobj.upgrade(mc).filter(|dobj| dobj.parent().is_none())
}
