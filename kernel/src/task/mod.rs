// kernel/src/task/mod.rs
//
// Cooperative async task system — Phase 9 (types/traits).
//
// ─── HOW RUST'S ASYNC WORKS WITHOUT AN OS ────────────────────────────────────
//
// Rust `async fn` compiles to a state machine that implements `Future`:
//
//   trait Future {
//       type Output;
//       fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
//   }
//
// `poll()` is called by an EXECUTOR. If the future is ready (result available),
// it returns `Poll::Ready(value)`. If not (e.g., waiting for a scancode), it
// returns `Poll::Pending` and stores a `Waker` from the `Context` — when
// something changes (interrupt fires, scancode arrives), the waker is called,
// and the executor knows to poll this future again.
//
// This is "cooperative" multitasking: tasks voluntarily yield (return Pending)
// rather than being forcibly preempted by a timer. No thread stack switching
// needed — each task IS a tiny state machine. Perfect for a kernel without
// scheduler infrastructure.
//
// ─── PIN<BOX<DYN FUTURE>> ────────────────────────────────────────────────────
//
// Futures must be `Pin`ned in memory once polled — the state machine can hold
// self-referential pointers (e.g., a reference to a local variable across an
// `await` point). Moving the future after pinning would invalidate those pointers.
//
// `Box::pin(future)` boxes the future on the heap and returns a `Pin<Box<...>>`,
// guaranteeing the address won't move.

use alloc::boxed::Box;
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll},
};

pub mod executor;
pub mod keyboard;

// ─── TaskId ───────────────────────────────────────────────────────────────────

/// A unique identifier for a kernel task.
///
/// Generated from a global atomic counter — each new task gets a unique ID.
/// Used by the executor to identify which tasks need to be polled next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskId(u64);

impl TaskId {
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        // `fetch_add` is atomic: safe to call from multiple contexts simultaneously.
        TaskId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

// ─── Task ─────────────────────────────────────────────────────────────────────

/// A kernel async task: a boxed, pinned, heap-allocated future.
///
/// `Output = ()` because kernel tasks run forever (or until they complete their work
/// silently). We don't need a return value mechanism.
///
/// `Send` is required so tasks can be placed in the executor's queue
/// (even though we're single-threaded for now, the trait bound future-proofs this).
pub struct Task {
    pub id: TaskId,
    future: Pin<Box<dyn Future<Output = ()>>>,
}

impl Task {
    /// Create a new task from any `async fn` or `async {}` block.
    ///
    /// The future is immediately boxed and pinned on the kernel heap
    /// (requires the heap to be initialized before spawning tasks).
    pub fn new(future: impl Future<Output = ()> + 'static) -> Task {
        Task {
            id: TaskId::new(),
            future: Box::pin(future),
        }
    }

    /// Poll the task's future once.
    ///
    /// Called by the executor. If `Poll::Pending` is returned, the task is
    /// parked until its `Waker` is called. If `Poll::Ready(())` is returned,
    /// the task is complete and can be dropped.
    pub(crate) fn poll(&mut self, context: &mut Context) -> Poll<()> {
        self.future.as_mut().poll(context)
    }
}
