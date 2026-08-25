// kernel/src/task/executor.rs
//
// Async executor — Phase 9.
//
// ─── WHAT AN EXECUTOR DOES ───────────────────────────────────────────────────
//
// The executor is the runtime that drives futures to completion by calling `poll()`.
// Rust provides the Future trait and async/await syntax, but NO built-in runtime —
// you must bring your own executor.
//
// Our executor:
//   1. Maintains a map of TaskId → Task (the live tasks)
//   2. Maintains a wake queue (ArrayQueue<TaskId>) of tasks that need polling
//   3. On each iteration: dequeue a TaskId, look up the task, poll it
//   4. If Poll::Pending: task goes back to sleep, waiting for its Waker to fire
//   5. If Poll::Ready: task is done, drop it
//
// ─── WAKER MECHANISM ─────────────────────────────────────────────────────────
//
// When a task returns Poll::Pending, it registers a `Waker` from the `Context`.
// The Waker is a tiny callback: when called (waker.wake()), it pushes the TaskId
// back onto the wake queue, causing the executor to poll that task again.
//
// Who calls the waker? Interrupt handlers.
// When the keyboard interrupt fires and pushes a scancode to the queue, it calls
// `waker.wake()` → the keyboard task's TaskId enters the wake queue → executor
// polls the keyboard task → task reads the scancode and processes it.
//
// This is the "reactor-executor" pattern: the reactor (interrupt handler) notifies
// the executor (waker) when I/O is ready.
//
// ─── HLT-BASED IDLE LOOP ─────────────────────────────────────────────────────
//
// When the wake queue is empty, there's nothing to do. Instead of spinning
// (which wastes CPU cycles and power), we:
//   1. Enable interrupts (so interrupt handlers can fire and fill the wake queue)
//   2. Execute `hlt` (halt until next interrupt)
//   3. After the interrupt handler runs (and possibly pushes a TaskId), resume
//
// This gives us a power-efficient idle loop that's essentially:
//   "sleep until something interesting happens, then process it"

use super::{Task, TaskId};
use alloc::{collections::BTreeMap, sync::Arc, task::Wake};
use core::task::{Context, Poll, Waker};
use crossbeam_queue::ArrayQueue;

// ─── Executor ─────────────────────────────────────────────────────────────────

pub struct Executor {
    /// All live tasks, keyed by TaskId.
    tasks: BTreeMap<TaskId, Task>,

    /// Queue of tasks that need to be polled (their Waker was called).
    /// `Arc` so interrupt handlers can hold a reference to it and push TaskIds.
    task_queue: Arc<ArrayQueue<TaskId>>,

    /// Cached Wakers — creating a Waker involves an Arc allocation, so we cache
    /// one per task and reuse it on subsequent poll cycles.
    waker_cache: BTreeMap<TaskId, Waker>,
}

impl Executor {
    pub fn new() -> Self {
        Executor {
            tasks: BTreeMap::new(),
            task_queue: Arc::new(ArrayQueue::new(100)),
            waker_cache: BTreeMap::new(),
        }
    }

    /// Add a task to the executor and immediately schedule it for polling.
    pub fn spawn(&mut self, task: Task) {
        let task_id = task.id;
        // Store the task.
        if self.tasks.insert(task_id, task).is_some() {
            panic!("task with same ID already exists");
        }
        // Push immediately to the wake queue — every new task gets one poll on startup
        // to let it run to its first `await` point.
        self.task_queue
            .push(task_id)
            .expect("task queue full");
    }

    /// Main executor loop — runs forever (or until all tasks complete).
    ///
    /// Processes tasks from the wake queue, halts when idle.
    pub fn run(&mut self) -> ! {
        loop {
            self.run_ready_tasks();
            self.sleep_if_idle();
        }
    }

    /// Poll all tasks currently in the wake queue.
    fn run_ready_tasks(&mut self) {
        // Destructure to allow borrowing both `tasks` and `waker_cache` simultaneously
        // without the borrow checker complaining about self being borrowed twice.
        let Self {
            tasks,
            task_queue,
            waker_cache,
        } = self;

        while let Some(task_id) = task_queue.pop() {
            let task = match tasks.get_mut(&task_id) {
                Some(task) => task,
                None => continue, // task was completed and removed — stale wake, skip
            };

            // Get or create a Waker for this task.
            let waker = waker_cache
                .entry(task_id)
                .or_insert_with(|| TaskWaker::new(task_id, task_queue.clone()));

            let mut context = Context::from_waker(waker);

            match task.poll(&mut context) {
                Poll::Ready(()) => {
                    // Task completed. Remove it and its cached waker.
                    tasks.remove(&task_id);
                    waker_cache.remove(&task_id);
                }
                Poll::Pending => {
                    // Task is waiting. Leave it in `tasks` — it will be re-enqueued
                    // by the Waker when its interrupt fires.
                }
            }
        }
    }

    /// If there's nothing to do, halt the CPU until the next interrupt.
    ///
    /// We must:
    ///   1. Check the queue is STILL empty AFTER enabling interrupts.
    ///      (Race: an interrupt could have fired between our last check and `hlt`.)
    ///   2. If still empty, `hlt` — the CPU wakes on the next interrupt.
    ///   3. The interrupt handler pushes to the queue, so after `hlt` returns
    ///      we go back to `run_ready_tasks`.
    fn sleep_if_idle(&self) {
        use x86_64::instructions::interrupts;

        // Atomically check + halt: disable interrupts, check queue, then `hlt`.
        // `hlt` re-enables interrupts atomically as part of its microcode —
        // so no interrupt is missed between the check and the halt.
        interrupts::disable();
        if self.task_queue.is_empty() {
            interrupts::enable_and_hlt(); // atomic: enables interrupts + halts
        } else {
            interrupts::enable(); // queue has items, don't halt — go process them
        }
    }
}

// ─── TaskWaker ────────────────────────────────────────────────────────────────

/// A `Waker` implementation that pushes a TaskId onto the executor's wake queue.
///
/// When `waker.wake()` is called (from an interrupt handler or another task),
/// this pushes `task_id` onto `task_queue`, causing the executor to poll the
/// associated task on its next iteration.
struct TaskWaker {
    task_id: TaskId,
    task_queue: Arc<ArrayQueue<TaskId>>,
}

impl TaskWaker {
    fn new(task_id: TaskId, task_queue: Arc<ArrayQueue<TaskId>>) -> Waker {
        // Wrap in Arc so the Waker can be cloned cheaply (wake() can be called
        // multiple times by multiple sources).
        Waker::from(Arc::new(TaskWaker {
            task_id,
            task_queue,
        }))
    }

    /// Push this task back onto the executor's ready queue.
    fn wake_task(&self) {
        self.task_queue
            .push(self.task_id)
            .expect("task wake queue full");
    }
}

impl Wake for TaskWaker {
    /// Called when the Waker is consumed (ownership transferred to wake).
    fn wake(self: Arc<Self>) {
        self.wake_task();
    }

    /// Called when the Waker is not consumed (caller keeps a reference).
    /// More efficient than `wake()` — avoids dropping the Arc.
    fn wake_by_ref(self: &Arc<Self>) {
        self.wake_task();
    }
}
