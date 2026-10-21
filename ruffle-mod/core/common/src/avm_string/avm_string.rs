use super::context::StringContext;
use super::interner::AvmAtom;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 动态字符串分配总数（SXDffle 诊断用）。
/// Ruffle 的每个 AvmString 都是独立 Gc，页游大量拼接/数字转字符串时
/// 会制造海量短命对象，这里单独计量。
static STRING_ALLOCS: AtomicUsize = AtomicUsize::new(0);

/// 迄今为止分配过的 AvmString 数量（含已回收）。
pub fn string_alloc_total() -> usize {
    STRING_ALLOCS.load(Ordering::Relaxed)
}
use super::repr::AvmStringRepr;

use gc_arena::{Collect, Gc, Mutation};
use ruffle_wstr::{Pattern, WStr, WString, wstr_impl_traits};
use std::borrow::Cow;
use std::ops::Deref;

#[derive(Clone, Copy, Collect)]
#[collect(no_drop)]
pub struct AvmString<'gc>(Gc<'gc, AvmStringRepr<'gc>>);

impl<'gc> AvmString<'gc> {
    /// Turns a string to a fully owned (non-dependent) managed string.
    pub(super) fn to_fully_owned(self, mc: &Mutation<'gc>) -> Gc<'gc, AvmStringRepr<'gc>> {
        if self.0.is_dependent() {
            let repr = AvmStringRepr::from_raw(WString::from(self.as_wstr()), false);
            Gc::new(mc, repr)
        } else {
            self.0
        }
    }

    pub fn new_ascii_static(gc_context: &Mutation<'gc>, bytes: &'static [u8]) -> Self {
        let repr = AvmStringRepr::from_raw_static(WStr::from_units(bytes), false);
        STRING_ALLOCS.fetch_add(1, Ordering::Relaxed);
        Self(Gc::new(gc_context, repr))
    }

    pub fn new_utf8<'s, S: Into<Cow<'s, str>>>(gc_context: &Mutation<'gc>, string: S) -> Self {
        let buf = match string.into() {
            Cow::Owned(utf8) => WString::from_utf8_owned(utf8),
            Cow::Borrowed(utf8) => WString::from_utf8(utf8),
        };
        let repr = AvmStringRepr::from_raw(buf, false);
        STRING_ALLOCS.fetch_add(1, Ordering::Relaxed);
        Self(Gc::new(gc_context, repr))
    }

    pub fn new_utf8_bytes(gc_context: &Mutation<'gc>, bytes: &[u8]) -> Self {
        let buf = WString::from_utf8_bytes(bytes.to_vec());
        Self::new(gc_context, buf)
    }

    pub fn new<S: Into<WString>>(gc_context: &Mutation<'gc>, string: S) -> Self {
        let repr = AvmStringRepr::from_raw(string.into(), false);
        STRING_ALLOCS.fetch_add(1, Ordering::Relaxed);
        Self(Gc::new(gc_context, repr))
    }

    pub fn substring(mc: &Mutation<'gc>, string: AvmString<'gc>, start: usize, end: usize) -> Self {
        let repr = AvmStringRepr::new_dependent(string.0, start, end);
        STRING_ALLOCS.fetch_add(1, Ordering::Relaxed);
        Self(Gc::new(mc, repr))
    }

    pub fn is_dependent(&self) -> bool {
        self.0.is_dependent()
    }

    pub fn as_wstr(&self) -> &'gc WStr {
        Gc::as_ref(self.0).as_wstr()
    }

    pub fn as_interned(&self) -> Option<AvmAtom<'gc>> {
        if self.0.is_interned() {
            Some(AvmAtom(self.0))
        } else {
            None
        }
    }

    pub fn concat(
        mc: &Mutation<'gc>,
        left: AvmString<'gc>,
        right: AvmString<'gc>,
    ) -> AvmString<'gc> {
        if left.is_empty() {
            right
        } else if right.is_empty() {
            left
        } else if let Some(repr) = AvmStringRepr::try_append_inline(left.0, &right) {
            Self(Gc::new(mc, repr))
        } else {
            // When doing a non-in-place append,
            // Overallocate a bit so that further appends can be in-place.
            // (Note that this means that all first-time appends will happen here and
            // overallocate, even if done only once)
            // This growth logic should be equivalent to AVM's, except I capped the growth at 1MB instead of 4MB.
            let new_size = left.len() + right.len();
            let new_capacity = if new_size < 32 {
                32
            } else if new_size > 1024 * 1024 {
                new_size + 1024 * 1024
            } else {
                new_size * 2
            };

            // SXDffle：`s += x` 的每次拼接都新建一整块缓冲，旧的那块要等 GC。
            // 一帧内跑几万次拼接就是几百 MB 的「活的垃圾」—— 这里单独打标，
            // 好确认战报峰值里那 1.5GB 的 >8MB 大块到底是不是它。
            let _sxd_tag = crate::alloc_tag::Guard::new(11);
            let mut out = WString::with_capacity(new_capacity, left.is_wide() || right.is_wide());
            out.push_str(&left);
            out.push_str(&right);
            Self::new(mc, out)
        }
    }

    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        std::ptr::eq(this.as_wstr(), other.as_wstr())
    }

    /// Like [`WStr::split`], but yields dependent `AvmString`s
    pub fn split_dependent<'a, P>(
        self,
        context: &'a StringContext<'gc>,
        pattern: P,
    ) -> impl Iterator<Item = AvmString<'gc>> + 'a
    where
        P: Pattern<'gc> + 'a,
    {
        self.as_wstr()
            .split_indices(pattern)
            .map(move |range| context.substring(self, range))
    }
}

impl<'gc> From<AvmAtom<'gc>> for AvmString<'gc> {
    #[inline]
    fn from(atom: AvmAtom<'gc>) -> Self {
        Self(atom.0)
    }
}

impl<'gc> From<Gc<'gc, AvmStringRepr<'gc>>> for AvmString<'gc> {
    #[inline]
    fn from(repr: Gc<'gc, AvmStringRepr<'gc>>) -> Self {
        Self(repr)
    }
}

impl Deref for AvmString<'_> {
    type Target = WStr;
    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_wstr()
    }
}

// Manual equality implementation with fast paths for owned strings.
impl PartialEq for AvmString<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        if Gc::ptr_eq(self.0, other.0) {
            // Fast accept for identical strings.
            true
        } else if self.0.is_interned() && other.0.is_interned() {
            // Fast reject for distinct interned strings.
            false
        } else {
            // Fallback case.
            self.as_wstr() == other.as_wstr()
        }
    }
}

impl<'gc> PartialEq<AvmString<'gc>> for AvmAtom<'gc> {
    fn eq(&self, other: &AvmString<'gc>) -> bool {
        if let Some(atom) = other.as_interned() {
            *self == atom
        } else {
            self.as_wstr() == other.as_wstr()
        }
    }
}

impl<'gc> PartialEq<AvmAtom<'gc>> for AvmString<'gc> {
    #[inline(always)]
    fn eq(&self, other: &AvmAtom<'gc>) -> bool {
        PartialEq::eq(other, self)
    }
}

impl Eq for AvmString<'_> {}

wstr_impl_traits!(impl['gc] manual_eq for AvmString<'gc>);
