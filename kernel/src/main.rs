// kernel/src/main.rs
//
// Kernel entry point — wires all phases together.
//
// BOOT FLOW
// ─────────
// 1. BIOS loads the bootloader from the MBR
// 2. Bootloader switches CPU to 64-bit long mode, sets up initial page tables,
//    requests a VESA framebuffer, scans the memory map, then calls kernel_main()
// 3. kernel_main() initializes each subsystem in dependency order:
//    Phase 1: Framebuffer (need output ASAP)
//    Phase 2: GDT + TSS   (need before IDT for IST stacks)
//    Phase 3: IDT          (need before enabling hardware interrupts)
//    Phase 4: PIC          (hardware interrupts)
//    Phase 5+6: Memory     (frame allocator + page table mapper)
//    Phase 7: Heap         (need memory before heap; need heap before async)
//    Phase 8+9: Executor   (need heap to box futures; need interrupts for wakers)
//
// NO_STD / NO_MAIN
// ─────────────────
// `#![no_std]`: don't link the Rust standard library (it requires an OS).
//               We use only `core` (always available) and `alloc` (our own heap).
// `#![no_main]`: the normal `main()` entry is not used. `entry_point!` macro
//                defines a custom `_start` / `kernel_main` entry point that the
//                bootloader calls directly.
//
// FEATURE FLAGS
// ─────────────
// `alloc_error_handler`: nightly feature that lets us define what happens when
//                         an allocation fails. Without this, OOM causes an abort
//                         with no message. With it, we can print a panic message.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point};
use core::panic::PanicInfo;

mod allocator;
mod framebuffer;
mod gdt;
mod interrupts;
mod memory;
mod task;

// ─── Bootloader configuration ─────────────────────────────────────────────────

/// Configuration passed to the bootloader at compile time.
///
/// `physical_memory = Some(Mapping::Dynamic)`:
///   Tells the bootloader to map ALL physical memory into the kernel's virtual
///   address space at some offset, and report that offset in BootInfo.
///   This is what `OffsetPageTable` (memory.rs) needs to walk page tables —
///   without this we can't dereference physical page table addresses.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

// `entry_point!` macro does three things:
//   1. Generates the `_start` symbol the bootloader jumps to
//   2. Ensures our function has the correct signature (takes &'static mut BootInfo)
//   3. Embeds BOOTLOADER_CONFIG into the binary so the bootloader reads it
entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

// ─── Kernel main ─────────────────────────────────────────────────────────────

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // ── Phase 1: Framebuffer ─────────────────────────────────────────────────
    //
    // Initialize first so all subsequent phases can print diagnostic messages.
    // If the framebuffer isn't available, we have no output — hard crash.
    let framebuffer = boot_info
        .framebuffer
        .as_mut()
        .expect("no framebuffer provided by bootloader");
    framebuffer::init(framebuffer);

    println!("=== Kernel booting ===");
    println!();

    // ── Phase 2: GDT ─────────────────────────────────────────────────────────
    //
    // Load our own GDT (replacing the bootloader's temporary one) and register
    // the TSS with the double-fault IST stack. Must happen before IDT.
    gdt::init();
    println!("[GDT] loaded  (kernel code seg + TSS with double-fault IST stack)");

    // ── Phase 3: IDT ─────────────────────────────────────────────────────────
    //
    // Load the Interrupt Descriptor Table. After this, CPU exceptions (divide-by-zero,
    // page fault, etc.) will be caught and printed instead of triple-faulting.
    interrupts::init_idt();
    println!("[IDT] loaded  (breakpoint, double-fault, page-fault handlers active)");

    // Verify the IDT is working: trigger a breakpoint exception intentionally.
    // We should see the breakpoint handler's output, then execution continues here.
    x86_64::instructions::interrupts::int3();
    println!("[IDT] breakpoint exception caught successfully");

    // ── Phase 4: PIC (hardware interrupts) ────────────────────────────────────
    //
    // Initialize and remap the 8259 PIC, then enable CPU interrupts (sti).
    // After this: timer fires ~18Hz, keyboard IRQ fires on keypress.
    unsafe {
        interrupts::PICS.lock().initialize();
    }
    x86_64::instructions::interrupts::enable();
    println!("[PIC] initialized  (timer + keyboard IRQs active)");

    // ── Phase 5+6: Memory management ──────────────────────────────────────────
    //
    // The bootloader maps all physical memory at `physical_memory_offset`.
    // We use this to initialize OffsetPageTable (so we can modify page tables)
    // and BootInfoFrameAllocator (so we have free physical frames to map).
    let phys_mem_offset = x86_64::VirtAddr::new(
        boot_info
            .physical_memory_offset
            .into_option()
            .expect("physical memory offset not set — check BOOTLOADER_CONFIG"),
    );

    let mut mapper = unsafe { memory::init(phys_mem_offset) };
    let mut frame_allocator =
        unsafe { memory::BootInfoFrameAllocator::init(&boot_info.memory_regions) };
    println!("[MEM] page table mapper + frame allocator ready");

    // ── Phase 7: Heap ─────────────────────────────────────────────────────────
    //
    // Map a virtual address range as the kernel heap and initialize the bump allocator.
    // After this, `Box`, `Vec`, `Arc`, `BTreeMap`, and `async` futures all work.
    allocator::init_heap(&mut mapper, &mut frame_allocator)
        .expect("heap initialization failed");
    println!(
        "[HEAP] {} KiB at {:#x}  (bump allocator)",
        allocator::HEAP_SIZE / 1024,
        allocator::HEAP_START,
    );

    // ── Phase 8+9: Async executor ─────────────────────────────────────────────
    //
    // Spawn tasks and run the executor. The executor never returns —
    // it loops: poll ready tasks → hlt if idle → wake on interrupt → repeat.
    println!();
    println!("[EXEC] starting async task executor");
    println!("[EXEC] tasks: example_task, keyboard::print_keypresses");
    println!();
    println!("--- Kernel ready. Type something! ---");

    let mut executor = task::executor::Executor::new();
    executor.spawn(task::Task::new(example_task()));
    executor.spawn(task::Task::new(task::keyboard::print_keypresses()));
    executor.run(); // does not return
}

// ─── Example async task ───────────────────────────────────────────────────────

/// A minimal async task to demonstrate that the executor works.
///
/// Runs to completion on the first poll (no `await` points that return Pending),
/// then gets dropped by the executor. Output appears right after boot messages.
async fn example_task() {
    let n = compute_answer().await;
    println!("[TASK] example_task complete: the answer is {}", n);
}

async fn compute_answer() -> u32 {
    42 // immediately ready — no actual async I/O, just demonstrating the plumbing
}

// ─── Panic handler ────────────────────────────────────────────────────────────

/// Called by the Rust runtime on any `panic!()`, failed `unwrap()`, etc.
///
/// We print the panic info (message + location) to the framebuffer,
/// then halt. We don't attempt to unwind or recover — in a kernel, a panic
/// means something is fundamentally wrong and we need a human to look at it.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Print without the interrupt lock — if we panicked while holding it,
    // we're already in a bad state. Best effort.
    println!();
    println!("!!! KERNEL PANIC !!!");
    println!("{}", info);
    println!("System halted.");

    // Halt the CPU. Spin in the hlt loop so we don't accidentally execute past here.
    loop {
        x86_64::instructions::hlt();
    }
}

/// Called by the `alloc` crate when an allocation fails (returns null).
///
/// With bump allocation, this means the heap is exhausted.
/// We treat it as a panic — print the layout that failed and halt.
#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    panic!("heap allocation failed: {:?}", layout)
}
