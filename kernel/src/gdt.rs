// kernel/src/gdt.rs
//
// Global Descriptor Table (GDT) — Phase 2.
//
// WHY WE NEED A GDT IN 64-BIT MODE
// ──────────────────────────────────
// x86_64 long mode doesn't do segmented memory like x86 real/protected mode — every
// segment covers the full 64-bit address space. But the CPU still requires a valid GDT
// to be loaded at all times. More importantly:
//
//   1. The code segment selector (CS) must match what the CPU expects for 64-bit long mode
//      (L=1 flag). The bootloader leaves its own GDT loaded — we must define ours.
//
//   2. The Task State Segment (TSS) is still used in 64-bit mode for one critical job:
//      the Interrupt Stack Table (IST). When a hardware exception fires, the CPU can
//      automatically switch to a dedicated stack listed in the IST — essential for
//      catching stack overflows.
//
// WITHOUT IST FOR DOUBLE FAULTS
// ──────────────────────────────
// If the kernel stack overflows:
//   1. The CPU tries to push the exception frame onto the (overflowed) stack → fault
//   2. Double fault fires → tries to push frame onto the same overflowed stack → fault
//   3. Triple fault → CPU resets → QEMU reboots → no debug output whatsoever
//
// With IST entry 0 pointing at a separate DOUBLE_FAULT_STACK, the CPU switches to that
// stack before handling the double fault, so we can see the panic message.
//
// GDT LAYOUT
// ───────────
//   Slot 0: Null descriptor (mandatory — hardware ignores it but its presence is required)
//   Slot 1: Kernel code segment (ring 0, 64-bit)
//   Slot 2+3: TSS descriptor (128 bits wide — uses two GDT slots in 64-bit mode)

use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// IST entry index (0-based) used for the double-fault handler stack.
/// "IST index 0" in the x86_64 crate corresponds to IST1 in the CPU manual
/// (the manual's IST entries are 1-based; the crate normalizes to 0-based).
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Size of the stack reserved for double-fault handling.
/// 20 KiB is plenty for a minimal double-fault handler that just prints and halts.
/// Oversizing is safe — it's a static array, so it's always present in the binary.
const DOUBLE_FAULT_STACK_SIZE: usize = 4096 * 5; // 20 KiB

lazy_static! {
    /// Task State Segment.
    ///
    /// We statically allocate the double-fault stack here and store its TOP address
    /// in IST[DOUBLE_FAULT_IST_INDEX]. The CPU switches to this stack automatically
    /// when it delivers a double-fault exception.
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();

        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            // Static byte array for the double-fault stack.
            // `static mut` is safe here because:
            //   - TSS is initialized exactly once before any interrupts fire
            //   - Only the CPU ever "uses" this memory (stack operations during exception)
            //   - We never alias it from Rust code
            static mut DOUBLE_FAULT_STACK: [u8; DOUBLE_FAULT_STACK_SIZE] =
                [0; DOUBLE_FAULT_STACK_SIZE];

            let stack_start = VirtAddr::from_ptr(&raw const DOUBLE_FAULT_STACK);
            // The x86 stack grows DOWN: we must point to the HIGH address (the top).
            stack_start + DOUBLE_FAULT_STACK_SIZE as u64
        };

        tss
    };
}

/// Segment selectors we need to reload after loading our GDT.
pub struct Selectors {
    /// Kernel code segment selector — loaded into CS.
    pub code_selector: SegmentSelector,
    /// TSS selector — loaded into TR (Task Register).
    pub tss_selector: SegmentSelector,
}

lazy_static! {
    /// The Global Descriptor Table and the segment selectors.
    ///
    /// `lazy_static!` defers construction to first access (safe for no_std).
    /// We return both the GDT and the selectors because after loading the GDT
    /// we need the selector values to reload CS and TR.
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();

        // Kernel code segment: ring 0, 64-bit long mode (L=1 flag set by the x86_64 crate).
        // x86_64 0.15 renamed add_entry() → append()
        let code_selector = gdt.append(Descriptor::kernel_code_segment());

        // TSS descriptor: 128-bit entry pointing to our TSS.
        // The x86_64 crate handles the split across two GDT slots automatically.
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));

        (gdt, Selectors { code_selector, tss_selector })
    };
}

/// Load our GDT, reload CS, and load the TSS.
///
/// Must be called early in `kernel_main`, before enabling interrupts.
///
/// # Why we must reload CS and TR after loading the GDT
/// `lgdt` (load GDT) only updates the GDTR register — it does NOT update the cached
/// segment registers. CS, DS, SS, etc. still hold their old selector values which
/// pointed into the BOOTLOADER'S GDT. We must explicitly reload them to point into
/// our new GDT.
pub fn init() {
    use x86_64::instructions::segmentation::{CS, Segment};
    use x86_64::instructions::tables::load_tss;

    // 1. Load the GDT: writes our GDT address + size into GDTR.
    GDT.0.load();

    unsafe {
        // 2. Reload CS (Code Segment register).
        //    CS can't be loaded with a normal MOV — the x86_64 crate does a far RETF trick.
        CS::set_reg(GDT.1.code_selector);

        // 3. Load the TSS selector into TR (Task Register) via the `ltr` instruction.
        //    This tells the CPU: "when you need to switch stacks for an exception, look
        //    at IST entries from THIS TSS".
        load_tss(GDT.1.tss_selector);
    }
}
