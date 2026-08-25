// kernel/src/memory.rs
//
// Physical frame allocation and virtual memory mapping — Phases 5 and 6.
//
// ─── PHASE 5: PHYSICAL FRAME ALLOCATOR ───────────────────────────────────────
//
// To map virtual pages to physical memory, we need free physical "frames"
// (4 KiB aligned chunks of physical RAM). The bootloader's MemoryRegions tell us
// which physical address ranges are available (Usable) versus reserved by firmware.
//
// We implement a simple linear-scan allocator: iterate through Usable regions,
// hand out frames one by one. This is "bump allocation" for physical frames — simple,
// no deallocation, works perfectly for a kernel that bootstraps itself once.
//
// ─── PHASE 6: PAGING / VIRTUAL MEMORY ────────────────────────────────────────
//
// x86_64 uses 4-level paging. A virtual address is split as:
//
//   [63..48] Sign extension (must match bit 47)
//   [47..39] L4 index (9 bits → 512 entries per table)
//   [38..30] L3 index (9 bits)
//   [29..21] L2 index (9 bits)
//   [20..12] L1 index (9 bits)
//   [11..0]  Page offset (12 bits → 4096 byte pages)
//
// Each level is a 4 KiB page table with 512 8-byte entries (PTEs).
// The CPU walks this tree (hardware page table walk) on every memory access.
//
// OffsetPageTable
// ───────────────
// To manipulate page tables, we need to read/write their PHYSICAL addresses —
// but we can only address memory VIRTUALLY in 64-bit mode. The bootloader solves
// this by mapping ALL physical memory at a fixed virtual offset (physical_memory_offset).
// So physical address P is accessible at virtual address P + physical_memory_offset.
//
// `OffsetPageTable` from the x86_64 crate handles this translation for us.
// We give it the offset once at init, and it can dereference any physical page table
// address by adding the offset.
//
// map_page()
// ──────────
// Maps a virtual `Page` to a physical `PhysFrame` with specified access flags.
// Internally: walks the L4→L3→L2→L1 chain, creating intermediate tables if needed
// (using `frame_allocator` to allocate physical frames for new tables), then writes
// the final L1 PTE with the physical frame address + flags.

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame,
        Size4KiB,
    },
};

// ─── Phase 6: OffsetPageTable init ───────────────────────────────────────────

/// Initialize an `OffsetPageTable` for the currently active level-4 page table.
///
/// # Safety
/// - `physical_memory_offset` must be the correct virtual offset where all physical
///   memory is linearly mapped (provided by the bootloader via BootInfo).
/// - This function must be called only once. Calling it again would create a second
///   mutable reference to the same page table, causing undefined behavior.
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let level4_table = active_level4_table(physical_memory_offset);
    OffsetPageTable::new(level4_table, physical_memory_offset)
}

/// Read the currently active L4 page table pointer from CR3 and return a mutable reference.
///
/// # Safety
/// - `physical_memory_offset` must correctly map all physical addresses to virtual ones.
/// - Caller must ensure no aliasing (call only once).
unsafe fn active_level4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    // CR3 holds the physical address of the currently active L4 page table.
    let (level4_table_frame, _) = Cr3::read();

    // Convert the physical address to a virtual address via the offset mapping.
    let phys = level4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();

    // Interpret the virtual address as a pointer to a PageTable and dereference it.
    // SAFETY: The bootloader guarantees this frame is a valid L4 page table.
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
    &mut *page_table_ptr
}

// ─── Phase 5 + 6: map_page helper ────────────────────────────────────────────

/// Map a virtual `page` to a new physical frame with the given `flags`.
///
/// Allocates intermediate page table frames as needed using `frame_allocator`.
/// This is a thin wrapper around `mapper.map_to()` that flushes the TLB after mapping.
///
/// TLB (Translation Lookaside Buffer) flush: the CPU caches page table lookups.
/// After modifying a PTE, we must invalidate the cached translation for that address
/// with `invlpg` — otherwise the CPU continues using the stale (old) mapping.
/// `MapperFlush::flush()` does this automatically.
#[allow(dead_code)]
pub fn map_page(
    page: Page,
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    flags: PageTableFlags,
) {
    // Allocate a physical frame to back this virtual page.
    let frame = frame_allocator
        .allocate_frame()
        .expect("out of physical frames");

    // SAFETY: `mapper.map_to` is unsafe because:
    //   - We must ensure `frame` is not already mapped to another page (it won't be —
    //     our allocator only gives out each frame once).
    //   - We must ensure the frame is writable (it is — general RAM).
    let flush = unsafe {
        mapper
            .map_to(page, frame, flags, frame_allocator)
            .expect("map_to failed")
    };

    // Flush the TLB for this specific page (invlpg instruction).
    flush.flush();
}

// ─── Phase 5: BootInfoFrameAllocator ─────────────────────────────────────────

/// A physical frame allocator backed by the bootloader's memory map.
///
/// Strategy: iterate through memory regions, find Usable ones, hand out their
/// frames one by one via a counter (`next_frame`). Simple and correct for kernel init.
///
/// Limitation: frames are NEVER returned. Once allocated, a frame is gone.
/// This is fine for a kernel bootstrap allocator — once the heap is up,
/// heap-level allocations use the bump/linked-list allocator in allocator.rs.
pub struct BootInfoFrameAllocator {
    memory_regions: &'static MemoryRegions,
    next_frame: usize,
}

impl BootInfoFrameAllocator {
    /// Create a new allocator from the bootloader-provided memory map.
    ///
    /// # Safety
    /// The caller must guarantee that the memory map is accurate and that all
    /// Usable frames are actually free (the bootloader guarantees this at boot time).
    pub unsafe fn init(memory_regions: &'static MemoryRegions) -> Self {
        Self {
            memory_regions,
            next_frame: 0,
        }
    }

    /// Iterator over all usable physical frames in the memory map.
    ///
    /// We take each Usable region, convert its byte range to 4 KiB frame addresses,
    /// and yield `PhysFrame` values.
    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> {
        self.memory_regions
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
            .map(|r| r.start..r.end)                    // byte ranges
            .flat_map(|r| r.step_by(4096))              // 4 KiB-aligned addresses
            .map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

/// Implement `FrameAllocator` so `BootInfoFrameAllocator` can be passed to `mapper.map_to()`.
unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        // Skip frames we've already given out by taking the nth frame.
        // This is O(n) per allocation — acceptable because frame allocation only
        // happens a few hundred times during kernel init (heap setup + page table creation).
        // Once the heap is live, we stop using this allocator for fine-grained allocs.
        let frame = self.usable_frames().nth(self.next_frame);
        self.next_frame += 1;
        frame
    }
}
