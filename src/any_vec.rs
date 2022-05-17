use std::{mem, ptr};
use std::alloc::{alloc, dealloc, Layout, realloc, handle_alloc_error};
use std::any::TypeId;
use std::ptr::{NonNull};
use crate::{AnyVecMut, AnyVecRef, AnyVecTyped, copy_bytes, swap_bytes};

/// Type erased vec-like container.
/// All elements have the same type.
///
/// Only destruct operations have indirect call overhead.
///
/// *`Element: 'static` due to TypeId requirements*
pub struct AnyVec {
    mem: NonNull<u8>,
    capacity: usize,        // in elements
    len: usize,             // in elements
    element_layout: Layout, // size is aligned
    type_id: TypeId,        // purely for safety checks
    drop_fn: Option<fn(ptr: *mut u8, len: usize)>
}

impl AnyVec {
    pub fn new<Element: 'static>() -> Self {
        Self::with_capacity::<Element>(0)
    }

    pub fn with_capacity<Element: 'static>(capacity: usize) -> Self {
        let mut this = Self{
            mem: NonNull::<u8>::dangling(),
            capacity: 0,
            len: 0,
            element_layout: Layout::new::<Element>(),
            type_id: TypeId::of::<Element>(),
            drop_fn:
                if !mem::needs_drop::<Element>(){
                    None
                } else{
                    Some(|mut ptr: *mut u8, len: usize|{
                        for _ in 0..len{
                            unsafe{
                                ptr::drop_in_place(ptr as *mut Element);
                                ptr = ptr.add(mem::size_of::<Element>());
                            }
                        }
                    })
                }
        };
        this.set_capacity(capacity);
        this
    }

    #[inline]
    pub fn downcast_ref<Element: 'static>(&self) -> Option<AnyVecRef<Element>> {
        if self.type_id == TypeId::of::<Element>() {
            unsafe{ Some(self.downcast_ref_unchecked()) }
        } else {
            None
        }
    }

    #[inline]
    pub unsafe fn downcast_ref_unchecked<Element: 'static>(&self) -> AnyVecRef<Element> {
        AnyVecRef{
            any_vec_typed: (AnyVecTyped::new(
                NonNull::new_unchecked(self as *const _ as *mut _)
            ))
        }
    }

    #[inline]
    pub fn downcast_mut<Element: 'static>(&mut self) -> Option<AnyVecMut<Element>> {
        if self.type_id == TypeId::of::<Element>() {
            unsafe{ Some(self.downcast_mut_unchecked()) }
        } else {
            None
        }
    }

    #[inline]
    pub unsafe fn downcast_mut_unchecked<Element: 'static>(&mut self) -> AnyVecMut<Element> {
        AnyVecMut{
            any_vec_typed: AnyVecTyped::new(NonNull::new_unchecked(self))
        }
    }

    fn set_capacity(&mut self, new_capacity: usize){
        // Never cut
        debug_assert!(self.len <= new_capacity);

        if self.capacity == new_capacity {
            return;
        }

        if self.element_layout.size() != 0 {
            unsafe{
                let mem_layout = Layout::from_size_align_unchecked(
                    self.element_layout.size() * self.capacity, self.element_layout.align()
                );

                self.mem =
                    if new_capacity == 0 {
                        dealloc(self.mem.as_ptr(), mem_layout);
                        NonNull::<u8>::dangling()
                    } else {
                        // mul carefully, to prevent overflow.
                        let new_mem_size = self.element_layout.size().checked_mul(new_capacity).unwrap();
                        let new_mem_layout = Layout::from_size_align_unchecked(
                            new_mem_size, self.element_layout.align()
                        );

                        if self.capacity == 0 {
                            // allocate
                            NonNull::new(alloc(new_mem_layout))
                        } else {
                            // reallocate
                            NonNull::new(realloc(
                                self.mem.as_ptr(), mem_layout,new_mem_size
                            ))
                        }
                        .unwrap_or_else(|| handle_alloc_error(new_mem_layout))
                    }
            }
        }
        self.capacity = new_capacity;
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self){
        self.set_capacity(
             if self.capacity == 0 {2} else {self.capacity * 2}
        );
    }

    /// Pushes one element without actually writing anything.
    ///
    /// Return byte slice, that must be filled with element data.
    ///
    /// # Safety
    /// This is highly unsafe, due to the number of invariants that aren’t checked:
    /// * returned byte slice must be written with actual Element bytes.
    /// * Element bytes must be aligned.
    /// * Element must be "forgotten".
    #[inline]
    pub unsafe fn push_uninit(&mut self) -> &mut[u8] {
        if self.len == self.capacity{
            self.grow();
        }

        let new_element = self.mem.as_ptr().add(self.element_layout.size() * self.len);
        self.len += 1;

        std::slice::from_raw_parts_mut(
            new_element,
            self.element_layout.size(),
        )
    }

    #[inline]
    fn drop_element(&mut self, ptr: *mut u8, len: usize){
        if let Some(drop_fn) = self.drop_fn{
            (drop_fn)(ptr, len);
        }
    }

    /// Type erased version of [`Vec::swap_remove`]. Due to this, does not return element.
    ///
    /// # Panics
    ///
    /// Panics if index is out of bounds.
    #[inline]
    pub fn swap_remove(&mut self, index: usize) {
    unsafe{
        assert!(index < self.len);

        let element = self.mem.as_ptr().add(self.element_layout.size() * index);

        // 1. swap elements
        let last_index = self.len - 1;
        let last_element = self.mem.as_ptr().add(self.element_layout.size() * last_index);
        if index != last_index {
            if self.drop_fn.is_none(){
                copy_bytes(last_element, element, self.element_layout.size());
            } else {
                swap_bytes(last_element, element, self.element_layout.size());
            }
        }

        // 2. shrink len
        self.len -= 1;

        // 3. drop last
        self.drop_element(last_element, 1);
    }
    }

    /// drop element, if out is null.
    /// element_size as parameter - because it possible can be known at compile time
    #[inline]
    pub(crate) unsafe fn swap_take_bytes_impl(&mut self, index: usize, element_size: usize, out: *mut u8)
    {
        assert!(index < self.len, "Index out of range!");

        // 1. move out element at index
        let element = self.mem.as_ptr().add(element_size * index);
        ptr::copy_nonoverlapping(element, out, element_size);

        // 2. move element
        let last_index = self.len - 1;
        if index != last_index {
            let last_element = self.mem.as_ptr().add(element_size * last_index);
            ptr::copy_nonoverlapping(last_element, element, element_size);
        }

        // 3. shrink len
        self.len -= 1;
    }

    /// Same as [`swap_remove`], but copy removed element as bytes to `out`.
    ///
    /// # Safety
    /// * It is your responsibility to properly drop `out` element.
    ///
    /// # Panics
    /// * Panics if index out of bounds.
    /// * Panics if out len does not match element size.
    ///
    /// [`swap_remove`]: Self::swap_remove
    #[inline]
    pub unsafe fn swap_remove_into(&mut self, index: usize, out: &mut[u8]){
        assert_eq!(out.len(), self.element_layout.size());  // This allows compile time optimization!
        self.swap_take_bytes_impl(index, self.element_layout.size(), out.as_mut_ptr());
    }

    #[inline]
    pub fn clear(&mut self){
        let len = self.len;

        // Prematurely set the length to zero so that even if dropping the values panics users
        // won't be able to access the dropped values.
        self.len = 0;

        self.drop_element(self.mem.as_ptr(), len);
    }

    #[inline]
    pub(crate) unsafe fn as_slice_unchecked<T>(&self) -> &[T]{
        std::slice::from_raw_parts(
            self.mem.as_ptr().cast::<T>(),
            self.len,
        )
    }

    #[inline]
    pub(crate) unsafe fn as_mut_slice_unchecked<T>(&mut self) -> &mut[T]{
        std::slice::from_raw_parts_mut(
            self.mem.as_ptr().cast::<T>(),
            self.len,
        )
    }

    /// Element TypeId
    #[inline]
    pub fn element_typeid(&self) -> TypeId{
        self.type_id
    }

    /// Element Layout
    #[inline]
    pub fn element_layout(&self) -> Layout {
        self.element_layout
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Drop for AnyVec {
    fn drop(&mut self) {
        self.clear();
        self.set_capacity(0);
    }
}