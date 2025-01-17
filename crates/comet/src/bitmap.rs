use crate::api::HeapObjectHeader;
use crate::api::MIN_ALLOCATION;
use crate::immix::block::IMMIX_LINE_SIZE;
use crate::immix::chunk::CHUNK_SIZE;
use crate::utils::mmap::Mmap;
use atomic::Atomic;
use atomic::Ordering;
use core::fmt;
use std::mem::size_of;
use std::ptr::null_mut;
#[inline(always)]
pub const fn round_down(x: u64, n: u64) -> u64 {
    let x = x as i64;
    let n = n as i64;
    (x & -n) as _
}

#[inline(always)]
pub const fn round_up(x: u64, n: u64) -> u64 {
    round_down(x.wrapping_add(n).wrapping_sub(1), n)
    //round_down(x + n - 1, n)
}

#[allow(dead_code)]
pub struct SpaceBitmap<const ALIGN: usize> {
    mem_map: Mmap,
    bitmap_begin: *mut Atomic<usize>,
    bitmap_size: usize,
    heap_begin: usize,
    heap_limit: usize,
    name: &'static str,
}
const BITS_PER_INTPTR: usize = size_of::<usize>() * 8;
impl<const ALIGN: usize> SpaceBitmap<ALIGN> {
    pub fn is_null(&self) -> bool {
        self.bitmap_begin.is_null()
    }
    pub fn clear_all(&self) {
        self.mem_map
            .dontneed_and_zero(self.mem_map.start(), self.mem_map.size());
    }
    pub fn empty() -> Self {
        Self {
            mem_map: Mmap::uninit(),
            bitmap_begin: null_mut(),
            bitmap_size: 0,
            heap_begin: 0,
            heap_limit: 0,
            name: "",
        }
    }
    #[inline]
    pub fn get_name(&self) -> &'static str {
        self.name
    }
    #[inline]
    pub fn set_name(&mut self, name: &'static str) {
        self.name = name;
    }
    #[inline]
    pub fn heap_limit(&self) -> usize {
        self.heap_limit
    }
    #[inline]
    pub fn heap_begin(&self) -> usize {
        self.heap_begin
    }
    #[inline]
    pub fn set_heap_size(&mut self, bytes: usize) {
        self.bitmap_size = Self::offset_to_index(bytes) * size_of::<usize>();
        assert_eq!(self.heap_size(), bytes);
    }
    #[inline]
    pub fn heap_size(&self) -> usize {
        Self::index_to_offset(self.size() as u64 / size_of::<usize>() as u64) as _
    }
    #[inline]
    pub fn has_address(&self, obj: *const u8) -> bool {
        let offset = (obj as usize).wrapping_sub(self.heap_begin);
        let index = Self::offset_to_index(offset);
        index < (self.bitmap_size / size_of::<usize>())
    }
    #[inline]
    pub fn size(&self) -> usize {
        self.bitmap_size
    }
    #[inline]
    pub fn begin(&self) -> *mut Atomic<usize> {
        self.bitmap_begin
    }
    #[inline]
    pub fn index_to_offset(index: u64) -> u64 {
        index * ALIGN as u64 * BITS_PER_INTPTR as u64
    }
    #[inline]
    pub fn offset_to_index(offset: usize) -> usize {
        offset / ALIGN / BITS_PER_INTPTR
    }
    #[inline]
    pub fn offset_bit_index(offset: usize) -> usize {
        (offset / ALIGN) % BITS_PER_INTPTR
    }
    #[inline]
    pub fn offset_to_mask(offset: usize) -> usize {
        1 << Self::offset_bit_index(offset)
    }
    #[inline]
    pub fn atomic_test_and_set(&self, obj: *const u8) -> bool {
        let addr = obj as usize;
        debug_assert!(addr >= self.heap_begin);
        let offset = addr.wrapping_sub(self.heap_begin);
        let index = Self::offset_to_index(offset);
        let mask = Self::offset_to_mask(offset);
        unsafe {
            let atomic_entry = &mut *self.bitmap_begin.add(index);
            debug_assert!(
                index < self.bitmap_size / size_of::<usize>(),
                "bitmap_size: {}",
                self.bitmap_size
            );

            let mut old_word;
            while {
                old_word = atomic_entry.load(Ordering::Relaxed);
                if (old_word & mask) != 0 {
                    return true;
                }
                atomic_entry
                    .compare_exchange_weak(
                        old_word,
                        old_word | mask,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_err()
            } {}

            false
        }
    }
    #[inline]
    pub fn test(&self, obj: *const u8) -> bool {
        let addr = obj as usize;
        debug_assert!(self.has_address(obj), "Invalid object address: {:p}", obj);
        debug_assert!(self.heap_begin <= addr);
        unsafe {
            let offset = addr.wrapping_sub(self.heap_begin);
            let index = Self::offset_to_index(offset);

            ((*self.bitmap_begin.add(index)).load(Ordering::Relaxed) & Self::offset_to_mask(offset))
                != 0
        }
    }

    pub fn find_header(
        &mut self,
        address_maybe_pointing_to_the_middle_of_object: *const u8,
    ) -> *mut HeapObjectHeader {
        let address_maybe_pointing_to_the_middle_of_object =
            address_maybe_pointing_to_the_middle_of_object as usize;
        let object_offset =
            address_maybe_pointing_to_the_middle_of_object.wrapping_sub(self.heap_begin);
        let object_start_number = object_offset / ALIGN;

        let mut cell_index = object_start_number / BITS_PER_INTPTR;
        let bit = object_start_number & (BITS_PER_INTPTR - 1);

        let mut word = unsafe { self.bitmap_begin.add(cell_index).cast::<usize>().read() }
            & ((1 << (bit + 1)) - 1);

        while word == 0 && cell_index != 0 {
            word = unsafe {
                cell_index -= 1;
                self.bitmap_begin.add(cell_index).cast::<usize>().read()
            };
        }
        let leading_zeros = word.leading_zeros() as usize;
        let object_start_number =
            (cell_index * BITS_PER_INTPTR) + (BITS_PER_INTPTR - 1) - leading_zeros;
        let object_offset = object_start_number * MIN_ALLOCATION;

        (object_offset + self.heap_begin) as _
    }

    #[inline(always)]
    pub fn modify<const SET_BIT: bool>(&self, obj: *const u8) -> bool {
        let addr = obj as usize;
        debug_assert!(
            addr >= self.heap_begin,
            "invalid address: {:x} ({:x} > {:x} is false)",
            addr,
            addr,
            self.heap_begin
        );
        //debug_assert!(obj as usize % ALIGN == 0, "Unaligned address {:p}", obj);
        debug_assert!(self.has_address(obj), "Invalid object address: {:p}", obj);
        let offset = addr.wrapping_sub(self.heap_begin);
        let index = Self::offset_to_index(offset);
        let mask = Self::offset_to_mask(offset);
        debug_assert!(
            index < self.bitmap_size / size_of::<usize>(),
            "bitmap size: {}",
            self.bitmap_size
        );
        let atomic_entry = unsafe { &*self.bitmap_begin.add(index) };
        let old_word = atomic_entry.load(Ordering::Relaxed);
        if SET_BIT {
            // Check the bit before setting the word incase we are trying to mark a read only bitmap
            // like an image space bitmap. This bitmap is mapped as read only and will fault if we
            // attempt to change any words. Since all of the objects are marked, this will never
            // occur if we check before setting the bit. This also prevents dirty pages that would
            // occur if the bitmap was read write and we did not check the bit.
            if (old_word & mask) == 0 {
                atomic_entry.store(old_word | mask, Ordering::Relaxed);
            }
        } else {
            atomic_entry.store(old_word & !mask, Ordering::Relaxed);
        }

        debug_assert_eq!(self.test(obj), SET_BIT);
        (old_word & mask) != 0
    }

    #[inline(always)]
    pub fn modify_sync<const SET_BIT: bool>(&self, obj: *const u8) -> bool {
        let addr = obj as usize;
        debug_assert!(
            addr >= self.heap_begin,
            "invalid address: {:x} ({:x} > {:x} is false)",
            addr,
            addr,
            self.heap_begin
        );
        //debug_assert!(obj as usize % ALIGN == 0, "Unaligned address {:p}", obj);
        debug_assert!(self.has_address(obj), "Invalid object address: {:p}", obj);
        let offset = addr.wrapping_sub(self.heap_begin);
        let index = Self::offset_to_index(offset);
        let mask = Self::offset_to_mask(offset);
        debug_assert!(
            index < self.bitmap_size / size_of::<usize>(),
            "bitmap size: {}",
            self.bitmap_size
        );
        let atomic_entry = unsafe { &mut *self.bitmap_begin.add(index).cast::<usize>() };
        let old_word = *atomic_entry;
        if SET_BIT {
            // Check the bit before setting the word incase we are trying to mark a read only bitmap
            // like an image space bitmap. This bitmap is mapped as read only and will fault if we
            // attempt to change any words. Since all of the objects are marked, this will never
            // occur if we check before setting the bit. This also prevents dirty pages that would
            // occur if the bitmap was read write and we did not check the bit.
            if (old_word & mask) == 0 {
                *atomic_entry = old_word | mask;
                //atomic_entry.store(old_word | mask, Ordering::Relaxed);
            }
        } else {
            *atomic_entry = old_word & !mask;
            //atomic_entry.store(old_word & !mask, Ordering::Relaxed);
        }

        debug_assert_eq!(self.test(obj), SET_BIT);
        (old_word & mask) != 0
    }

    #[inline(always)]
    pub fn set(&self, obj: *const u8) -> bool {
        self.modify::<true>(obj)
    }
    #[inline(always)]
    pub fn set_sync(&self, obj: *const u8) -> bool {
        self.modify_sync::<true>(obj)
    }

    #[inline(always)]
    pub fn clear(&self, obj: *const u8) -> bool {
        self.modify::<false>(obj)
    }

    pub fn compute_bitmap_size(capacity: u64) -> usize {
        let bytes_covered_per_word = ALIGN * BITS_PER_INTPTR;
        ((round_up(capacity, bytes_covered_per_word as _) / bytes_covered_per_word as u64)
            * size_of::<usize>() as u64) as _
    }
    pub fn compute_heap_size(bitmap_bytes: u64) -> usize {
        (bitmap_bytes * 8 * ALIGN as u64) as _
    }

    pub fn clear_range(&self, begin: *const u8, end: *const u8) {
        let mut begin_offset = begin as usize - self.heap_begin as usize;
        let mut end_offset = end as usize - self.heap_begin as usize;
        while begin_offset < end_offset && Self::offset_bit_index(begin_offset) != 0 {
            self.clear((self.heap_begin + begin_offset) as _);
            begin_offset += ALIGN;
        }

        while begin_offset < end_offset && Self::offset_bit_index(end_offset) != 0 {
            end_offset -= ALIGN;
            self.clear((self.heap_begin + end_offset) as _);
        }
        // TODO: try to madvise unused pages.
    }

    /// Visit marked bits in bitmap.
    ///
    /// NOTE: You can safely change bits in this bitmap during visiting it since it loads word and then visits marked bits in it.
    pub fn visit_marked_range(
        &self,
        visit_begin: *const u8,
        visit_end: *const u8,
        mut visitor: impl FnMut(*mut HeapObjectHeader),
    ) {
        /*let mut scan = visit_begin;
        let end = visit_end;
        unsafe {
            while scan < end {
                if self.test(scan) {
                    visitor(scan as _);
                }
                scan = scan.add(ALIGN);
            }
        }*/
        let offset_start = visit_begin as usize - self.heap_begin as usize;
        let offset_end = visit_end as usize - self.heap_begin as usize;

        let index_start = Self::offset_to_index(offset_start);
        let index_end = Self::offset_to_index(offset_end);
        let bit_start = (offset_start / ALIGN) % BITS_PER_INTPTR;
        let bit_end = (offset_end / ALIGN) % BITS_PER_INTPTR;
        // Index(begin)  ...    Index(end)
        // [xxxxx???][........][????yyyy]
        //      ^                   ^
        //      |                   #---- Bit of visit_end
        //      #---- Bit of visit_begin
        //

        unsafe {
            let mut left_edge = self.bitmap_begin.cast::<usize>().add(index_start).read();
            left_edge &= !((1 << bit_start) - 1);
            let mut right_edge;
            if index_start < index_end {
                // Left edge != right edge.

                // Traverse left edge.
                if left_edge != 0 {
                    let ptr_base =
                        Self::index_to_offset(index_start as _) as usize + self.heap_begin;
                    while {
                        let shift = left_edge.trailing_zeros();
                        let obj = (ptr_base + shift as usize * ALIGN) as *mut HeapObjectHeader;
                        visitor(obj);
                        left_edge ^= 1 << shift as usize;
                        left_edge != 0
                    } {}
                }
                // Traverse the middle, full part.
                for i in index_start + 1..index_end {
                    let mut w = (*self.bitmap_begin.add(i)).load(Ordering::Relaxed);
                    if w != 0 {
                        let ptr_base = Self::index_to_offset(i as _) as usize + self.heap_begin;
                        while {
                            let shift = w.trailing_zeros();
                            let obj = (ptr_base + shift as usize * ALIGN) as *mut HeapObjectHeader;
                            visitor(obj);
                            w ^= 1 << shift as usize;
                            w != 0
                        } {}
                    }
                }

                // Right edge is unique.
                // But maybe we don't have anything to do: visit_end starts in a new word...
                if bit_end == 0 {
                    right_edge = 0;
                } else {
                    right_edge = self.bitmap_begin.cast::<usize>().add(index_end).read();
                }
            } else {
                right_edge = left_edge;
            }

            // right edge handling

            right_edge &= (1 << bit_end) - 1;
            if right_edge != 0 {
                let ptr_base = Self::index_to_offset(index_end as _) as usize + self.heap_begin;
                while {
                    let shift = right_edge.trailing_zeros();
                    let obj = (ptr_base + shift as usize * ALIGN) as *mut HeapObjectHeader;
                    visitor(obj);
                    right_edge ^= 1 << shift as usize;
                    right_edge != 0
                } {}
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(
        name: &'static str,
        mem_map: Mmap,
        bitmap_begin: *mut usize,
        bitmap_size: usize,
        heap_begin: *mut u8,
        heap_capacity: usize,
    ) -> Self {
        Self {
            name,
            mem_map,
            bitmap_size,
            bitmap_begin: bitmap_begin.cast(),

            heap_begin: heap_begin as _,
            heap_limit: heap_begin as usize + heap_capacity,
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn create_from_memmap(
        name: &'static str,
        mem_map: Mmap,
        heap_begin: *mut u8,
        heap_capacity: usize,
    ) -> Self {
        let bitmap_begin = mem_map.start() as *mut u8;
        let bitmap_size = Self::compute_bitmap_size(heap_capacity as _);
        mem_map.commit(bitmap_begin, mem_map.size());
        unsafe {
            core::ptr::write_bytes(bitmap_begin, 0, bitmap_size);
        }
        Self {
            name,
            mem_map,
            bitmap_begin: bitmap_begin.cast(),
            bitmap_size,
            heap_begin: heap_begin as _,
            heap_limit: heap_begin as usize + heap_capacity,
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn create(name: &'static str, heap_begin: *mut u8, heap_capacity: usize) -> Self {
        let bitmap_size = Self::compute_bitmap_size(heap_capacity as _);

        let mem_map = Mmap::new(bitmap_size, 0);

        Self::create_from_memmap(name, mem_map, heap_begin, heap_capacity)
    }

    #[cfg(target_arch = "wasm32")]
    pub fn create(name: &'static str, heap_begin: *mut u8, heap_capacity: usize) -> Self {
        let bitmap_size = Self::compute_bitmap_size(heap_capacity as _);
        let memory = unsafe { libc::malloc(bitmap_size).cast::<u8>() };
        Self::create_from_raw(name, memory, heap_begin, heap_capacity)
    }
    #[cfg(target_arch = "wasm32")]
    pub fn create_from_raw(
        name: &'static str,
        mem: *mut u8,
        heap_begin: *mut u8,
        heap_capacity: usize,
    ) -> Self {
        let bitmap_begin = mem as *mut u8;
        let bitmap_size = Self::compute_bitmap_size(heap_capacity as _);
        Self {
            name,
            mem,
            bitmap_begin: bitmap_begin.cast(),
            bitmap_size,
            heap_begin: heap_begin as _,
            heap_limit: heap_begin as usize + heap_capacity,
        }
    }

    pub fn sweep_walk_color(
        live_bitmap: &SpaceBitmap<{ ALIGN }>,
        sweep_begin: usize,
        sweep_end: usize,

        mut callback: impl FnMut(usize, *mut *mut HeapObjectHeader),
        buffer_size: Option<usize>,
        from_color: u8,
        to_color: u8,
    ) {
        if sweep_end <= sweep_begin {
            return;
        }

        let buffer_size = buffer_size.unwrap_or_else(|| size_of::<usize>() * BITS_PER_INTPTR);
        unsafe {
            let start = Self::offset_to_index(sweep_begin - live_bitmap.heap_begin as usize);
            let end = Self::offset_to_index(sweep_end - live_bitmap.heap_begin as usize - 1);

            let mut pointer_buf = vec![null_mut::<HeapObjectHeader>(); buffer_size];
            let mut cur_pointer = &mut pointer_buf[0] as *mut *mut HeapObjectHeader;
            let pointer_end = cur_pointer.add(buffer_size - BITS_PER_INTPTR);
            let live = live_bitmap.bitmap_begin;
            for i in start..=end {
                let mut garbage = (*live.add(i)).load(Ordering::Relaxed);
                if garbage != 0 {
                    // there is potential garbage in this bitmap word
                    let ptr_base =
                        Self::index_to_offset(i as _) as usize + live_bitmap.heap_begin as usize;
                    while {
                        let shift = garbage.trailing_zeros() as usize;
                        garbage ^= 1 << shift;
                        let object = (ptr_base + shift * ALIGN) as *mut HeapObjectHeader;

                        if (*object).set_color(from_color, to_color) {
                            // set_color returns `true` if changing color failed. If it fails there then object color is white and we can put it to
                            // buffer for freeing objects.
                            cur_pointer.write((ptr_base + shift * ALIGN) as _);
                            cur_pointer = cur_pointer.add(1);
                        }

                        garbage != 0
                    } {}
                    if cur_pointer >= pointer_end {
                        callback(
                            cur_pointer.offset_from(&pointer_buf[0]) as _,
                            &mut pointer_buf[0],
                        );
                        cur_pointer = &mut pointer_buf[0];
                    }
                }
            }

            if cur_pointer > &mut pointer_buf[0] as *mut *mut HeapObjectHeader {
                callback(
                    cur_pointer.offset_from(&pointer_buf[0]) as _,
                    &mut pointer_buf[0],
                );
            }
        }
    }

    pub fn sweep_walk(
        live_bitmap: &SpaceBitmap<{ ALIGN }>,
        mark_bitmap: &SpaceBitmap<{ ALIGN }>,
        sweep_begin: usize,
        sweep_end: usize,
        mut callback: impl FnMut(usize, *mut *mut HeapObjectHeader),
    ) {
        if sweep_end <= sweep_begin {
            return;
        }

        let buffer_size = size_of::<usize>() * BITS_PER_INTPTR;

        let live = live_bitmap.bitmap_begin;
        let mark = mark_bitmap.bitmap_begin;
        unsafe {
            let start = Self::offset_to_index(sweep_begin - live_bitmap.heap_begin as usize);
            let end = Self::offset_to_index(sweep_end - live_bitmap.heap_begin as usize - 1);

            let mut pointer_buf = vec![null_mut::<HeapObjectHeader>(); buffer_size];
            let mut cur_pointer = &mut pointer_buf[0] as *mut *mut HeapObjectHeader;
            let pointer_end = cur_pointer.add(buffer_size - BITS_PER_INTPTR);
            for i in start..=end {
                let mut garbage = (*live.add(i)).load(Ordering::Relaxed)
                    & !(*mark.add(i)).load(Ordering::Relaxed);
                if garbage != 0 {
                    let ptr_base =
                        Self::index_to_offset(i as _) as usize + live_bitmap.heap_begin as usize;
                    while {
                        let shift = garbage.trailing_zeros() as usize;
                        garbage ^= 1 << shift;
                        cur_pointer.write((ptr_base + shift * ALIGN) as _);
                        cur_pointer = cur_pointer.add(1);
                        garbage != 0
                    } {}

                    if cur_pointer >= pointer_end {
                        callback(
                            cur_pointer.offset_from(&pointer_buf[0]) as _,
                            &mut pointer_buf[0],
                        );
                        cur_pointer = &mut pointer_buf[0];
                    }
                }
            }

            if cur_pointer > &mut pointer_buf[0] as *mut *mut HeapObjectHeader {
                callback(
                    cur_pointer.offset_from(&pointer_buf[0]) as _,
                    &mut pointer_buf[0],
                );
            }
        }
    }

    pub fn copy_view(&mut self, other: &Self) {
        self.bitmap_begin = other.bitmap_begin;
        self.bitmap_size = other.bitmap_size;
        self.heap_begin = other.heap_begin;
        self.heap_limit = other.heap_limit;
    }
}

impl<const ALIGN: usize> fmt::Debug for SpaceBitmap<ALIGN> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[begin={:p},end={:p}]",
            self.heap_begin as *const (), self.heap_limit as *const ()
        )
    }
}

pub struct HeapBitmap {
    continuous_space_bitmaps: Vec<*const SpaceBitmap<{ MIN_ALLOCATION }>>,
}

// TODO: PreciseAllocation
impl HeapBitmap {
    pub fn new() -> Self {
        Self {
            continuous_space_bitmaps: vec![],
        }
    }
    pub fn get_continuous_space_bitmap(
        &self,
        obj: *const HeapObjectHeader,
    ) -> Option<&SpaceBitmap<{ MIN_ALLOCATION }>> {
        for bitmap in self.continuous_space_bitmaps.iter() {
            unsafe {
                if (**bitmap).has_address(obj.cast()) {
                    return Some(&**bitmap);
                }
            }
        }
        None
    }

    pub fn test(&self, obj: *const HeapObjectHeader) -> bool {
        let bitmap = self.get_continuous_space_bitmap(obj);
        if let Some(bitmap) = bitmap {
            return bitmap.test(obj.cast());
        }
        unsafe {
            debug_assert!((*obj).is_precise());
        }
        false
    }

    pub fn set(&self, obj: *const HeapObjectHeader) -> bool {
        let bitmap = self.get_continuous_space_bitmap(obj);
        if let Some(bitmap) = bitmap {
            return bitmap.set(obj.cast());
        }
        unsafe {
            debug_assert!((*obj).is_precise());
        }
        false
    }

    pub fn atomic_test_and_set(&self, obj: *const HeapObjectHeader) -> bool {
        let bitmap = self.get_continuous_space_bitmap(obj);
        if let Some(bitmap) = bitmap {
            return bitmap.atomic_test_and_set(obj.cast());
        }

        unsafe {
            debug_assert!((*obj).is_precise());
        }
        false
    }

    pub fn clear(&self, obj: *const HeapObjectHeader) -> bool {
        let bitmap = self.get_continuous_space_bitmap(obj);
        if let Some(bitmap) = bitmap {
            return bitmap.clear(obj.cast());
        }

        unsafe {
            debug_assert!((*obj).is_precise());
        }
        false
    }

    pub fn add_continuous_space(&mut self, space: *const SpaceBitmap<{ MIN_ALLOCATION }>) {
        self.continuous_space_bitmaps.push(space);
    }
}

pub struct ObjectStartBitmap {
    #[allow(dead_code)]
    mmap: Mmap,
    bitmap: *mut u8,
    offset: usize,
}

impl ObjectStartBitmap {
    pub fn empty() -> Self {
        Self {
            mmap: Mmap::uninit(),
            bitmap: null_mut(),
            offset: 0,
        }
    }
    pub const BITS_PER_CELL: usize = 8;
    pub const CELL_MASK: usize = Self::BITS_PER_CELL - 1;
    pub fn new(heap_begin: *const u8, size: usize) -> Self {
        let mmap = Mmap::new(Self::allocation_size(size), 0);
        unsafe {
            core::ptr::write_bytes(mmap.start(), 0, mmap.size());
        }
        Self {
            bitmap: mmap.start(),
            mmap,
            offset: heap_begin as _,
        }
    }
    pub fn allocation_size(heap_size: usize) -> usize {
        ((heap_size + 4096) + ((Self::BITS_PER_CELL * MIN_ALLOCATION) - 1))
            / (Self::BITS_PER_CELL * MIN_ALLOCATION)
    }
    #[inline(always)]
    pub fn load(&self, index: usize) -> u8 {
        unsafe { self.bitmap.add(index).read() }
    }
    #[inline(always)]
    pub fn store(&self, index: usize, bit: u8) {
        unsafe {
            self.bitmap.add(index).write(bit);
        }
    }
    #[inline(always)]
    fn object_start_index_and_bit(&self, addr: usize, cell_index: &mut usize, bit: &mut usize) {
        let object_offset = addr - self.offset;
        let object_start_number = object_offset / MIN_ALLOCATION;
        *cell_index = object_start_number / Self::BITS_PER_CELL;
        *bit = object_start_number & Self::CELL_MASK;
    }

    #[inline(always)]
    pub fn set_bit(&self, addr: *const u8) {
        let mut cell_index = 0;
        let mut object_bit = 0;
        self.object_start_index_and_bit(addr as _, &mut cell_index, &mut object_bit);
        self.store(cell_index, self.load(cell_index) | (1 << object_bit));
    }
    #[inline(always)]
    pub fn clear_bit(&self, addr: *const u8) {
        let mut cell_index = 0;
        let mut object_bit = 0;
        self.object_start_index_and_bit(addr as _, &mut cell_index, &mut object_bit);
        self.store(cell_index, self.load(cell_index) & !(1 << object_bit));
    }
    pub fn check_bit(&self, addr: *const u8) -> bool {
        let mut cell_index = 0;
        let mut object_bit = 0;
        self.object_start_index_and_bit(addr as _, &mut cell_index, &mut object_bit);
        (self.load(cell_index) & (1 << object_bit)) != 0
    }
    pub fn find_header(&self, addr_in_middle: *const u8) -> *mut HeapObjectHeader {
        let mut object_offset = addr_in_middle as usize - self.offset;
        let mut object_start_number = object_offset / MIN_ALLOCATION;
        let mut cell_index = object_start_number / Self::BITS_PER_CELL;
        let bit = object_start_number & Self::CELL_MASK;
        let mut byte = self.load(cell_index) & ((1 << (bit + 1)) - 1);
        while byte == 0 && cell_index != 0 {
            cell_index -= 1;
            byte = self.load(cell_index);
        }
        let leading_zeros = byte.leading_zeros();
        object_start_number =
            (cell_index * Self::BITS_PER_CELL) + (Self::BITS_PER_CELL - 1) - leading_zeros as usize;
        object_offset = object_start_number * MIN_ALLOCATION;
        (object_offset + self.offset) as _
    }

    pub fn clear(&self) {
        unsafe {
            memx::memset(
                std::slice::from_raw_parts_mut(self.bitmap, self.mmap.size()),
                0,
            );
        }
    }
}

macro_rules! gen_const_bitmap {
    ($name: ident, $align: expr, $capacity: expr) => {
        #[allow(dead_code)]
        pub struct $name {
            storage: [u8; Self::BITMAP_SIZE],
            bitmap_begin: *mut Atomic<usize>,
            bitmap_size: usize,
            heap_begin: usize,
            heap_limit: usize,
            name: &'static str,
        }
        impl $name {
            pub const BITMAP_SIZE: usize = {
                let bytes_covered_per_word = $align * (core::mem::size_of::<usize>() * 8);
                (round_up($capacity as u64, bytes_covered_per_word as _)
                    / bytes_covered_per_word as u64) as usize
                    * core::mem::size_of::<isize>()
            };
            pub const ALIGN: usize = $align;
            pub const CAPACITY: usize = $capacity;
            pub fn is_null(&self) -> bool {
                self.bitmap_begin.is_null()
            }
            pub fn clear_all(&mut self) {
                unsafe {
                    std::ptr::write_bytes(self.bitmap_begin.cast::<u8>(), 0, self.bitmap_size);
                }
            }
            pub fn empty() -> Self {
                Self {
                    storage: [0; {
                        let bytes_covered_per_word = $name::ALIGN * BITS_PER_INTPTR;
                        ((round_up($name::CAPACITY as _, bytes_covered_per_word as _)
                            / bytes_covered_per_word as u64)
                            * size_of::<usize>() as u64) as usize
                    }],
                    bitmap_begin: null_mut(),
                    bitmap_size: 0,
                    heap_begin: 0,
                    heap_limit: 0,
                    name: "",
                }
            }
            #[inline]
            pub fn get_name(&self) -> &'static str {
                self.name
            }
            #[inline]
            pub fn set_name(&mut self, name: &'static str) {
                self.name = name;
            }
            #[inline]
            pub fn heap_limit(&self) -> usize {
                self.heap_limit
            }
            #[inline]
            pub fn heap_begin(&self) -> usize {
                self.heap_begin
            }
            #[inline]
            pub fn set_heap_size(&mut self, bytes: usize) {
                self.bitmap_size = Self::offset_to_index(bytes) * size_of::<usize>();
                assert_eq!(self.heap_size(), bytes);
            }
            #[inline]
            pub fn heap_size(&self) -> usize {
                Self::index_to_offset(self.size() as u64 / size_of::<usize>() as u64) as _
            }
            #[inline]
            pub fn has_address(&self, obj: *const u8) -> bool {
                let offset = (obj as usize).wrapping_sub(self.heap_begin);
                let index = Self::offset_to_index(offset);
                index < (self.bitmap_size / size_of::<usize>())
            }
            #[inline]
            pub fn size(&self) -> usize {
                self.bitmap_size
            }
            #[inline]
            pub fn begin(&self) -> *mut Atomic<usize> {
                self.bitmap_begin
            }
            #[inline]
            pub fn index_to_offset(index: u64) -> u64 {
                index * Self::ALIGN as u64 * BITS_PER_INTPTR as u64
            }
            #[inline]
            pub fn offset_to_index(offset: usize) -> usize {
                offset / Self::ALIGN / BITS_PER_INTPTR
            }
            #[inline]
            pub fn offset_bit_index(offset: usize) -> usize {
                (offset / Self::ALIGN) % BITS_PER_INTPTR
            }
            #[inline]
            pub fn offset_to_mask(offset: usize) -> usize {
                1 << Self::offset_bit_index(offset)
            }
            #[inline]
            pub fn atomic_test_and_set(&self, obj: *const u8) -> bool {
                let addr = obj as usize;
                debug_assert!(addr >= self.heap_begin);
                let offset = addr.wrapping_sub(self.heap_begin);
                let index = Self::offset_to_index(offset);
                let mask = Self::offset_to_mask(offset);
                unsafe {
                    let atomic_entry = &mut *self.bitmap_begin.add(index);
                    debug_assert!(
                        index < self.bitmap_size / size_of::<usize>(),
                        "bitmap_size: {}",
                        self.bitmap_size
                    );

                    let mut old_word;
                    while {
                        old_word = atomic_entry.load(Ordering::Relaxed);
                        if (old_word & mask) != 0 {
                            return true;
                        }
                        atomic_entry
                            .compare_exchange_weak(
                                old_word,
                                old_word | mask,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_err()
                    } {}

                    false
                }
            }
            #[inline]
            pub fn test(&self, obj: *const u8) -> bool {
                let addr = obj as usize;
                debug_assert!(self.has_address(obj), "Invalid object address: {:p}", obj);
                debug_assert!(self.heap_begin <= addr);
                unsafe {
                    let offset = addr.wrapping_sub(self.heap_begin);
                    let index = Self::offset_to_index(offset);

                    ((*self.bitmap_begin.add(index)).load(Ordering::Relaxed)
                        & Self::offset_to_mask(offset))
                        != 0
                }
            }

            pub fn find_header(
                &mut self,
                address_maybe_pointing_to_the_middle_of_object: *const u8,
            ) -> *mut HeapObjectHeader {
                let address_maybe_pointing_to_the_middle_of_object =
                    address_maybe_pointing_to_the_middle_of_object as usize;
                let object_offset =
                    address_maybe_pointing_to_the_middle_of_object.wrapping_sub(self.heap_begin);
                let object_start_number = object_offset / Self::ALIGN;

                let mut cell_index = object_start_number / BITS_PER_INTPTR;
                let bit = object_start_number & (BITS_PER_INTPTR - 1);

                let mut word = unsafe { self.bitmap_begin.add(cell_index).cast::<usize>().read() }
                    & ((1 << (bit + 1)) - 1);

                while word == 0 && cell_index != 0 {
                    word = unsafe {
                        cell_index -= 1;
                        self.bitmap_begin.add(cell_index).cast::<usize>().read()
                    };
                }
                let leading_zeros = word.leading_zeros() as usize;
                let object_start_number =
                    (cell_index * BITS_PER_INTPTR) + (BITS_PER_INTPTR - 1) - leading_zeros;
                let object_offset = object_start_number * MIN_ALLOCATION;

                (object_offset + self.heap_begin) as _
            }

            #[inline(always)]
            pub fn modify<const SET_BIT: bool>(&self, obj: *const u8) -> bool {
                let addr = obj as usize;
                debug_assert!(
                    addr >= self.heap_begin,
                    "invalid address: {:x} ({:x} > {:x} is false)",
                    addr,
                    addr,
                    self.heap_begin
                );
                //debug_assert!(obj as usize % ALIGN == 0, "Unaligned address {:p}", obj);
                debug_assert!(self.has_address(obj), "Invalid object address: {:p}", obj);
                let offset = addr.wrapping_sub(self.heap_begin);
                let index = Self::offset_to_index(offset);
                let mask = Self::offset_to_mask(offset);
                debug_assert!(
                    index < self.bitmap_size / size_of::<usize>(),
                    "bitmap size: {}",
                    self.bitmap_size
                );
                let atomic_entry = unsafe { &*self.bitmap_begin.add(index) };
                let old_word = atomic_entry.load(Ordering::Relaxed);
                if SET_BIT {
                    // Check the bit before setting the word incase we are trying to mark a read only bitmap
                    // like an image space bitmap. This bitmap is mapped as read only and will fault if we
                    // attempt to change any words. Since all of the objects are marked, this will never
                    // occur if we check before setting the bit. This also prevents dirty pages that would
                    // occur if the bitmap was read write and we did not check the bit.
                    if (old_word & mask) == 0 {
                        atomic_entry.store(old_word | mask, Ordering::Relaxed);
                    }
                } else {
                    atomic_entry.store(old_word & !mask, Ordering::Relaxed);
                }

                debug_assert_eq!(self.test(obj), SET_BIT);
                (old_word & mask) != 0
            }

            #[inline(always)]
            pub fn modify_sync<const SET_BIT: bool>(&self, obj: *const u8) -> bool {
                let addr = obj as usize;
                debug_assert!(
                    addr >= self.heap_begin,
                    "invalid address: {:x} ({:x} > {:x} is false)",
                    addr,
                    addr,
                    self.heap_begin
                );
                //debug_assert!(obj as usize % ALIGN == 0, "Unaligned address {:p}", obj);
                debug_assert!(self.has_address(obj), "Invalid object address: {:p}", obj);
                let offset = addr.wrapping_sub(self.heap_begin);
                let index = Self::offset_to_index(offset);
                let mask = Self::offset_to_mask(offset);
                debug_assert!(
                    index < self.bitmap_size / size_of::<usize>(),
                    "bitmap size: {}",
                    self.bitmap_size
                );
                let atomic_entry = unsafe { &mut *self.bitmap_begin.add(index).cast::<usize>() };
                let old_word = *atomic_entry;
                if SET_BIT {
                    // Check the bit before setting the word incase we are trying to mark a read only bitmap
                    // like an image space bitmap. This bitmap is mapped as read only and will fault if we
                    // attempt to change any words. Since all of the objects are marked, this will never
                    // occur if we check before setting the bit. This also prevents dirty pages that would
                    // occur if the bitmap was read write and we did not check the bit.
                    if (old_word & mask) == 0 {
                        *atomic_entry = old_word | mask;
                        //atomic_entry.store(old_word | mask, Ordering::Relaxed);
                    }
                } else {
                    *atomic_entry = old_word & !mask;
                    //atomic_entry.store(old_word & !mask, Ordering::Relaxed);
                }

                debug_assert_eq!(self.test(obj), SET_BIT);
                (old_word & mask) != 0
            }

            #[inline(always)]
            pub fn set(&self, obj: *const u8) -> bool {
                self.modify::<true>(obj)
            }
            #[inline(always)]
            pub fn set_sync(&self, obj: *const u8) -> bool {
                self.modify_sync::<true>(obj)
            }

            #[inline(always)]
            pub fn clear(&self, obj: *const u8) -> bool {
                self.modify::<false>(obj)
            }

            pub fn compute_bitmap_size(capacity: u64) -> usize {
                let bytes_covered_per_word = Self::ALIGN * BITS_PER_INTPTR;
                ((round_up(capacity, bytes_covered_per_word as _) / bytes_covered_per_word as u64)
                    * size_of::<usize>() as u64) as _
            }
            pub fn compute_heap_size(bitmap_bytes: u64) -> usize {
                (bitmap_bytes * 8 * Self::ALIGN as u64) as _
            }

            pub fn clear_range(&self, begin: *const u8, end: *const u8) {
                let mut begin_offset = begin as usize - self.heap_begin as usize;
                let mut end_offset = end as usize - self.heap_begin as usize;
                while begin_offset < end_offset && Self::offset_bit_index(begin_offset) != 0 {
                    self.clear((self.heap_begin + begin_offset) as _);
                    begin_offset += Self::ALIGN;
                }

                while begin_offset < end_offset && Self::offset_bit_index(end_offset) != 0 {
                    end_offset -= Self::ALIGN;
                    self.clear((self.heap_begin + end_offset) as _);
                }
                // TODO: try to madvise unused pages.
            }

            /// Visit marked bits in bitmap.
            ///
            /// NOTE: You can safely change bits in this bitmap during visiting it since it loads word and then visits marked bits in it.
            pub fn visit_marked_range(
                &self,
                visit_begin: *const u8,
                visit_end: *const u8,
                mut visitor: impl FnMut(*mut HeapObjectHeader),
            ) {
                /*let mut scan = visit_begin;
                let end = visit_end;
                unsafe {
                    while scan < end {
                        if self.test(scan) {
                            visitor(scan as _);
                        }
                        scan = scan.add(ALIGN);
                    }
                }*/
                let offset_start = visit_begin as usize - self.heap_begin as usize;
                let offset_end = visit_end as usize - self.heap_begin as usize;

                let index_start = Self::offset_to_index(offset_start);
                let index_end = Self::offset_to_index(offset_end);
                let bit_start = (offset_start / Self::ALIGN) % BITS_PER_INTPTR;
                let bit_end = (offset_end / Self::ALIGN) % BITS_PER_INTPTR;
                // Index(begin)  ...    Index(end)
                // [xxxxx???][........][????yyyy]
                //      ^                   ^
                //      |                   #---- Bit of visit_end
                //      #---- Bit of visit_begin
                //

                unsafe {
                    let mut left_edge = self.bitmap_begin.cast::<usize>().add(index_start).read();
                    left_edge &= !((1 << bit_start) - 1);
                    let mut right_edge;
                    if index_start < index_end {
                        // Left edge != right edge.

                        // Traverse left edge.
                        if left_edge != 0 {
                            let ptr_base =
                                Self::index_to_offset(index_start as _) as usize + self.heap_begin;
                            while {
                                let shift = left_edge.trailing_zeros();
                                let obj = (ptr_base + shift as usize * Self::ALIGN)
                                    as *mut HeapObjectHeader;
                                visitor(obj);
                                left_edge ^= 1 << shift as usize;
                                left_edge != 0
                            } {}
                        }
                        // Traverse the middle, full part.
                        for i in index_start + 1..index_end {
                            let mut w = (*self.bitmap_begin.add(i)).load(Ordering::Relaxed);
                            if w != 0 {
                                let ptr_base =
                                    Self::index_to_offset(i as _) as usize + self.heap_begin;
                                while {
                                    let shift = w.trailing_zeros();
                                    let obj = (ptr_base + shift as usize * Self::ALIGN)
                                        as *mut HeapObjectHeader;
                                    visitor(obj);
                                    w ^= 1 << shift as usize;
                                    w != 0
                                } {}
                            }
                        }

                        // Right edge is unique.
                        // But maybe we don't have anything to do: visit_end starts in a new word...
                        if bit_end == 0 {
                            right_edge = 0;
                        } else {
                            right_edge = self.bitmap_begin.cast::<usize>().add(index_end).read();
                        }
                    } else {
                        right_edge = left_edge;
                    }

                    // right edge handling

                    right_edge &= (1 << bit_end) - 1;
                    if right_edge != 0 {
                        let ptr_base =
                            Self::index_to_offset(index_end as _) as usize + self.heap_begin;
                        while {
                            let shift = right_edge.trailing_zeros();
                            let obj =
                                (ptr_base + shift as usize * Self::ALIGN) as *mut HeapObjectHeader;
                            visitor(obj);
                            right_edge ^= 1 << shift as usize;
                            right_edge != 0
                        } {}
                    }
                }
            }
            pub unsafe fn init_bitmap(&mut self) {
                self.bitmap_begin = (&mut self.storage[0] as *mut u8) as *mut Atomic<usize>;
            }
            pub fn create(name: &'static str, heap_begin: *mut u8, heap_capacity: usize) -> Self {
                let mut this = Self::empty();
                this.name = name;
                this.heap_begin = heap_begin as _;
                this.heap_limit = heap_begin as usize + heap_capacity;
                this.bitmap_size = Self::BITMAP_SIZE;
                this
            }

            pub fn sweep_walk_color(
                live_bitmap: &Self,
                sweep_begin: usize,
                sweep_end: usize,

                mut callback: impl FnMut(usize, *mut *mut HeapObjectHeader),
                buffer_size: Option<usize>,
                from_color: u8,
                to_color: u8,
            ) {
                if sweep_end <= sweep_begin {
                    return;
                }

                let buffer_size =
                    buffer_size.unwrap_or_else(|| size_of::<usize>() * BITS_PER_INTPTR);
                unsafe {
                    let start =
                        Self::offset_to_index(sweep_begin - live_bitmap.heap_begin as usize);
                    let end =
                        Self::offset_to_index(sweep_end - live_bitmap.heap_begin as usize - 1);

                    let mut pointer_buf = vec![null_mut::<HeapObjectHeader>(); buffer_size];
                    let mut cur_pointer = &mut pointer_buf[0] as *mut *mut HeapObjectHeader;
                    let pointer_end = cur_pointer.add(buffer_size - BITS_PER_INTPTR);
                    let live = live_bitmap.bitmap_begin;
                    for i in start..=end {
                        let mut garbage = (*live.add(i)).load(Ordering::Relaxed);
                        if garbage != 0 {
                            // there is potential garbage in this bitmap word
                            let ptr_base = Self::index_to_offset(i as _) as usize
                                + live_bitmap.heap_begin as usize;
                            while {
                                let shift = garbage.trailing_zeros() as usize;
                                garbage ^= 1 << shift;
                                let object =
                                    (ptr_base + shift * Self::ALIGN) as *mut HeapObjectHeader;

                                if (*object).set_color(from_color, to_color) {
                                    // set_color returns `true` if changing color failed. If it fails there then object color is white and we can put it to
                                    // buffer for freeing objects.
                                    cur_pointer.write((ptr_base + shift * Self::ALIGN) as _);
                                    cur_pointer = cur_pointer.add(1);
                                }

                                garbage != 0
                            } {}
                            if cur_pointer >= pointer_end {
                                callback(
                                    cur_pointer.offset_from(&pointer_buf[0]) as _,
                                    &mut pointer_buf[0],
                                );
                                cur_pointer = &mut pointer_buf[0];
                            }
                        }
                    }

                    if cur_pointer > &mut pointer_buf[0] as *mut *mut HeapObjectHeader {
                        callback(
                            cur_pointer.offset_from(&pointer_buf[0]) as _,
                            &mut pointer_buf[0],
                        );
                    }
                }
            }

            pub fn sweep_walk(
                live_bitmap: &Self,
                mark_bitmap: &Self,
                sweep_begin: usize,
                sweep_end: usize,
                mut callback: impl FnMut(usize, *mut *mut HeapObjectHeader),
            ) {
                if sweep_end <= sweep_begin {
                    return;
                }

                let buffer_size = size_of::<usize>() * BITS_PER_INTPTR;

                let live = live_bitmap.bitmap_begin;
                let mark = mark_bitmap.bitmap_begin;
                unsafe {
                    let start =
                        Self::offset_to_index(sweep_begin - live_bitmap.heap_begin as usize);
                    let end =
                        Self::offset_to_index(sweep_end - live_bitmap.heap_begin as usize - 1);

                    let mut pointer_buf = vec![null_mut::<HeapObjectHeader>(); buffer_size];
                    let mut cur_pointer = &mut pointer_buf[0] as *mut *mut HeapObjectHeader;
                    let pointer_end = cur_pointer.add(buffer_size - BITS_PER_INTPTR);
                    for i in start..=end {
                        let mut garbage = (*live.add(i)).load(Ordering::Relaxed)
                            & !(*mark.add(i)).load(Ordering::Relaxed);
                        if garbage != 0 {
                            let ptr_base = Self::index_to_offset(i as _) as usize
                                + live_bitmap.heap_begin as usize;
                            while {
                                let shift = garbage.trailing_zeros() as usize;
                                garbage ^= 1 << shift;
                                cur_pointer.write((ptr_base + shift * Self::ALIGN) as _);
                                cur_pointer = cur_pointer.add(1);
                                garbage != 0
                            } {}

                            if cur_pointer >= pointer_end {
                                callback(
                                    cur_pointer.offset_from(&pointer_buf[0]) as _,
                                    &mut pointer_buf[0],
                                );
                                cur_pointer = &mut pointer_buf[0];
                            }
                        }
                    }

                    if cur_pointer > &mut pointer_buf[0] as *mut *mut HeapObjectHeader {
                        callback(
                            cur_pointer.offset_from(&pointer_buf[0]) as _,
                            &mut pointer_buf[0],
                        );
                    }
                }
            }

            pub fn copy_view(&mut self, other: &Self) {
                self.bitmap_begin = other.bitmap_begin;
                self.bitmap_size = other.bitmap_size;
                self.heap_begin = other.heap_begin;
                self.heap_limit = other.heap_limit;
            }
        }
    };
}

gen_const_bitmap!(LineMarkTable, IMMIX_LINE_SIZE, CHUNK_SIZE);

pub type ChunkMap = SpaceBitmap<{ CHUNK_SIZE }>;
