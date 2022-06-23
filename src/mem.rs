use bootloader::boot_info::{MemoryRegion, MemoryRegionKind};
use x86_64::structures::paging::frame::PhysFrameRangeInclusive;
use x86_64::structures::paging::page::PageRangeInclusive;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{structures::paging::PageTable, PhysAddr, VirtAddr};

pub mod page_table_tree;
pub(self) mod offset_page_table;

const PAGE_SIZE: usize = 4096;

/// A FrameAllocator that returns usable frames from the bootloader's memory map.
pub struct BootInfoFrameAllocator {
    memory_map: &'static [MemoryRegion],
    next: usize,
}

impl BootInfoFrameAllocator {
    /// Create a FrameAllocator from the passed memory map.
    ///
    /// This function is unsafe because the caller must guarantee that the passed
    /// memory map is valid. The main requirement is that all frames that are marked
    /// as `USABLE` in it are really unused.
    pub unsafe fn init(memory_map: &'static [MemoryRegion]) -> Self {
        BootInfoFrameAllocator {
            memory_map,
            next: 0,
        }
    }

    /// Returns an iterator over the usable frames specified in the memory map.
    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> {
        let regions = self.memory_map.iter();
        let usable_regions = regions.filter(|r| r.kind == MemoryRegionKind::Usable);

        let addr_ranges = usable_regions.map(|r| r.start..r.end);

        let frame_addresses = addr_ranges.flat_map(|r| r.step_by(4096));

        frame_addresses.map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}

/// Returns a mutable reference to the active level 4 table.
///
/// This function is unsafe because the caller must guarantee that the
/// complete physical memory is mapped to virtual memory at the passed
/// `physical_memory_offset`. Also, this function must be only called once
/// to avoid aliasing `&mut` references (which is undefined behavior).
unsafe fn active_l4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;
    let (l4, _) = Cr3::read();

    let phys = l4.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    &mut *page_table_ptr
}

/// Initialize a new OffsetPageTable.
///
/// This function is unsafe because the caller must guarantee that the
/// complete physical memory is mapped to virtual memory at the passed
/// `physical_memory_offset`. Also, this function must be only called once
/// to avoid aliasing `&mut` references (which is undefined behavior).
pub unsafe fn init(offset: VirtAddr) -> OffsetPageTable<'static> {
    let l4 = active_l4_table(offset);
    OffsetPageTable::new(l4, offset)
}

/// This is here to break safety and should only be used under very
/// specific circumstances. Usage of this struct should be avoided
/// unless absolutely necessary.
///
/// Stores
#[allow(dead_code)]
pub(crate) struct VeryUnsafeFrameAllocator {
    addr: Option<PhysAddr>,
    virt_base: Option<VirtAddr>,
    advanced: usize,
}

#[allow(dead_code)]
impl VeryUnsafeFrameAllocator {
    /// Creates an instance of VeryUnsafeFrameAllocator
    ///
    /// This function is unsafe because it god damn well should be
    pub unsafe fn new() -> Self {
        Self {
            addr: None,
            virt_base: None,
            advanced: 0, // ((this - 1) * 4096) + addr = end frame address
        }
    }

    /// Sets physical addr if original VeryUnsafeFrameAllocator is dropped
    ///
    /// This function will panic if addr has already been set
    ///
    /// This function is unsafe because it potentially violates memory safety
    pub unsafe fn set_geom(&mut self, addr: PhysAddr, advance: usize) {
        if let None = self.addr {
            self.addr = Some(addr);
            self.advanced = advance
        } else {
            panic!("Cannot reassign physical address")
        }
    }

    pub fn get_advance(&self) -> usize {
        self.advanced
    }

    /// Allocates a range of physical frames to virtual memory via `self.advance`
    ///
    /// this works via calling `self.advance` do its caveats apply
    pub unsafe fn map_frames_from_range(
        &mut self,
        phy_addr_range: PhysFrameRangeInclusive,
        mapper: &mut OffsetPageTable,
    ) -> Option<VirtAddr> {
        self.set_geom(phy_addr_range.start.start_address(), 0);

        let mut virt_addr_base = None;

        for frame in phy_addr_range {
            if let None = virt_addr_base {
                virt_addr_base = self.advance(frame.start_address(), mapper);
            } else {
                self.advance(frame.start_address(), mapper);
            }
        }
        virt_addr_base
    }

    /// Maps a specified physical frame to a virtual page.
    /// Sets self.addr to an address with the last frame allocated
    ///

    pub unsafe fn get_frame(
        &mut self,
        phy_addr: PhysAddr,
        mapper: &mut OffsetPageTable,
    ) -> Option<VirtAddr> {
        // be careful with this

        self.set_geom(phy_addr, 0);

        self.advance(phy_addr, mapper)
    }

    /// Allocates memory by calling mapper.map_to
    ///
    /// In theory this only needs to be used to access memory written
    /// to by the firmware as such the writable bit is unset
    ///
    /// ##Panics
    /// this function will panic if it exceeds 2Mib of allocations
    /// because it cannot guarantee that the next page is free
    ///
    /// ##Saftey
    /// This function is unsafe because it violates memory safety
    pub unsafe fn advance(
        &mut self,
        phy_addr: PhysAddr,
        mapper: &mut OffsetPageTable,
    ) -> Option<VirtAddr> {
        return if let Some(page) = Self::find_unused_high_half(mapper) {
            mapper
                .map_to(
                    page,
                    PhysFrame::containing_address(phy_addr),
                    PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE,
                    self,
                )
                .unwrap()
                .flush();
            self.addr = None;
            Some(page.start_address())
        } else {
            None
        };
    }

    /// Unmaps Virtual page
    ///
    /// This function is unsafe because it can be used to unmap
    /// arbitrary Virtual memory
    pub unsafe fn dealloc_page(page: Page, mapper: &mut OffsetPageTable) {
        mapper.unmap(page).unwrap().1.flush();
    }

    /// Unmaps a range of Virtual pages
    ///
    /// This function is unsafe because it can be used to unmap
    /// arbitrary Virtual memory
    pub unsafe fn dealloc_pages(pages: PageRangeInclusive, mapper: &mut OffsetPageTable) {
        for page in pages {
            mapper.unmap(page).unwrap().1.flush();
        }
    }

    /// Searches fo an unused page within the high half of memory
    ///
    /// currently only scans for unused L3 page tables
    fn find_unused_high_half(mapper: &mut OffsetPageTable) -> Option<Page> {
        // todo make this better by traversing to L1 page tables
        for (e, p3) in mapper.level_4_table().iter().enumerate().skip(255) {
            if p3.is_unused() {
                return Some(Page::containing_address(VirtAddr::new((e << 39) as u64)));
                //conversion into 512GiB
            }
        }
        return None;
    }
}

unsafe impl FrameAllocator<Size4KiB> for VeryUnsafeFrameAllocator {
    //jesus christ this is bad
    //be careful with this
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(
            self.addr
                .expect("VeryUnsafeFrameAllocator failed addr not set")
                + (4096 * self.advanced),
        );
        self.advanced += 1;
        Some(frame)
    }
}

#[derive(PartialOrd, PartialEq, Debug, Copy, Clone)]
enum PageSizeLevel{
    L3,
    L2,
    L1,
}

impl PageSizeLevel{
    //const PAGE_SIZE: u32 = 4096;
    const PAGE_TABLE_ENTRIES: u32 = 512;
    const fn num_4k_pages(&self) -> u32 {
        return match self{
            PageSizeLevel::L3 => Self::PAGE_TABLE_ENTRIES * Self::PAGE_TABLE_ENTRIES,
            PageSizeLevel::L2 => Self::PAGE_TABLE_ENTRIES,
            PageSizeLevel::L1 => 1
        }
    }
}

#[derive(Clone, Copy)]
struct PageIterator{
    pub start: Page,
    pub end: Page,
}

impl PageIterator{

    /// skips to the next l2 index pretending that next() was never called
    fn skip_l2(&mut self){
        let n = self.start;
        let n = Page::containing_address(n.start_address() + 0x200000u64);

        if n > self.end {
            self.start = self.end
        }
        self.start = n
    }

    fn skip_l3(&mut self){
        let n = self.start;
        let n = Page::containing_address(n.start_address() + 0x40000000u64);

        if n > self.end {
            self.start = self.end
        }
        self.start = n
    }

    fn skip_l4(&mut self){
        let n = self.start;
        let n = Page::containing_address(n.start_address() + 0x8000000000u64);

        if n > self.end {
            self.start = self.end
        }
        self.start = n
    }

    fn step_back(&mut self){
        let n = self.start;
        let n = Page::containing_address(n.start_address() - 0x1000u64);

        if n > self.end {
            self.start = self.end
        }
        self.start = n
    }
}

impl Iterator for PageIterator{
    type Item = Page;

    fn next(&mut self) -> Option<Self::Item> {
        let old = self.start;


        let addr = old.start_address().as_u64();
        let new = Page::containing_address(VirtAddr::new(addr.checked_add(0x1000u64)?));

        return if new > self.end {
            None
        } else {
            self.start = new;
            Some(old)
        }


    }
}

pub(crate) const fn addr_from_indices(l4: usize, l3: usize, l2: usize, l1: usize) -> usize {
    const BITS_PAGE_OFFSET: u8 = 12;
    const BITS_TABLE_OFFSET: u8 = 9;

    assert!(l1 < 512);
    assert!(l2 < 512);
    assert!(l3 < 512);
    assert!(l4 < 512);

    let mut ret = l1 << BITS_PAGE_OFFSET;

    ret |= l2 << BITS_PAGE_OFFSET + (BITS_TABLE_OFFSET * 1 );
    ret |= l3 << BITS_PAGE_OFFSET + (BITS_TABLE_OFFSET * 2 );
    ret |= l4 << BITS_PAGE_OFFSET + (BITS_TABLE_OFFSET * 3 );

    ret
}