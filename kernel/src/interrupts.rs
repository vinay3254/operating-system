// kernel/src/interrupts.rs
//
// Interrupt Descriptor Table (IDT) + Programmable Interrupt Controller (8259 PIC)
// Phases 3 and 4.
#![allow(clippy::empty_loop)]

//
// ─── PHASE 3: CPU EXCEPTIONS (IDT) ──────────────────────────────────────────
//
// Without an IDT, any CPU exception causes a triple fault (immediate reset).
// The IDT maps exception/interrupt numbers (0–255) to handler functions.
//
// Exception numbers 0–31 are CPU exceptions (Intel-defined):
//   #0  Division by zero
//   #3  Breakpoint (int3 instruction) — used by debuggers
//   #6  Invalid opcode
//   #8  Double fault (exception while handling an exception)
//   #14 Page fault
//   ...
//
// We register handlers for:
//   - Breakpoint (#3): safe, no error code, execution continues after handler
//   - Double fault (#8): fatal, runs on IST stack (from gdt.rs), prints + halts
//   - Page fault (#14): prints fault address + reason, halts
//
// ─── PHASE 4: HARDWARE INTERRUPTS (PIC) ──────────────────────────────────────
//
// CPU exceptions are software events (caused by CPU itself). Hardware interrupts
// come from external devices via the 8259 PIC (Programmable Interrupt Controller).
//
// The 8259 PIC sits between hardware devices and the CPU interrupt pin.
// It has a MASTER PIC (IRQ0–IRQ7) and a SLAVE PIC (IRQ8–IRQ15) chained together.
//
// CRITICAL: By default, the PIC maps hardware IRQs to interrupt vectors 0–15,
// which OVERLAP with CPU exceptions (0–31). This causes "ghost exceptions" —
// a timer tick fires vector 0 which the CPU thinks is a division-by-zero fault.
//
// We REMAP the PICs:
//   Master PIC: IRQ 0–7  → vectors PIC_1_OFFSET (32) to 39
//   Slave  PIC: IRQ 8–15 → vectors PIC_2_OFFSET (40) to 47
//
// Hardware IRQs we handle:
//   IRQ0 (vector 32): Timer — fires ~18.2 Hz from the PIT. We count ticks + send EOI.
//   IRQ1 (vector 33): PS/2 Keyboard — fires on key press/release. We read scancode + queue it.
//
// END OF INTERRUPT (EOI)
// ──────────────────────
// After handling any hardware interrupt, we MUST send an EOI command to the PIC.
// Without EOI, the PIC assumes the interrupt is still being handled and stops
// sending new interrupts of equal or lower priority → keyboard and timer stop working.

use crate::gdt;
use crate::task::keyboard;
use crate::println;
use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

// ─── PIC configuration ────────────────────────────────────────────────────────

/// Master PIC remapped to start at vector 32 (just above CPU exceptions 0–31).
pub const PIC_1_OFFSET: u8 = 32;
/// Slave PIC remapped to start at vector 40 (immediately after master's 8 entries).
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

/// The chained 8259 PIC pair, protected by a spinlock.
///
/// `unsafe`: ChainedPics::new() is unsafe because misconfiguring the PIC
/// offsets could cause the CPU to misroute interrupts, but our choice of 32/40
/// is the standard safe remapping.
pub static PICS: Mutex<ChainedPics> = Mutex::new(unsafe {
    ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET)
});

/// Maps interrupt numbers to human-readable names (for the PIC interrupt index enum).
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    /// PIT timer interrupt (IRQ0 → vector PIC_1_OFFSET + 0 = 32).
    Timer = PIC_1_OFFSET,
    /// PS/2 keyboard interrupt (IRQ1 → vector PIC_1_OFFSET + 1 = 33).
    Keyboard,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

// ─── IDT ─────────────────────────────────────────────────────────────────────

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        // ── CPU Exceptions ──────────────────────────────────────────────────

        // Breakpoint: triggered by `int3` instruction (or a debugger breakpoint).
        // The handler runs on the NORMAL kernel stack — this is safe because
        // a breakpoint is an intentional, expected exception, not a fault.
        // After the handler returns, execution continues at the next instruction.
        idt.breakpoint.set_handler_fn(breakpoint_handler);

        // Double fault: triggered when an exception occurs WHILE handling another exception,
        // OR when the CPU can't deliver an exception (e.g. IDT not loaded, stack overflow).
        // We run this on IST stack index DOUBLE_FAULT_IST_INDEX from our TSS —
        // so even if the kernel stack is fully overflowed, we still get a handler call.
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }

        // Page fault: triggered when a virtual address is accessed but:
        //   - The page table entry doesn't exist (not mapped)
        //   - A privilege violation occurs (ring 3 accessing ring 0 page)
        //   - A write to a read-only page
        //   - An instruction fetch from a non-executable page
        // The error code and CR2 register tell us exactly what happened.
        idt.page_fault.set_handler_fn(page_fault_handler);

        // ── Hardware Interrupts (PIC) ────────────────────────────────────────

        // Timer (IRQ0): fires from the 8253/8254 PIT at ~18.2 Hz.
        // x86_64 0.15: IDT is indexed by u8, not usize
        idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);

        // Keyboard (IRQ1): fires when a key is pressed or released.
        idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);

        idt
    };
}

/// Load our IDT into the IDTR register.
///
/// Must be called after `gdt::init()` (so the TSS is loaded for IST to work).
pub fn init_idt() {
    IDT.load();
}

// ─── Exception handlers ───────────────────────────────────────────────────────

/// Breakpoint handler (#3).
///
/// Called when `x86_64::instructions::interrupts::int3()` is executed.
/// We just print a message and return — execution continues at the next instruction.
/// Useful for verifying that the IDT is set up correctly before touching anything else.
extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    println!("[EXCEPTION] BREAKPOINT\n{:#?}", stack_frame);
}

/// Double fault handler (#8).
///
/// Called when an exception fires while already handling an exception, OR when
/// the CPU can't deliver an exception (e.g. stack overflow, IDT not loaded).
/// This is a DIVERGING handler — it must never return (double fault is not resumable).
///
/// Runs on the dedicated IST stack (configured in gdt.rs) so it works even if
/// the kernel stack is completely overflowed.
extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64, // always 0 for double fault, but we must accept the parameter
) -> ! {
    panic!("[EXCEPTION] DOUBLE FAULT\n{:#?}", stack_frame);
}

/// Page fault handler (#14).
///
/// The CPU stores the faulting virtual address in CR2 before delivering the exception.
/// `PageFaultErrorCode` tells us WHY: protection violation, write to read-only, etc.
extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    println!("[EXCEPTION] PAGE FAULT");
    println!(
        "  Accessed address: {:?}",
        x86_64::registers::control::Cr2::read()
    );
    println!("  Error code: {:?}", error_code);
    println!("{:#?}", stack_frame);

    // Page faults are not recoverable without memory management infrastructure.
    // For now, halt.
    hlt_loop();
}

// ─── Hardware interrupt handlers ──────────────────────────────────────────────

/// Timer interrupt handler (IRQ0, vector 32).
///
/// Fires ~18.2 times per second from the Programmable Interval Timer (PIT).
/// We don't use it for scheduling yet — just send EOI to keep the PIC happy.
/// A tick counter could be added here later for simple timekeeping.
extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    // SAFETY: Sending EOI to the PIC is safe — it's just a port I/O write.
    // Without EOI the PIC would not send any more timer interrupts.
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
}

/// Keyboard interrupt handler (IRQ1, vector 33).
///
/// Fires when a PS/2 key is pressed or released.
/// We read the raw scancode from I/O port 0x60 and push it to the async scancode queue.
///
/// WHY NOT DECODE HERE?
/// Interrupt handlers must be short and non-blocking. Decoding scancodes (especially
/// for multi-byte extended keys) is more complex and should happen in a task context
/// where we can take our time. We just queue the raw byte and return.
extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;

    // Read the raw scancode from PS/2 controller data port 0x60.
    // SAFETY: Port 0x60 is the standard PS/2 data port. Reading it is always safe.
    let scancode: u8 = unsafe { Port::new(0x60).read() };

    // Push to the async queue so the keyboard task can decode it.
    keyboard::add_scancode(scancode);

    // Send EOI to the PIC so it resumes sending keyboard interrupts.
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

// ─── Halt loop ────────────────────────────────────────────────────────────────

/// Halt the CPU until the next interrupt, then repeat.
///
/// This is the kernel's idle loop — much better than spinning (`loop {}`) because
/// `hlt` puts the CPU into a low-power state. The CPU wakes on any interrupt
/// (timer, keyboard, etc.), processes it, then halts again.
pub fn hlt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}
