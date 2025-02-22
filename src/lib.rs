//! Async executors.
//!
//! # Examples
//!
//! ```
//! use async_executor::Executor;
//! use futures_lite::future;
//!
//! // Create a new executor.
//! let ex = Executor::new();
//!
//! // Spawn a task.
//! let task = ex.spawn(async {
//!     println!("Hello world");
//! });
//!
//! // Run the executor until the task completes.
//! future::block_on(ex.run(task));
//! ```

#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

mod taskqueue;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::{
    cell::Cell,
    panic::{RefUnwindSafe, UnwindSafe},
};
use std::{cell::RefCell, future::Future};

use async_task::Runnable;

use cache_padded::CachePadded;
use futures_lite::{future, prelude::*};
use parking_lot::{Mutex, RwLock};
use slab::Slab;
use taskqueue::{GlobalQueue, LocalQueue, LocalQueueHandle};

#[doc(no_inline)]
pub use async_task::Task;

/// An async executor.
///
/// # Examples
///
/// A multi-threaded executor:
///
/// ```
/// use async_channel::unbounded;
/// use async_executor::Executor;
/// use easy_parallel::Parallel;
/// use futures_lite::future;
///
/// let ex = Executor::new();
/// let (signal, shutdown) = unbounded::<()>();
///
/// Parallel::new()
///     // Run four executor threads.
///     .each(0..4, |_| future::block_on(ex.run(shutdown.recv())))
///     // Run the main future on the current thread.
///     .finish(|| future::block_on(async {
///         println!("Hello world!");
///         drop(signal);
///     }));
/// ```
#[derive(Debug)]
pub struct Executor<'a> {
    /// The executor state.
    state: once_cell::sync::OnceCell<Arc<State>>,

    /// Makes the `'a` lifetime invariant.
    _marker: PhantomData<std::cell::UnsafeCell<&'a ()>>,
}

unsafe impl Send for Executor<'_> {}
unsafe impl Sync for Executor<'_> {}

impl UnwindSafe for Executor<'_> {}
impl RefUnwindSafe for Executor<'_> {}

impl<'a> Executor<'a> {
    /// Creates a new executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    /// ```
    pub const fn new() -> Executor<'a> {
        Executor {
            state: once_cell::sync::OnceCell::new(),
            _marker: PhantomData,
        }
    }

    /// Returns `true` if there are no unfinished tasks.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    /// assert!(ex.is_empty());
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(!ex.is_empty());
    ///
    /// assert!(ex.try_tick());
    /// assert!(ex.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.state().active.lock().is_empty()
    }

    /// Spawns a task onto the executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// ```
    pub fn spawn<T: Send + 'a>(&self, future: impl Future<Output = T> + Send + 'a) -> Task<T> {
        let mut active = self.state().active.lock();

        // Remove the task from the set of active tasks when the future finishes.
        let index = active.vacant_entry().key();
        let state = self.state().clone();
        let future = async move {
            let _guard = CallOnDrop(move || {
                // TODO: use try_remove once https://github.com/tokio-rs/slab/pull/89 merged
                let mut active = state.active.lock();
                if active.contains(index) {
                    drop(active.remove(index));
                }
            });
            future.await
        };

        // Create the task and register it in the set of active tasks.
        let (runnable, task) = unsafe { async_task::spawn_unchecked(future, self.schedule()) };
        active.insert(runnable.waker());

        runnable.schedule();
        task
    }

    /// Attempts to run a task if at least one is scheduled.
    ///
    /// Running a scheduled task means simply polling its future once.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    /// assert!(!ex.try_tick()); // no tasks to run
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(ex.try_tick()); // a task was found
    /// ```
    pub fn try_tick(&self) -> bool {
        match self.state().queue.pop() {
            None => false,
            Some(runnable) => {
                // Notify another ticker now to pick up where this ticker left off, just in case
                // running the task takes a long time.
                self.state().notify();

                // Run the task.
                runnable.run();
                true
            }
        }
    }

    /// Runs a single task.
    ///
    /// Running a task means simply polling its future once.
    ///
    /// If no tasks are scheduled when this method is called, it will wait until one is scheduled.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    /// use futures_lite::future;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// future::block_on(ex.tick()); // runs the task
    /// ```
    pub async fn tick(&self) {
        let state = self.state().clone();
        let runnable = Ticker::new(state).runnable().await;
        runnable.run();
    }

    /// Runs the executor until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    /// use futures_lite::future;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async { 1 + 2 });
    /// let res = future::block_on(ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        let mut runner = Runner::new(self.state().clone());
        runner.set_tls_active();
        let _guard = CallOnDrop(clear_tls);
        // A future that runs tasks forever.
        let run_forever = async {
            loop {
                for _ in 0..200 {
                    let runnable = runner.runnable().await;
                    let yielded = runnable.run();
                    JUST_YIELDED.with(|v| v.replace(yielded));
                }
                future::yield_now().await;
            }
        };

        // Run `future` and `run_forever` concurrently until `future` completes.
        future.or(run_forever).await
    }

    /// Returns a function that schedules a runnable task when it gets woken up.
    fn schedule(&self) -> impl Fn(Runnable) + Send + Sync + 'static {
        let state = self.state().clone();

        // Try to push to the local queue. If it doesn't work, push to the global queue.
        move |runnable| {
            if let Err(runnable) = try_push_tls(&state, runnable) {
                state.queue.push(runnable);
                state.notify();
            }
        }
    }

    /// Returns a reference to the inner state.
    fn state(&self) -> &Arc<State> {
        self.state.get_or_init(|| Arc::new(State::new()))
    }
}

thread_local! {
    static JUST_YIELDED: Cell<bool> = Cell::new(false);
}

impl Drop for Executor<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.state.get() {
            let mut active = state.active.lock();
            for w in active.drain() {
                w.wake();
            }
            drop(active);

            while state.queue.pop().is_some() {}
        }
    }
}

impl<'a> Default for Executor<'a> {
    fn default() -> Executor<'a> {
        Executor::new()
    }
}

/// A thread-local executor.
///
/// The executor can only be run on the thread that created it.
///
/// # Examples
///
/// ```
/// use async_executor::LocalExecutor;
/// use futures_lite::future;
///
/// let local_ex = LocalExecutor::new();
///
/// future::block_on(local_ex.run(async {
///     println!("Hello world!");
/// }));
/// ```
#[derive(Debug)]
pub struct LocalExecutor<'a> {
    /// The inner executor.
    inner: once_cell::unsync::OnceCell<Executor<'a>>,

    /// Makes the type `!Send` and `!Sync`.
    _marker: PhantomData<Rc<()>>,
}

impl UnwindSafe for LocalExecutor<'_> {}
impl RefUnwindSafe for LocalExecutor<'_> {}

impl<'a> LocalExecutor<'a> {
    /// Creates a single-threaded executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    ///
    /// let local_ex = LocalExecutor::new();
    /// ```
    pub const fn new() -> LocalExecutor<'a> {
        LocalExecutor {
            inner: once_cell::unsync::OnceCell::new(),
            _marker: PhantomData,
        }
    }

    /// Returns `true` if there are no unfinished tasks.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    ///
    /// let local_ex = LocalExecutor::new();
    /// assert!(local_ex.is_empty());
    ///
    /// let task = local_ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(!local_ex.is_empty());
    ///
    /// assert!(local_ex.try_tick());
    /// assert!(local_ex.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.inner().is_empty()
    }

    /// Spawns a task onto the executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    ///
    /// let local_ex = LocalExecutor::new();
    ///
    /// let task = local_ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// ```
    pub fn spawn<T: 'a>(&self, future: impl Future<Output = T> + 'a) -> Task<T> {
        let mut active = self.inner().state().active.lock();

        // Remove the task from the set of active tasks when the future finishes.
        let index = active.vacant_entry().key();
        let state = self.inner().state().clone();
        let future = async move {
            let _guard = CallOnDrop(move || drop(state.active.lock().remove(index)));
            future.await
        };

        // Create the task and register it in the set of active tasks.
        let (runnable, task) = unsafe { async_task::spawn_unchecked(future, self.schedule()) };
        active.insert(runnable.waker());

        runnable.schedule();
        task
    }

    /// Attempts to run a task if at least one is scheduled.
    ///
    /// Running a scheduled task means simply polling its future once.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    ///
    /// let ex = LocalExecutor::new();
    /// assert!(!ex.try_tick()); // no tasks to run
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(ex.try_tick()); // a task was found
    /// ```
    pub fn try_tick(&self) -> bool {
        self.inner().try_tick()
    }

    /// Runs a single task.
    ///
    /// Running a task means simply polling its future once.
    ///
    /// If no tasks are scheduled when this method is called, it will wait until one is scheduled.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    /// use futures_lite::future;
    ///
    /// let ex = LocalExecutor::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// future::block_on(ex.tick()); // runs the task
    /// ```
    pub async fn tick(&self) {
        self.inner().tick().await
    }

    /// Runs the executor until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    /// use futures_lite::future;
    ///
    /// let local_ex = LocalExecutor::new();
    ///
    /// let task = local_ex.spawn(async { 1 + 2 });
    /// let res = future::block_on(local_ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        self.inner().run(future).await
    }

    /// Returns a function that schedules a runnable task when it gets woken up.
    fn schedule(&self) -> impl Fn(Runnable) + Send + Sync + 'static {
        let state = self.inner().state().clone();
        move |runnable| {
            state.queue.push(runnable);
            state.notify();
        }
    }

    /// Returns a reference to the inner executor.
    fn inner(&self) -> &Executor<'a> {
        self.inner.get_or_init(Executor::new)
    }
}

impl<'a> Default for LocalExecutor<'a> {
    fn default() -> LocalExecutor<'a> {
        LocalExecutor::new()
    }
}

/// The state of a executor.
#[derive(Debug)]
struct State {
    /// The global queue.
    queue: CachePadded<GlobalQueue>,

    /// Count of searching runners.
    searching_count: CachePadded<AtomicUsize>,

    /// Local queues created by runners.
    local_queues: CachePadded<RwLock<Slab<LocalQueueHandle>>>,

    /// Set to `true` when a sleeping ticker is notified or no tickers are sleeping.
    notified: CachePadded<AtomicBool>,

    /// A list of sleeping tickers.
    sleepers: CachePadded<Mutex<Sleepers>>,

    /// Currently active tasks.
    active: CachePadded<Mutex<Slab<Waker>>>,
}

impl State {
    /// Creates state for a new executor.
    fn new() -> State {
        State {
            queue: GlobalQueue::default().into(),
            searching_count: AtomicUsize::new(0).into(),
            local_queues: RwLock::new(Slab::new()).into(),
            notified: AtomicBool::new(true).into(),
            sleepers: parking_lot::Mutex::new(Sleepers {
                count: 0,
                wakers: Vec::new(),
                free_ids: Vec::new(),
            })
            .into(),
            active: Mutex::new(Slab::new()).into(),
        }
    }

    /// Notifies a sleeping ticker.
    #[inline]
    fn notify(&self) {
        if self
            .notified
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let waker = self.sleepers.lock().notify();
            if let Some(w) = waker {
                w.wake();
            }
        }
    }
}

/// A list of sleeping tickers.
#[derive(Debug)]
struct Sleepers {
    /// Number of sleeping tickers (both notified and unnotified).
    count: usize,

    /// IDs and wakers of sleeping unnotified tickers.
    ///
    /// A sleeping ticker is notified when its waker is missing from this list.
    wakers: Vec<(usize, Waker)>,

    /// Reclaimed IDs.
    free_ids: Vec<usize>,
}

impl Sleepers {
    /// Inserts a new sleeping ticker.
    fn insert(&mut self, waker: &Waker) -> usize {
        let id = match self.free_ids.pop() {
            Some(id) => id,
            None => self.count + 1,
        };
        self.count += 1;
        self.wakers.push((id, waker.clone()));
        id
    }

    /// Re-inserts a sleeping ticker's waker if it was notified.
    ///
    /// Returns `true` if the ticker was notified.
    fn update(&mut self, id: usize, waker: &Waker) -> bool {
        for item in &mut self.wakers {
            if item.0 == id {
                if !item.1.will_wake(waker) {
                    item.1 = waker.clone();
                }
                return false;
            }
        }

        self.wakers.push((id, waker.clone()));
        true
    }

    /// Removes a previously inserted sleeping ticker.
    ///
    /// Returns `true` if the ticker was notified.
    fn remove(&mut self, id: usize) -> Option<Waker> {
        self.count -= 1;
        self.free_ids.push(id);

        for i in (0..self.wakers.len()).rev() {
            if self.wakers[i].0 == id {
                let (_, waker) = self.wakers.remove(i);
                return Some(waker);
            }
        }
        None
    }

    /// Returns `true` if a sleeping ticker is notified or no tickers are sleeping.
    fn is_notified(&self) -> bool {
        self.count == 0 || self.count > self.wakers.len()
    }

    /// Returns notification waker for a sleeping ticker.
    ///
    /// If a ticker was notified already or there are no tickers, `None` will be returned.
    fn notify(&mut self) -> Option<Waker> {
        if self.wakers.len() == self.count {
            self.wakers.pop().map(|item| item.1)
        } else {
            None
        }
    }
}

/// Runs task one by one.
#[derive(Debug)]
struct Ticker {
    /// The executor state.
    state: Arc<State>,

    /// Set to a non-zero sleeper ID when in sleeping state.
    ///
    /// States a ticker can be in:
    /// 1) Woken.
    /// 2a) Sleeping and unnotified.
    /// 2b) Sleeping and notified.
    sleeping: AtomicUsize,
}

impl Ticker {
    /// Creates a ticker.
    fn new(state: Arc<State>) -> Ticker {
        Ticker {
            state,
            sleeping: AtomicUsize::new(0),
        }
    }

    /// Moves the ticker into sleeping and unnotified state.
    ///
    /// Returns `false` if the ticker was already sleeping and unnotified.
    fn sleep(&self, waker: &Waker) -> bool {
        let mut sleepers = self.state.sleepers.lock();

        match self.sleeping.load(Ordering::SeqCst) {
            // Move to sleeping state.
            0 => self
                .sleeping
                .store(sleepers.insert(waker), Ordering::SeqCst),

            // Already sleeping, check if notified.
            id => {
                if !sleepers.update(id, waker) {
                    return false;
                }
            }
        }

        self.state
            .notified
            .swap(sleepers.is_notified(), Ordering::SeqCst);

        true
    }

    /// Moves the ticker into woken state.
    fn wake(&self) -> Option<Waker> {
        let id = self.sleeping.swap(0, Ordering::SeqCst);
        if id != 0 {
            let mut sleepers = self.state.sleepers.lock();
            let toret = sleepers.remove(id);

            self.state
                .notified
                .swap(sleepers.is_notified(), Ordering::SeqCst);
            toret
        } else {
            None
        }
    }

    /// Waits for the next runnable task to run.
    async fn runnable(&self) -> Runnable {
        self.runnable_with(|| self.state.queue.pop()).await
    }

    /// Waits for the next runnable task to run, given a function that searches for a task.
    async fn runnable_with(&self, mut search: impl FnMut() -> Option<Runnable>) -> Runnable {
        future::poll_fn(|cx| {
            loop {
                // This kills performance somehow
                // DEBUG_SEARCHING_COUNT.fetch_add(1, Ordering::Relaxed);
                let res = search();
                // DEBUG_SEARCHING_COUNT.fetch_sub(1, Ordering::Relaxed);

                match res {
                    None => {
                        // Move to sleeping and unnotified state.
                        if !self.sleep(cx.waker()) {
                            // If already sleeping and unnotified, return.
                            return Poll::Pending;
                        }
                    }
                    Some(r) => {
                        // Wake up.
                        self.wake();
                        // Sibling notification.
                        if self.state.searching_count.load(Ordering::Relaxed) == 0
                        // && fastrand::u8(0..) < 5
                        {
                            self.state.notify();
                        }
                        return Poll::Ready(r);
                    }
                }
            }
        })
        .await
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        // If this ticker is in sleeping state, it must be removed from the sleepers list.
        let id = self.sleeping.swap(0, Ordering::SeqCst);
        if id != 0 {
            let mut sleepers = self.state.sleepers.lock();
            let notified = sleepers.remove(id).is_none();

            self.state
                .notified
                .swap(sleepers.is_notified(), Ordering::SeqCst);

            // If this ticker was notified, then notify another ticker.
            if notified {
                drop(sleepers);
                // eprintln!("TICKER DROP");
                self.state.notify();
            }
        }
    }
}

struct TlsData {
    state: Arc<State>,
    ticker: Arc<Ticker>,
    pending_tasks: Vec<Runnable>,
}

impl Drop for TlsData {
    fn drop(&mut self) {
        // move the pending tasks into the state
        for task in self.pending_tasks.drain(0..) {
            self.state.queue.push(task)
        }
    }
}

thread_local! {
    static TLS: RefCell<Option<TlsData>> = Default::default()
}

fn clear_tls() {
    TLS.with(|v| *v.borrow_mut() = Default::default())
}

fn try_push_tls(state: &Arc<State>, runnable: Runnable) -> Result<(), Runnable> {
    TLS.with(|tls| {
        let tls = tls.try_borrow_mut();
        if let Ok(mut tls) = tls {
            if let Some(tlsdata) = tls.as_mut() {
                if !Arc::ptr_eq(state, &tlsdata.state) {
                    return Err(runnable);
                }
                tlsdata.pending_tasks.push(runnable);
                // notify ticker
                // eprintln!("successfully pushed locally");
                if let Some(v) = tlsdata.ticker.wake() {
                    v.wake()
                }
                Ok(())
            } else {
                Err(runnable)
            }
        } else {
            Err(runnable)
        }
    })
}

fn try_pop_tls() -> Option<Vec<Runnable>> {
    TLS.with(|tls| {
        let mut tls = tls.borrow_mut();
        if let Some(tlsdata) = tls.as_mut() {
            Some(std::mem::replace(
                &mut tlsdata.pending_tasks,
                Vec::with_capacity(4),
            ))
        } else {
            None
        }
    })
}

/// A worker in a work-stealing executor.
///
/// This is just a ticker that also has an associated local queue for improved cache locality.
#[derive(Debug)]
struct Runner {
    /// The executor state.
    state: Arc<State>,

    /// Inner ticker.
    ticker: Arc<Ticker>,

    /// The local queue.
    local: LocalQueue,

    /// Bumped every time a runnable task is found.
    ticks: usize,

    /// ID.
    id: usize,
}

impl Runner {
    /// Creates a runner and registers it in the executor state.
    fn new(state: Arc<State>) -> Runner {
        let mut runner = Runner {
            state: state.clone(),
            ticker: Arc::new(Ticker::new(state.clone())),
            local: LocalQueue::default(),
            ticks: 0,
            id: 0,
        };
        runner.id = state.local_queues.write().insert(runner.local.handle());
        runner
    }

    /// Sets as active in the TLS
    fn set_tls_active(&self) {
        // let weak_ticker = Arc::downgrade(&self.ticker);
        // let weak_local = Arc::downgrade(&self.local);
        TLS.with(|tls| {
            let mut tls = tls.borrow_mut();
            if tls.is_none() {
                *tls = Some(TlsData {
                    state: self.state.clone(),
                    ticker: self.ticker.clone(),
                    pending_tasks: Vec::new(),
                })
            }
        })
    }

    /// Waits for the next runnable task to run.
    async fn runnable(&mut self) -> Runnable {
        // static USELESS_WAKEUP_COUNT: AtomicUsize = AtomicUsize::new(0);
        // static GOOD_WAKEUP_COUNT: AtomicUsize = AtomicUsize::new(0);

        let runnable = self
            .ticker
            .clone()
            .runnable_with(|| {
                let must_yield = JUST_YIELDED.with(|v| v.replace(false));
                // Try the TLS.
                if let Some(r) = try_pop_tls() {
                    for task in r {
                        // SAFETY: only one thread can push to self.local at the same time
                        if let Err(task) = self.local.push(must_yield, task) {
                            self.state.queue.push(task);
                        }
                    }
                }

                // Try the local queue.
                if let Some(r) = self.local.pop() {
                    return Some(r);
                }

                self.state.searching_count.fetch_add(1, Ordering::Relaxed);
                // Try stealing from the global queue.
                self.local.steal_global(&self.state.queue);
                if let Some(r) = self.local.pop() {
                    self.state.searching_count.fetch_sub(1, Ordering::Relaxed);
                    return Some(r);
                }

                // Try stealing from other runners.
                let local_queues = self.state.local_queues.read();

                // Pick a random starting point in the iterator list and rotate the list.
                let n = local_queues.len();
                let start = fastrand::usize(..n);
                let iter = local_queues
                    .iter()
                    .chain(local_queues.iter())
                    .skip(start)
                    .take(n);

                // Remove this runner's local queue.
                let id = self.id;
                let iter = iter.filter(|local| local.0 != id);

                // Try stealing from each local queue in the list.
                for (_, local) in iter {
                    self.local.steal_local(local);
                    if let Some(r) = self.local.pop() {
                        self.state.searching_count.fetch_sub(1, Ordering::Relaxed);
                        return Some(r);
                    }
                }

                self.state.searching_count.fetch_sub(1, Ordering::Relaxed);
                None
            })
            .await;

        // Bump the tick counter.
        self.ticks += 1;

        if self.ticks % 64 == 0 {
            // Steal tasks from the global queue to ensure fair task scheduling.
            self.local.steal_global(&self.state.queue)
        }

        runnable
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        // Remove the local queue.
        self.state.local_queues.write().remove(self.id);

        // Re-schedule remaining tasks in the local queue.
        // SAFETY: this cannot possibly be run from two different threads concurrently.
        while let Some(r) = self.local.pop() {
            r.schedule();
        }
    }
}
/// Runs a closure when dropped.
struct CallOnDrop<F: Fn()>(F);

impl<F: Fn()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
