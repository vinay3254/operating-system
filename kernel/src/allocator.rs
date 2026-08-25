// kernel/src/allocator.rs
//
// Kernel heap allocator — Phase 7.
//
// ─── WHY WE NEED A HEAP ALLOCATOR ────────────────────────────────────────────
//
// Without a heap, everything must have a compile-time-known size on the stack.
// That rules out:
//   - `Box<T>` (owned heap allocation)
//   - `Vec<T>`, `String` (dynamically growing collections)
//   - `Arc<T>` (reference-counted sharing)
//   - `async`/`await` futures (which require boxing in our executor)
//
// Rust's `alloc` crate provides all of these, but they call `GlobalAllocator::alloc()`
// which panics if no `#[global_allocator]` is defined. We define one here.
//
// ─── BUMP ALLOCATOR ──────────────────────────────────────────────────────────
//
// The simplest correct heap allocator:
//
//   heap_start ────────────────────────────────── heap_end
//                ^                ^
//                │                │
//               next           (free)
//
// `allocate(size, align)`:
//   1. Round `next` up to `align` bytes (alignment requirement)
//   2. Check `next + size <= heap_end` (out of memory check)
//   3. Return old `next` as the allocation pointer
//   4. Advance `next` by `size`
//
// `deallocate(ptr, layout)`:
//   - Do nothing. Memory is never reclaimed.
//   - Once the heap fills up, all subsequent allocations fail.
//
// TRADE-OFFS vs. Linked-List Allocator
// ─────────────────────────────────────
// | Property              | Bump       | Linked-list             |
// |-----------------------|------------|-------------------------|
// | Allocation speed      | O(1)       | O(n) worst case         |
// | Deallocation          | No-op      | Merges free blocks      |
// | Memory efficiency     | Wastes all | Reuses freed memory     |
// | Implementation size   | ~30 lines  | ~200 lines              |
// | Correct for our use?  | Yes        | Yes (more complex)      |
//
// For a learning kernel running async tasks that allocate Box<Future> once and
// never drop them in a tight loop, bump allocation is perfectly adequate.
// If you later want deallocation, swap out `BumpAllocator` for
// `linked_list_allocator::Heap` (already in Cargo.toml for this purpose).
//
// ─── HEAP VIRTUAL ADDRESS RANGE ──────────────────────────────────────────────
//
// We carve out a fixed virtual address range for the heap.
// The address 0x_4444_4444_0000 is chosen arbitrarily — far from:
//   - Kernel code/data (low addresses)
//   - The identity-mapped physical memory (wherever bootloader puts it)
//   - The stack (near top of virtual space)
// 100 KiB is conservative but sufficient for hello-world-level async tasks.

use alloc::alloc::{GlobalAlloc, Layout};
use core::ptr;
use spin::Mutex;
use x86_64::{
    VirtAddr,
    structures::paging::{mapper::MapToError, FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB},
};

/// Virtual address where our heap starts.
pub const HEAP_START: usize = 0x_4444_4444_0000;
/// Size of the heap in bytes.
pub const HEAP_SIZE: usize = 100 * 1024; // 100 KiB

// ─── GlobalAllocator registration ────────────────────────────────────────────

/// The global kernel allocator — a spinlock-wrapped bump allocator.
///
/// `#[global_allocator]` tells the Rust runtime to use this for all `alloc::*` calls
/// (Box::new, Vec::new, Arc::new, async executor task boxing, etc.).
#[global_allocator]
static ALLOCATOR: Locked<BumpAllocator> = Locked::new(BumpAllocator::new());

// ─── Heap initialization ──────────────────────────────────────────────────────

/// Map the heap's virtual address range into physical memory, then initialize the allocator.
///
/// Called once from `kernel_main` after the page table mapper and frame allocator are ready.
pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    // The heap spans from HEAP_START to HEAP_START + HEAP_SIZE.
    // We must map every 4 KiB page in this range to a physical frame.
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + HEAP_SIZE as u64 - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;

        // Flags: PRESENT (the page exists) + WRITABLE (the heap must be writable).
        // We don't set USER_ACCESSIBLE — heap is kernel-only.
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

        unsafe {
            // Map the virtual heap page to the physical frame.
            mapper.map_to(page, frame, flags, frame_allocator)?.flush();
        }
    }

    // Tell the bump allocator about the now-valid heap range.
    unsafe {
        ALLOCATOR.lock().init(HEAP_START, HEAP_SIZE);
    }

    Ok(())
}

// ─── BumpAllocator ────────────────────────────────────────────────────────────

/// A bump (linear) allocator.
///
/// Allocates by advancing `next` forward. Never deallocates.
/// Thread-safe when wrapped in `Locked<BumpAllocator>`.
pub struct BumpAllocator {
    heap_start: usize,
    heap_end: usize,
    next: usize,         // next free byte (advances on each allocation)
    allocations: usize,  // count of live allocations (informational only)
}

impl BumpAllocator {
    /// Create an uninitialized allocator.
    /// `init()` must be called before any allocation.
    pub const fn new() -> Self {
        BumpAllocator {
            heap_start: 0,
            heap_end: 0,
            next: 0,
            allocations: 0,
        }
    }

    /// Initialize the allocator with the heap's virtual address range.
    ///
    /// # Safety
    /// The caller must ensure `[heap_start, heap_start + heap_size)` is valid,
    /// mapped, writable virtual memory. Called exactly once by `init_heap()`.
    pub unsafe fn init(&mut self, heap_start: usize, heap_size: usize) {
        self.heap_start = heap_start;
        self.heap_end = heap_start + heap_size;
        self.next = heap_start;
    }
}

unsafe impl GlobalAlloc for Locked<BumpAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut bump = self.lock();

        // Step 1: Align `next` up to the required alignment.
        // Most types need 8-byte or 16-byte alignment. The bump pointer may be
        // misaligned from the previous allocation, so we pad to the next boundary.
        let alloc_start = align_up(bump.next, layout.align());

        // Check for overflow in the address arithmetic.
        let alloc_end = match alloc_start.checked_add(layout.size()) {
            Some(end) => end,
            None => return ptr::null_mut(), // arithmetic overflow → OOM
        };

        // Step 2: Out-of-memory check.
        if alloc_end > bump.heap_end {
            return ptr::null_mut(); // heap exhausted
        }

        // Step 3: Commit the allocation.
        bump.next = alloc_end;
        bump.allocations += 1;
        alloc_start as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        let mut bump = self.lock();
        bump.allocations -= 1;

        // Bump allocator optimization: if ALL allocations have been deallocated,
        // we can reset `next` back to the start (full heap reclaim).
        // This only happens if everything allocated in a "generation" is freed together.
        if bump.allocations == 0 {
            bump.next = bump.heap_start;
        }

        // Otherwise: no-op. Bump allocators don't reclaim individual allocations.
        // This is by design — see module docs for trade-offs.
    }
}

// ─── Locked wrapper ───────────────────────────────────────────────────────────

/// A `spin::Mutex` wrapper that satisfies `GlobalAlloc` requirements.
///
/// `GlobalAlloc` requires `unsafe impl` (since alloc/dealloc are unsafe),
/// and the standard `Mutex` doesn't implement it — so we use this newtype.
pub struct Locked<A> {
    inner: Mutex<A>,
}

impl<A> Locked<A> {
    pub const fn new(inner: A) -> Self {
        Locked {
            inner: Mutex::new(inner),
        }
    }

    pub fn lock(&self) -> spin::MutexGuard<'_, A> {
        self.inner.lock()
    }
}

// ─── Alignment helper ─────────────────────────────────────────────────────────

/// Round `addr` up to the nearest multiple of `align`.
///
/// `align` MUST be a power of 2 (which Rust's `Layout` always guarantees).
///
/// Trick: `(addr + align - 1) & !(align - 1)`
///   - Adding `align - 1` ensures we overshoot to at least the next boundary
///   - Masking with `!(align - 1)` clears the low bits to align down
/// The combination gives us "align up".
fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}
