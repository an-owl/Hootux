//! This module contains public interfaces for memory allocation

use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::NonNull,
};

use crate::{allocator::HeapAlloc, mem};
use x86_64::{
    structures::paging::{page::PageRangeInclusive, Mapper, Page, PageTableFlags, Size4KiB},
    PhysAddr, VirtAddr,
};

/// Used to Allocate Physical memory regions. All allocations via this type are guaranteed to be the
/// size of the allocation aligned up to [mem::PAGE_SIZE]
///
/// # Safety
///
/// Using this type is unsafe because it allows access to arbitrary regions of physical memory,
/// which allows aliasing and mutation of read only memory.
///
/// MmioAlloc should **only** be used to access physical memory doing otherwise may lead to UB
///
/// The constructors for this struct are unsafe because [core::alloc::Allocator:: cannot make
#[derive(Copy, Clone)]
pub struct MmioAlloc {
    addr: usize,
}

impl MmioAlloc {
    pub unsafe fn new(phys_addr: usize) -> Self {
        Self { addr: phys_addr }
    }

    pub unsafe fn new_from_phys_addr(phys_addr: PhysAddr) -> Self {
        Self::new(phys_addr.as_u64() as usize)
    }

    pub fn as_phys_addr(&self) -> PhysAddr {
        PhysAddr::new(self.addr as u64)
    }

    /// Returns a pointer to the physical address requested within the given page
    fn offset_addr(&self, ptr: NonNull<[u8]>, len: usize) -> NonNull<[u8]> {
        let offset = self.addr & (mem::PAGE_SIZE - 1); //  returns bytes 11-0

        let req_addr = ptr.cast::<u8>().as_ptr() as usize + offset;

        let new_ptr = unsafe { core::slice::from_raw_parts_mut(req_addr as *mut u8, len) };
        NonNull::new(new_ptr).unwrap() // shouldn't panic
    }

    fn get_page_range<T: ?Sized>(&self, layout: &Layout, ptr: &NonNull<T>) -> PageRangeInclusive {
        let addr = VirtAddr::new(ptr.cast::<u8>().as_ptr() as usize as u64);
        let pages = PageRangeInclusive::<Size4KiB> {
            start: Page::containing_address(addr),
            end: Page::containing_address(addr + layout.size() - 1u64),
        };
        pages
    }

    /// Consumes self and returns a Box containing a `T`
    pub unsafe fn boxed_alloc<T>(self) -> Result<alloc::boxed::Box<T, Self>, AllocError> {
        let ptr = self.allocate(Layout::new::<T>())?.cast::<T>();
        let b = alloc::boxed::Box::from_raw_in(ptr.as_ptr(), self);
        Ok(b)
    }
}

unsafe impl Allocator for MmioAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE; // huge page

        let ptr = super::COMBINED_ALLOCATOR.lock().virt_allocate(layout)?;

        let pages = self.get_page_range(&layout, &ptr);
        let mut phys_frame = x86_64::structures::paging::PhysFrame::<Size4KiB>::containing_address(
            PhysAddr::new(self.addr as u64),
        );

        for page in pages {
            unsafe {
                mem::SYS_MAPPER
                    .get()
                    .map_to(page, phys_frame, flags, &mut mem::DummyFrameAlloc)
                    .unwrap() // idk debug
                    .ignore(); // not mapped so not cached
            }
            phys_frame = phys_frame + mem::PAGE_SIZE as u64;
        }

        Ok(self.offset_addr(ptr, layout.size()))
    }

    /// See [Self::grow_zeroed]
    fn allocate_zeroed(&self, _layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        panic!(
            "Called allocate_zeroed() on {}: Not allowed",
            core::any::type_name::<Self>()
        )
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let pages = self.get_page_range(&layout, &ptr);

        // align down to page boundary
        let mut addr = ptr.as_ptr() as *mut u8 as usize;
        addr &= !(mem::PAGE_SIZE - 1);

        let ptr = NonNull::new(addr as *mut u8).expect("Tried to deallocate illegal address");

        for page in pages {
            mem::SYS_MAPPER
                .get()
                .unmap(page)
                .expect("Tried to deallocate unhandled memory")
                .1
                .flush();
        }

        crate::allocator::COMBINED_ALLOCATOR
            .lock()
            .virt_deallocate(ptr, layout);
    }

    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // This fn does not require copying memory
        self.deallocate(ptr, old_layout);
        self.allocate(new_layout)
    }

    /// This fn will panic because the behaviour expected of this fn is likely to overwrite used memory
    ///
    /// use [Self::grow] instead and manually write zeros
    unsafe fn grow_zeroed(
        &self,
        _ptr: NonNull<u8>,
        _old_layout: Layout,
        _new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        panic!(
            "Called grow_zeroed() on {}: Not allowed",
            core::any::type_name::<Self>()
        )
    }
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // This fn does not require copying memory
        self.deallocate(ptr, old_layout);
        self.allocate(new_layout)
    }
}
