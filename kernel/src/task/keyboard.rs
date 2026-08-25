// kernel/src/task/keyboard.rs
//
// Async keyboard input task — Phase 8.
//
// ─── TWO-HALF DESIGN ─────────────────────────────────────────────────────────
//
// Keyboard input is split across two contexts that run at different priorities:
//
//   1. INTERRUPT HALF (runs at interrupt priority, must be fast)
//      `add_scancode()` — called by the IRQ1 handler in interrupts.rs
//      Reads the raw scancode byte from port 0x60, pushes it to a lock-free queue.
//      Returns immediately. Must not block, must not allocate, must not acquire locks
//      (to avoid priority inversion: the interrupt could interrupt a task holding the lock).
//
//   2. TASK HALF (runs as a normal async task at task priority)
//      `print_keypresses()` — the async task spawned by the executor.
//      Reads from the queue, decodes scancodes using `pc-keyboard`, prints characters.
//      Can be slow, can allocate, can block (yields via Poll::Pending).
//
// ─── LOCK-FREE QUEUE: ArrayQueue ─────────────────────────────────────────────
//
// `crossbeam_queue::ArrayQueue` is a bounded MPSC (multi-producer, single-consumer)
// lock-free ring buffer. It's safe to push from interrupt context and pop from
// task context simultaneously without any mutex — perfect for this use case.
//
// Bounded size (100 entries) means it never needs to allocate at interrupt time.
// If the queue fills up (task can't keep up), we drop scancodes silently —
// acceptable behavior (a stuck key or burst of fast typing).
//
// ─── WAKER-BASED SLEEP ───────────────────────────────────────────────────────
//
// When the queue is empty, the keyboard task should SLEEP instead of spinning.
// We implement a `ScancodeStream` future that:
//   1. Tries to pop from the queue
//   2. If empty: stores the Waker in `WAKER` and returns Poll::Pending
//   3. `add_scancode()` calls `WAKER.wake()` after pushing → executor re-polls the task
//   4. Task wakes, pops scancode, processes it, sleeps again
//
// This integrates cleanly with the HLT-based idle loop in the executor:
// the CPU sleeps until the keyboard interrupt fires, which both queues the scancode
// AND wakes the keyboard task via the stored Waker.

use conquer_once::spin::OnceCell;
use core::{
    pin::Pin,
    task::{Context, Poll},
};
use crate::print;
use crossbeam_queue::ArrayQueue;
use futures_util::{stream::Stream, task::AtomicWaker, StreamExt};
use pc_keyboard::{layouts, DecodedKey, HandleControl, Keyboard, ScancodeSet1};

// ─── Shared state (interrupt ↔ task) ─────────────────────────────────────────

/// Bounded scancode queue — lock-free, shared between IRQ1 handler and keyboard task.
///
/// `OnceCell` (from conquer-once) allows safe lazy initialization without std.
/// We can't use `lazy_static` here because `ArrayQueue::new()` requires a heap
/// allocation, and `lazy_static` initializes at first access — which might happen
/// before the heap is set up. `OnceCell` lets us initialize it explicitly.
static SCANCODE_QUEUE: OnceCell<ArrayQueue<u8>> = OnceCell::uninit();

/// Waker stored by `ScancodeStream::poll_next()` and called by `add_scancode()`.
///
/// `AtomicWaker` is a lock-free, thread/interrupt-safe waker cell.
/// It handles the race between "task checks for scancodes and registers waker"
/// and "interrupt fires and calls wake" without a mutex.
static WAKER: AtomicWaker = AtomicWaker::new();

// ─── Interrupt-half API ───────────────────────────────────────────────────────

/// Called from the keyboard IRQ handler (interrupts.rs) with each raw scancode byte.
///
/// Initializes the queue on first call, then pushes the scancode.
/// If the queue is full, drops the scancode (acceptable — we prefer dropping to deadlock).
/// After pushing, wakes the keyboard task so the executor polls it.
///
/// MUST be callable from interrupt context:
///   - No heap allocation after first call (queue is pre-allocated)
///   - No locks (ArrayQueue is lock-free, AtomicWaker uses atomics)
pub(crate) fn add_scancode(scancode: u8) {
    // Initialize the queue if this is the first call.
    // `try_init_with` returns Err if already initialized — we just ignore that.
    if let Ok(queue) = SCANCODE_QUEUE.try_get() {
        if queue.push(scancode).is_err() {
            // Queue full — scancode dropped. Not a panic-worthy event.
            // In practice this only happens if the task is completely stuck.
        }
        // Wake the keyboard task so it polls for new scancodes.
        WAKER.wake();
    } else {
        // Queue not yet initialized (keyboard interrupt fired before task setup).
        // Scancode dropped — this is fine, the user hasn't even seen the boot message yet.
    }
}

// ─── ScancodeStream ───────────────────────────────────────────────────────────

/// An async stream that yields one `u8` scancode per keyboard event.
///
/// Implements `futures_util::stream::Stream` (the async version of `Iterator`).
/// The keyboard task uses `stream.next().await` to wait for each scancode.
pub struct ScancodeStream {
    /// Private field prevents external construction — forces use of `ScancodeStream::new()`.
    _private: (),
}

impl ScancodeStream {
    /// Create the stream and initialize the scancode queue.
    ///
    /// Must be called exactly once. The `OnceCell` will panic on a second initialization.
    pub fn new() -> Self {
        // Initialize the queue here, in task context, where heap allocation is safe.
        SCANCODE_QUEUE
            .try_init_once(|| ArrayQueue::new(100))
            .expect("ScancodeStream::new() should only be called once");

        ScancodeStream { _private: () }
    }
}

impl Stream for ScancodeStream {
    type Item = u8;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<u8>> {
        let queue = SCANCODE_QUEUE
            .try_get()
            .expect("scancode queue not initialized");

        // Optimization: fast path — try to pop without registering the waker.
        // If a scancode is available immediately, we avoid the overhead of waker registration.
        if let Some(scancode) = queue.pop() {
            return Poll::Ready(Some(scancode));
        }

        // Queue is empty — register our Waker so `add_scancode` can wake us.
        // `register` is safe to call multiple times: it replaces the old waker.
        WAKER.register(cx.waker());

        // Try one more time AFTER registering the waker to close the race window:
        //
        //   Thread A (task): checks queue → empty
        //   Thread B (IRQ):  pushes scancode → calls WAKER.wake() → nothing registered yet
        //   Thread A (task): registers waker  (wakeup missed!)
        //
        // By checking again after registering, we catch scancodes that arrived
        // in the window between our first check and waker registration.
        match queue.pop() {
            Some(scancode) => Poll::Ready(Some(scancode)),
            None => Poll::Pending,
        }
    }
}

// ─── Keyboard task ────────────────────────────────────────────────────────────

/// Async task: decode scancodes and print characters to the framebuffer.
///
/// Spawned by `kernel_main` via the executor. Runs forever.
///
/// Each `.await` on `scancodes.next()` yields to the executor if the queue
/// is empty, letting other tasks (e.g., the timer, future UI tasks) run.
pub async fn print_keypresses() {
    let mut scancodes = ScancodeStream::new();

    // PS/2 keyboard decoder: converts raw scan set 1 scancodes to key events.
    // `layouts::Us104Key`: US QWERTY 104-key layout.
    // `HandleControl::Ignore`: don't convert Ctrl+[A-Z] to ASCII control codes —
    //   let the user handle Ctrl combinations at a higher level.
    let mut keyboard: Keyboard<layouts::Us104Key, ScancodeSet1> =
        Keyboard::new(ScancodeSet1::new(), layouts::Us104Key, HandleControl::Ignore);

    // `StreamExt::next()` provides the `.await`-able interface over our Stream.
    while let Some(scancode) = scancodes.next().await {
        // Feed the raw scancode to the decoder state machine.
        // `add_byte()` handles multi-byte extended key sequences (e.g., arrow keys
        // send 0xE0 followed by a direction byte — the decoder buffers the 0xE0).
        if let Ok(Some(key_event)) = keyboard.add_byte(scancode) {
            // A complete key event decoded (press or release).
            // `process_keyevent()` maps the key event to a `DecodedKey`:
            //   - `Unicode(char)`: a printable character
            //   - `RawKey(key)`: a non-printable key (F1, arrow, Ctrl, etc.)
            if let Some(key) = keyboard.process_keyevent(key_event) {
                match key {
                    DecodedKey::Unicode(character) => print!("{}", character),
                    DecodedKey::RawKey(key) => print!("{:?}", key),
                }
            }
        }
    }
}
