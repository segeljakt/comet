use std::{
    marker::PhantomData,
    mem::size_of,
    ops::{Deref, DerefMut, Index, IndexMut},
    ptr::{null_mut, NonNull},
};

use crate::{large_space::PreciseAllocation, small_type_id, util::*};
use mopa::mopafy;
pub unsafe trait Trace {
    fn trace(&mut self, _vis: &mut dyn Visitor) {}
}

pub trait Collectable: Trace + Finalize + mopa::Any {
    #[inline(always)]
    fn allocation_size(&self) -> usize {
        std::mem::size_of_val(self)
    }
}

mopafy!(Collectable);

pub unsafe trait Finalize {
    unsafe fn finalize(&mut self) {
        std::ptr::drop_in_place(self)
    }
}

#[repr(C)]
pub struct HeapObjectHeader {
    pub value: u64,

    pub padding: u32,
    pub type_id: u32,
}

pub const MIN_ALLOCATION: usize = 16;

impl HeapObjectHeader {
    #[inline(always)]
    pub fn get_dyn(&mut self) -> &mut dyn Collectable {
        unsafe {
            std::mem::transmute::<_, _>(mopa::TraitObject {
                data: self.data() as *mut (),
                vtable: self.vtable() as _,
            })
        }
    }
    #[inline(always)]
    pub fn set_forwarded(&mut self, fwdptr: usize) {
        self.value = VTableBitField::update(self.value, fwdptr as _);
        self.value = ForwardedBit::update(self.value, 1);
    }
    #[inline(always)]
    pub fn is_forwarded(&self) -> bool {
        ForwardedBit::decode(self.value) != 0
    }

    #[inline(always)]
    pub fn size(&self) -> usize {
        SizeBitField::decode(self.value) as usize * MIN_ALLOCATION
    }
    #[inline(always)]
    pub fn is_precise(&self) -> bool {
        SizeBitField::decode(self.value) == 0
    }
    #[inline(always)]
    pub fn set_size(&mut self, size: usize) {
        assert!(size != 0);
        self.value = SizeBitField::update(self.value, size as u64 / MIN_ALLOCATION as u64);
    }
    #[inline(always)]
    pub fn set_large(&mut self) {
        self.value = SizeBitField::update(self.value, 0);
    }
    #[inline(always)]
    pub fn vtable(&self) -> usize {
        VTableBitField::decode(self.value) as _
    }
    #[inline(always)]
    pub fn set_vtable(&mut self, vtable: usize) {
        self.value = VTableBitField::encode(vtable as _);
    }
    #[inline(always)]
    pub fn is_allocated(&self) -> bool {
        self.vtable() != 0
    }
    #[inline(always)]
    pub fn data(&self) -> *const u8 {
        ((self as *const Self as usize) + size_of::<Self>()) as *const u8
    }
    #[inline(always)]
    pub fn marked_bit(&self) -> bool {
        MarkBit::decode(self.padding as _) != 0
    }

    pub fn unmark(&mut self) {
        self.padding = MarkBit::update(self.padding as _, 0) as _;
    }
    #[inline(always)]
    pub fn set_marked_bit(&mut self) {
        self.padding = MarkedBitField::update(self.padding as _, 1) as _;
    }
    #[inline(always)]
    pub fn type_id(&self) -> u32 {
        self.type_id
    }
}

/// A type that should be used to store GCed struct fields. It is not movable but dereferencable.
#[repr(transparent)]
pub struct Field<T: Collectable + ?Sized> {
    base: NonNull<HeapObjectHeader>,
    marker: PhantomData<T>,
}
impl<T: Collectable + ?Sized> Field<T> {
    pub fn as_dyn(&self) -> &dyn Collectable {
        unsafe {
            let trait_object = mopa::TraitObject {
                data: (*self.base.as_ptr()).data() as *mut (),
                vtable: (*self.base.as_ptr()).vtable() as *mut (),
            };

            std::mem::transmute(trait_object)
        }
    }
    pub fn as_dyn_mut(&mut self) -> &mut dyn Collectable {
        unsafe {
            let trait_object = mopa::TraitObject {
                data: (*self.base.as_ptr()).data() as *mut (),
                vtable: (*self.base.as_ptr()).vtable() as *mut (),
            };

            std::mem::transmute(trait_object)
        }
    }

    pub fn is<U: Collectable>(&self) -> bool {
        unsafe { (*self.base.as_ptr()).type_id == small_type_id::<U>() }
    }

    pub fn downcast<U: Collectable>(&self) -> Option<Gc<U>> {
        if self.is::<U>() {
            Some(Gc {
                base: self.base,
                marker: PhantomData,
            })
        } else {
            None
        }
    }

    pub fn to_dyn(&self) -> Gc<dyn Collectable> {
        Gc {
            base: self.base,
            marker: PhantomData,
        }
    }

    pub fn to_gc(&self) -> Gc<T> {
        Gc {
            base: self.base,
            marker: PhantomData,
        }
    }
}
impl<T: Collectable + Sized> Deref for Field<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe {
            let data = (*self.base.as_ptr()).data().cast::<T>();
            &*data
        }
    }
}

impl<T: Collectable + Sized> DerefMut for Field<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            let data = (*self.base.as_ptr()).data().cast::<T>() as *mut T;
            &mut *data
        }
    }
}

unsafe impl<T: Collectable + ?Sized> Trace for Field<T> {
    fn trace(&mut self, vis: &mut dyn Visitor) {
        vis.mark_object(&mut self.base);
    }
}

unsafe impl<T: Collectable + ?Sized> Finalize for Field<T> {}
unsafe impl<T: Collectable + ?Sized> Finalize for Gc<T> {}
impl<T: Collectable + ?Sized> Collectable for Gc<T> {}
pub(crate) fn vtable_of<T: Collectable>() -> usize {
    let x = null_mut::<T>();
    unsafe { std::mem::transmute::<_, mopa::TraitObject>(x as *mut dyn Collectable).vtable as _ }
}

pub struct Gc<T: Collectable + ?Sized> {
    pub(crate) base: NonNull<HeapObjectHeader>,
    pub(crate) marker: PhantomData<T>,
}

impl<T: Collectable + ?Sized> Gc<T> {
    pub fn to_field(self) -> Field<T> {
        Field {
            base: self.base,
            marker: self.marker,
        }
    }

    pub fn to_dyn(self) -> Gc<dyn Collectable> {
        Gc {
            base: self.base,
            marker: PhantomData,
        }
    }

    /// Returns "fake" unprotected handle.
    ///
    /// # Safety
    ///
    /// This function is unsafe because GC might happen when this handle is still alive but not protected by GC.
    ///
    pub unsafe fn fake_handle(&self) -> Handle<'_, T> {
        Handle { handle: self }
    }
    /// Returns "fake" unprotected handle.
    ///
    /// # Safety
    ///
    /// This function is unsafe because GC might happen when this handle is still alive but not protected by GC.
    ///
    pub unsafe fn fake_handle_mut(&mut self) -> HandleMut<'_, T> {
        HandleMut { handle: self }
    }
    #[inline(always)]
    pub fn is<U: Collectable>(&self) -> bool {
        unsafe { (*self.base.as_ptr()).type_id == small_type_id::<U>() }
    }

    pub fn vtable(&self) -> usize {
        unsafe { (*self.base.as_ptr()).vtable() }
    }

    pub fn downcast<U: Collectable>(&self) -> Option<Gc<U>> {
        if self.is::<U>() {
            Some(Gc {
                base: self.base,
                marker: PhantomData,
            })
        } else {
            None
        }
    }

    pub fn allocation_size(&self) -> usize {
        unsafe {
            let base = &*self.base.as_ptr();
            if base.is_precise() {
                (*PreciseAllocation::from_cell(self.base.as_ptr() as *mut _)).cell_size()
            } else {
                base.size()
            }
        }
    }
}

impl<T: Collectable + ?Sized> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Collectable + ?Sized> Copy for Gc<T> {}

/// Shadow stack implementation. Internally this is singly-linked list of on stack rooted values.
pub struct ShadowStack {
    #[doc(hidden)]
    pub head: core::cell::Cell<*mut RawShadowStackEntry>,
}
impl ShadowStack {
    /// Create new shadow stack instance.
    pub fn new() -> Self {
        Self {
            head: core::cell::Cell::new(core::ptr::null_mut()),
        }
    }
    /// Walk all rooted values in this shadow stack.
    ///
    /// # Safety
    /// TODO: I don't really know if this method should be safe or unsafe.
    ///
    pub unsafe fn walk(&self, mut visitor: impl FnMut(&mut dyn Rootable)) {
        let mut head = *self.head.as_ptr();
        while !head.is_null() {
            let next = (*head).prev;
            visitor((*head).get_dyn());
            head = next;
        }
    }
}
/// Raw entry in GC shadow stack. Internal fields is not exposed in public API in any ways.
///
///
/// This type internally stores shadow stack pointeter,previous pointer from the list and vtable
/// that is used to construct `dyn` trait.
///
#[repr(C)]
pub struct RawShadowStackEntry {
    /// Shadowstack itself
    stack: *mut ShadowStack,
    /// Previous rooted entry
    prev: *mut RawShadowStackEntry,
    /// Pointer to vtable that is a `Trace` of rooted variable
    vtable: usize,
    /// Value is located right after vtable pointer, to access it we can construct trait object.
    data_start: [u8; 0],
}
/// Trait that should be implemented for all types that could be rooted.
/// In simple cases `impl<T: Traceable> Rootable for T {}` is enough.
pub trait Rootable: Trace {}
impl RawShadowStackEntry {
    /// Obtain mutable reference to rooted value.
    ///
    /// # Safety
    /// This method is `&self` but returns `&mut dyn` which is *very* unsafey. If moving GC uses shadow stack
    /// it should be ***very*** accurate when moving objects around.
    pub unsafe fn get_dyn(&self) -> &mut dyn Rootable {
        core::mem::transmute(crate::mopa::TraitObject {
            vtable: self.vtable as _,
            data: self.data_start.as_ptr() as *mut (),
        })
    }
}
/// Almost the same as raw entry of shadow stack except this one gives access to value.
/// This type is not exposed in public API and used only internally.
#[repr(C)]
pub struct ShadowStackInternal<'a, T: Rootable> {
    pub stack: &'a ShadowStack,
    pub prev: *mut RawShadowStackEntry,
    pub vtable: usize,
    pub value: T,
}
impl<'a, T: Rootable> ShadowStackInternal<'a, T> {
    #[doc(hidden)]
    /// Constructs internal shadow stack value. Must not be used outside of `$letroot!` macro.
    ///
    /// # Safety
    ///
    /// Very unsafe function and must not be used by users!
    #[inline]
    pub unsafe fn construct(
        stack: &'a ShadowStack,
        prev: *mut RawShadowStackEntry,
        vtable: usize,
        value: T,
    ) -> Self {
        Self {
            stack,
            prev,
            vtable,
            value,
        }
    }
}
impl<T: Rootable> Drop for ShadowStackInternal<'_, T> {
    /// Drop current shadow stack entry and update shadow stack state.
    fn drop(&mut self) {
        (*self.stack).head.set(self.prev);
    }
}
/// Rooted value on stack. This is non-copyable type that is used to hold GC thing on stack.
pub struct Rooted<'a, 'b, T: Rootable> {
    #[doc(hidden)]
    pinned: core::pin::Pin<&'a mut ShadowStackInternal<'b, T>>,
}
impl<'a, 'b, T: Rootable> Rooted<'a, 'b, T> {
    /// Create rooted value from pinned reference. Note that this function must be used only
    /// inside `$letroot` macro.
    ///
    /// # Safety
    ///
    ///  Very unsafe function and must not be used by users!
    pub unsafe fn construct(pin: core::pin::Pin<&'a mut ShadowStackInternal<'b, T>>) -> Self {
        Self { pinned: pin }
    }
    /// Get internal rooted handle
    ///
    /// # Safety
    ///
    /// Very unsafe and should be used only by Deref and DerefMut impls
    pub unsafe fn get_internal(&self) -> &ShadowStackInternal<T> {
        core::mem::transmute_copy::<_, _>(&self.pinned)
    }
    /// Get internal rooted handle
    ///
    /// # Safety
    ///
    /// Very unsafe and should be used only by Deref and DerefMut impls
    pub unsafe fn get_internal_mut(&mut self) -> &mut &ShadowStackInternal<T> {
        core::mem::transmute_copy::<_, _>(&self.pinned)
    }
}
impl<'a, T: Rootable> core::ops::Deref for Rooted<'a, '_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.pinned.value
    }
}
impl<'a, T: Rootable> core::ops::DerefMut for Rooted<'a, '_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            &mut core::mem::transmute_copy::<_, &mut ShadowStackInternal<T>>(&mut self.pinned).value
        }
    }
}
macro_rules! impl_prim {
    ($($t: ty)*) => {
        $(
            unsafe impl Trace for $t {}
            unsafe impl Finalize for $t {}
            impl Collectable for $t {}
        )*
    };
}

impl_prim!(
    u8 u16 u32 u64 u128
    i8 i16 i32 i64 i128
    f32 f64
    bool
    std::fs::File String
);

impl<T: Trace> Rootable for T {}
impl<T: Rootable> std::fmt::Pointer for Rooted<'_, '_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:p}", self.pinned)
    }
}

pub struct Handle<'a, T: Collectable + ?Sized> {
    handle: &'a Gc<T>,
}

impl<'a, T: Collectable + ?Sized> Handle<'a, T> {
    /// Returns Gc<T>
    pub fn gc(&self) -> Gc<T> {
        *self.handle
    }
    #[inline]
    pub fn downcast<U: Collectable + Sized>(self) -> Result<Handle<'a, U>, Self> {
        if self.gc().is::<U>() {
            Ok(Handle {
                handle: unsafe { std::mem::transmute(self.handle) },
            })
        } else {
            Err(self)
        }
    }
    #[inline]
    pub fn to_dyn(self) -> Handle<'a, dyn Collectable> {
        unsafe {
            Handle {
                handle: std::mem::transmute(self.handle),
            }
        }
    }
    #[inline]
    pub fn get_dyn(&self) -> &dyn Collectable {
        unsafe { (*self.handle.base.as_ptr()).get_dyn() }
    }
}

impl<'a, T: Collectable> Deref for Handle<'a, T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe {
            let data = (*self.handle.base.as_ptr()).data().cast::<T>();
            &*data
        }
    }
}

pub struct HandleMut<'a, T: Collectable + ?Sized> {
    handle: &'a mut Gc<T>,
}

impl<'a, T: Collectable + ?Sized> HandleMut<'a, T> {
    #[inline(always)]
    /// Assigns new GC pointer to this Handle.
    pub fn write(&mut self, val: Gc<T>) {
        *self.handle = val;
    }
    #[inline(always)]
    /// Returns Gc<T>
    pub fn gc(&self) -> Gc<T> {
        *self.handle
    }
    #[inline(always)]
    pub fn downcast<U: Collectable + Sized>(self) -> Result<HandleMut<'a, U>, Self> {
        if self.gc().is::<U>() {
            Ok(HandleMut {
                handle: unsafe { std::mem::transmute(self.handle) },
            })
        } else {
            Err(self)
        }
    }
    #[inline(always)]
    pub fn to_dyn(self) -> HandleMut<'a, dyn Collectable> {
        unsafe {
            HandleMut {
                handle: std::mem::transmute(self.handle),
            }
        }
    }
    #[inline(always)]
    pub fn get_dyn(&self) -> &dyn Collectable {
        unsafe { (*self.handle.base.as_ptr()).get_dyn() }
    }
    #[inline(always)]
    pub fn get_dyn_mut(&mut self) -> &mut dyn Collectable {
        unsafe { (*self.handle.base.as_ptr()).get_dyn() }
    }
}

impl<'a, T: Collectable> Deref for HandleMut<'a, T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe {
            let data = (*self.handle.base.as_ptr()).data().cast::<T>();
            &*data
        }
    }
}
impl<'a, T: Collectable> DerefMut for HandleMut<'a, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            let data = (*self.handle.base.as_ptr()).data().cast::<T>() as *mut T;
            &mut *data
        }
    }
}

impl<'a, 'b, T: Collectable + ?Sized> Rooted<'a, 'b, Gc<T>> {
    #[inline(always)]
    pub fn handle(&mut self) -> Handle<'_, T> {
        Handle {
            handle: &mut **self,
        }
    }
    #[inline(always)]
    pub fn handle_mut(&mut self) -> HandleMut<'_, T> {
        HandleMut {
            handle: &mut **self,
        }
    }
}

unsafe impl<T: Collectable + ?Sized> Trace for Gc<T> {
    fn trace(&mut self, vis: &mut dyn Visitor) {
        vis.mark_object(&mut self.base);
    }
}

impl<T: Collectable + ?Sized> std::fmt::Pointer for Handle<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Handle({:p}->{:p})", self.handle, *self.handle)
    }
}

impl<T: Collectable + ?Sized> std::fmt::Pointer for HandleMut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HandleMut({:p}->{:p})", self.handle, *self.handle)
    }
}

/// Create rooted value and push it to provided shadowstack instance.
///
///
/// ***NOTE***: This macro does not heap allocate internally. It uses some unsafe tricks to
/// allocate value on stack and push stack reference to shadowstack. Returned rooted value internally
/// is `Pin<&mut T>`.
///
#[macro_export]
macro_rules! letroot {
    ($var_name: ident: $t: ty  = $stack: expr,$value: expr) => {
        let stack: &$crate::api::ShadowStack = &$stack;
        let value = $value;
        let mut $var_name = unsafe {
            $crate::api::ShadowStackInternal::<$t>::construct(
                stack,
                stack.head.get(),
                core::mem::transmute::<_, $crate::mopa::TraitObject>(
                    &value as &dyn $crate::api::Rootable,
                )
                .vtable as usize,
                value,
            )
        };

        stack
            .head
            .set(unsafe { core::mem::transmute(&mut $var_name) });
        #[allow(unused_mut)]
        let mut $var_name =
            unsafe { $crate::api::Rooted::construct(std::pin::Pin::new(&mut $var_name)) };
    };

    ($var_name : ident = $stack: expr,$value: expr) => {
        let stack: &$crate::api::ShadowStack = &$stack;
        let value = $value;
        let mut $var_name = unsafe {
            $crate::api::ShadowStackInternal::<_>::construct(
                stack,
                stack.head.get(),
                core::mem::transmute::<_, $crate::mopa::TraitObject>(
                    &value as &dyn $crate::api::Rootable,
                )
                .vtable as usize,
                value,
            )
        };

        stack
            .head
            .set(unsafe { core::mem::transmute(&mut $var_name) });
        #[allow(unused_mut)]
        let mut $var_name =
            unsafe { $crate::api::Rooted::construct(core::pin::Pin::new(&mut $var_name)) };
    };
}

impl<T: Collectable + Sized> Rooted<'_, '_, Gc<T>> {
    /// Get `&T` from Gc<T>
    pub fn get(&self) -> &T {
        unsafe {
            let data = (*self.pinned.value.base.as_ptr()).data().cast::<T>();
            &*data
        }
    }
    // Get `&mut T` from Gc<T>
    pub fn get_mut(&self) -> &T {
        unsafe {
            let data = (*self.pinned.value.base.as_ptr()).data().cast::<T>() as *mut T;
            &*data
        }
    }
}

pub trait Visitor {
    fn mark_object(&mut self, root: &mut NonNull<HeapObjectHeader>);
}

impl<T: Collectable + ?Sized> std::fmt::Pointer for Gc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:p}", self.base)
    }
}

pub trait GcIndex<'a, IDX> {
    type Output: 'a;
    fn at(&self, index: IDX) -> &'a Self::Output;
}

pub trait GcIndexMut<'a, IDX>: GcIndex<'a, IDX> {
    fn at_mut(&mut self, index: IDX) -> &'a mut Self::Output;
}

impl<'a, I, T: Collectable + GcIndex<'a, I>> Index<I> for Handle<'a, T> {
    type Output = <T as GcIndex<'a, I>>::Output;
    fn index(&self, index: I) -> &Self::Output {
        self.at(index)
    }
}

impl<'a, I, T: Collectable + GcIndex<'a, I>> Index<I> for HandleMut<'a, T> {
    type Output = <T as GcIndex<'a, I>>::Output;
    fn index(&self, index: I) -> &Self::Output {
        self.at(index)
    }
}

impl<'a, I, T: Collectable + GcIndexMut<'a, I> + GcIndex<'a, I>> IndexMut<I> for HandleMut<'a, T> {
    fn index_mut(&mut self, index: I) -> &'a mut Self::Output {
        self.at_mut(index)
    }
}

unsafe impl Trace for &mut [&mut dyn Trace] {
    fn trace(&mut self, _vis: &mut dyn Visitor) {
        for x in self.iter_mut() {
            x.trace(_vis);
        }
    }
}

unsafe impl<T: Trace> Trace for Option<T> {
    fn trace(&mut self, _vis: &mut dyn Visitor) {
        match self {
            Some(val) => val.trace(_vis),
            _ => (),
        }
    }
}