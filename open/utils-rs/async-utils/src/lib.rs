// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

//! Async utilities

use std::error::Error as StdError;
use std::fmt;
use std::panic::{self, PanicInfo};
use std::pin::Pin;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::prelude::*;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::{JoinError, JoinHandle};
use tokio::{signal, time};

/// This registers a customized panic hook with the stdlib.
/// The customized panic hook does the same thing as the default
/// panic handling - ie. it prints out the panic information
/// and optionally a trace - but then it calls abort().
///
/// This means that a panic in Tokio threadpool worker thread
/// will bring down the whole program as if the panic
/// occured on the main thread.
///
/// This function can be called any number of times,
/// but the hook will be set only on the first call.
/// This is thread-safe.
pub fn setup_panic_handling() {
    static HOOK_SETTER: Once = Once::new();

    HOOK_SETTER.call_once(|| {
        let default_hook = panic::take_hook();

        let our_hook = move |pi: &PanicInfo| {
            default_hook(pi);
            process::abort();
        };

        panic::set_hook(Box::new(our_hook));
    });
}

/// An extension trait for `Future` goodies,
/// currently this only entails the `timeout()` function.
pub trait FutureExt: Future {
    /// Require a `Future` to complete before the specified duration has elapsed.
    ///
    /// This is a chainable alias for `tokio::time::timeout()`.
    fn timeout(self, timeout: Duration) -> time::Timeout<Self>
    where
        Self: Sized,
    {
        time::timeout(timeout, self)
    }
}

impl<F: Future> FutureExt for F {}

/// Internal, used to signal termination via `trigger`
/// and notify `Tasks` when that happens.
#[derive(Debug)]
struct Halt {
    trigger: Trigger,
    notify_join: Arc<Notify>,
}

/// Internal, used in the `Tasks` channel,
/// contains either a join handle of a task
/// that was spawned or a ready notification which
/// indicates to the `join()` function that all necessary tasks
/// were spawned.
///
/// `spawn()` uses this to send a spawned task's handle,
/// `ready()` to send a Ready notification.
#[derive(Debug)]
enum TaskMsg {
    Task(JoinHandle<()>),
    Ready,
}

/// Internal, used in `HaltHandle::join()`
/// to wait on signal from `halt()`
/// and then collect halting tasks' join handles.
#[derive(Debug)]
struct Tasks {
    tasks_rx: mpsc::UnboundedReceiver<TaskMsg>,
    notify_join: Arc<Notify>,
}

/// Error type returned by `HaltHandle::join()`.
#[derive(Debug)]
pub enum HaltError {
    /// Tasks didn't finish inside the timeout passed to `join()`.
    Timeout,
    /// One of the tasks panicked.
    Join(JoinError),
}

impl HaltError {
    fn map<'a, T, F: FnOnce(&'a JoinError) -> Option<T>>(&'a self, f: F) -> Option<T> {
        match self {
            HaltError::Timeout => None,
            HaltError::Join(err) => f(err),
        }
    }
}

impl fmt::Display for HaltError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HaltError::Timeout => write!(fmt, "Timeout"),
            HaltError::Join(err) => write!(fmt, "Join error: {}", err),
        }
    }
}

impl StdError for HaltError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.map(JoinError::source)
    }

    #[allow(deprecated)]
    fn cause(&self) -> Option<&dyn StdError> {
        self.map(JoinError::cause)
    }
}

/// A synchronization end that can be used to
/// cancel tasks which use the associated `Tripwire` instance.
///
/// NB. This is really just a thin wrapper around `watch::Sender`.
#[derive(Debug)]
pub struct Trigger(watch::Sender<()>);

impl Trigger {
    pub fn cancel(self) {
        let _ = self.0.broadcast(());
    }
}

/// A synchronization end that tasks can use to wait on
/// using eg. `take_until()` or `select!()` or similar
/// to await cancellation.
///
/// NB. This is really just a thin wrapper around `watch::Receiver`.
#[derive(Clone, Debug)]
pub struct Tripwire(watch::Receiver<()>);

impl Tripwire {
    pub fn new() -> (Trigger, Self) {
        let (trigger, tripwire) = watch::channel(());
        (Trigger(trigger), Self(tripwire))
    }
}

impl Future for Tripwire {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<()> {
        let mut this = self.as_mut();
        let mut next = this.0.next();
        Pin::new(&mut next).poll(ctx).map(|_| ())
    }
}

/// A handle with which tasks can be spawned and then halted.
///
/// # Usage
/// 1. Create a `HaltHandle` with `HaltHandle::new()` or `HaltHandle::arc()`
/// (use the latter if you want to share it between tasks or use `halt_on_ctrlc()`).
/// 2. Spawn any number of tasks using the `spawn()` method.
/// 3. When all relevant `spawn()` calls were made, use the `ready()` method
///    to tell the `HaltHandle` that all tasks were spawned.
/// 4. Use `halt()` to tell the spawned tasks that they should stop.
///    You can also use `halt_on_ctrlc()`, which will setup a
///    handler that calls `halt()` on `SIGINT`.
/// 5. Use `join()` to wait on the tasks to stop (a timeout may be used).
///
/// Note that `halt()` or `halt_on_ctrlc()` doesn't necessarily need to be called
/// after `ready()`. These can be called pretty much anytime and it won't cause
/// a race condition as long as `ready()` is called in the right moment.
#[derive(Debug)]
pub struct HaltHandle {
    /// Tripwire that is cloned into
    /// 'child' tasks when they are started with this handle.
    tripwire: Tripwire,
    /// Used to trigger the tripwire and then notifies `tasks`.
    halt: Mutex<Option<Halt>>,
    /// Spawned task handles as well as a ready notification are sent here, see `TaskMsg`
    tasks_tx: mpsc::UnboundedSender<TaskMsg>,
    /// Used to receive notification from `halt` and the task handles.
    tasks: Mutex<Option<Tasks>>,
    /// A flag whether we've already spawned a ctrlc tasks;
    /// this can only be done once.
    ctrlc_task_spawned: AtomicBool,
}

impl HaltHandle {
    /// Create a new `HaltHandle`
    pub fn new() -> Self {
        let (trigger, tripwire) = Tripwire::new();
        let notify_join = Arc::new(Notify::new());
        let (tasks_tx, tasks_rx) = mpsc::unbounded_channel();

        Self {
            tripwire,
            halt: Mutex::new(Some(Halt {
                trigger,
                notify_join: notify_join.clone(),
            })),
            tasks_tx,
            tasks: Mutex::new(Some(Tasks {
                tasks_rx,
                notify_join,
            })),
            ctrlc_task_spawned: AtomicBool::new(false),
        }
    }

    /// Create a `HaltHandle` and wrap it in `Arc` for sharing between tasks
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Spawn a new task. `f` is a function that takes
    /// a `Tripwire` and returns a `Future` to be spawned.
    /// `Tripwire` can be passed to `StreamExt::take_until`
    /// to make a stream stop generating items when
    /// `halt()` is called on the `HaltHandle`.
    pub fn spawn<FT, FN>(&self, f: FN)
    where
        FT: Future<Output = ()> + Send + 'static,
        FN: FnOnce(Tripwire) -> FT,
    {
        let ft = f(self.tripwire.clone());
        let task = tokio::spawn(ft);

        // Add the task join handle to tasks_tx (used by join()).
        // Errors are ignored here - send() on an unbounded channel
        // only fails if the receiver is dropped, and in that case
        // we don't care that the send() failed...
        let _ = self.tasks_tx.send(TaskMsg::Task(task));
    }

    /// Tells the handle that all tasks were spawned
    pub fn ready(&self) {
        // Send a Ready message. join() uses this to tell
        // that enough join handles were collected.
        // Error is ignored here for the same reason as in spawn().
        let _ = self.tasks_tx.send(TaskMsg::Ready);
    }

    /// Tell the handle to halt all the associated tasks.
    pub fn halt(&self) {
        if let Some(halt) = self
            .halt
            .lock()
            .expect("BUG: HaltHandle: Poisoned mutex")
            .take()
        {
            halt.trigger.cancel();
            halt.notify_join.notify();
        }
    }

    // TODO: Convert these to take self: &Arc<Self> once this is stabilized
    // cf. https://github.com/rust-lang/rust/issues/44874
    /// Tell the handle to call `halt()` in `Ctrl + C` / `SIGINT`.
    pub fn halt_on_ctrlc(self: Arc<Self>) {
        Self::handle_ctrlc(self, |this| async move { this.halt() });
    }

    /// Tell the handle to catch `Ctrl + C` / `SIGINT` and run
    /// the future generated by `f` when the signal is received.
    pub fn handle_ctrlc<FT, FN>(self: Arc<Self>, f: FN)
    where
        FT: Future + Send + 'static,
        FN: FnOnce(Arc<Self>) -> FT,
    {
        if !self
            .ctrlc_task_spawned
            .compare_and_swap(false, true, Ordering::SeqCst)
        {
            let ft = f(self);
            tokio::spawn(async move {
                signal::ctrl_c()
                    .await
                    .expect("BUG: Error listening for SIGINT");
                ft.await;
            });
        }
    }

    /// Wait for all associated tasks to finish once `ready()` and `halt()` are called.
    ///
    /// An optional `timeout` may be provided, this is the maximum time
    /// to wait **after** `halt()` has been called.
    ///
    /// Returns `Ok(())` when tasks are collected succesfully, or a `HaltError::Timeout`
    /// if tasks tasks didn't stop in time, or a `HaltError::Join` when a task panics.
    /// If multiple tasks panic, the first join error encountered is returned.
    ///
    /// # Panics
    /// `join()` panics if you call it multiple times. It must only be called once.
    pub async fn join(&self, timeout: Option<Duration>) -> Result<(), HaltError> {
        let mut tasks = self
            .tasks
            .lock()
            .expect("BUG: HaltHandle: Poisoned mutex")
            .take()
            .expect("BUG: HaltHandle: join() called multiple times");

        let _ = tasks.notify_join.notified().await;

        // Collect join handles. Join handles are added to the
        // tasks channel by Self::spawn(). After the user decides all
        // relevant tasks were added, they call ready().
        // ready() pushes a ready message, TaskMsg::Ready, to this channel.
        // Here we collect all the task join handles until we reach the ready message.
        let mut handles = vec![];
        while let Some(task_msg) = tasks.tasks_rx.next().await {
            match task_msg {
                TaskMsg::Task(handle) => handles.push(handle),
                TaskMsg::Ready => break,
            }
        }

        // Join all the spawned tasks, wait for them to finalize
        let ft = future::join_all(handles.drain(..));
        // If there's a timeout, only wait so much
        let mut res = if let Some(timeout) = timeout {
            match ft.timeout(timeout).await {
                Ok(res) => res,
                Err(_) => return Err(HaltError::Timeout),
            }
        } else {
            ft.await
        };

        // Map errors, return the first one encountered (if any)
        res.drain(..)
            .fold(Ok(()), Result::and)
            .map_err(|e| HaltError::Join(e))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::{stream, time};

    #[tokio::test]
    async fn test_timeout() {
        let timeout = Duration::from_millis(100);

        let future = future::pending::<()>().timeout(timeout);
        future.await.expect_err("BUG: Timeout expected");

        let mut stream = stream::pending::<()>();
        let future = stream.next().timeout(timeout);
        future.await.expect_err("BUG: Timeout expected");
    }

    /// Wait indefinitely on a stream with a `Tripwire` for cancellation.
    async fn forever_stream(tripwire: Tripwire) {
        let mut stream = stream::pending::<()>().take_until(tripwire);

        // The pending stream never actually yields a value,
        // ie. next() resolves to None only in the canelled case,
        // otherwise it doesn't return at all.
        stream.next().await;
    }

    // Basic functional test
    #[tokio::test]
    async fn test_halthandle_basic() {
        let handle = HaltHandle::new();

        // Spawn a couple of tasks on the handle
        for _ in 0..10 {
            handle.spawn(|tripwire| forever_stream(tripwire));
        }

        // Signal ready, halt, and join tasks
        handle.ready();
        handle.halt();
        handle.join(None).await.expect("BUG: join() failed");
    }

    // The same as basic test but with halting happening from within a task.
    // In this case the `HaltHandle` is shared in an `Arc`.
    #[tokio::test]
    async fn test_halthandle_shared() {
        let handle = HaltHandle::arc();

        // Spawn a couple of tasks on the handle
        for _ in 0..10 {
            handle.spawn(|tripwire| forever_stream(tripwire));
        }

        // Spawn a task that will halt()
        let handle2 = handle.clone();
        handle.spawn(|_| {
            async move {
                handle2.halt();
            }
        });

        // Join tasks
        handle.ready();
        handle.join(None).await.expect("BUG: join() failed");
    }

    // Test that spawn() / halt() / join() is not racy when ready()
    // is used appropriately.
    #[tokio::test(threaded_scheduler)]
    async fn test_halthandle_race() {
        const NUM_TASKS: usize = 10;

        let handle = HaltHandle::arc();
        let num_cancelled = Arc::new(AtomicUsize::new(0));

        // Signal halt right away, this should be fine
        handle.halt();

        // Spawn tasks in another task to allow a race
        {
            let handle = handle.clone();
            let num_cancelled = num_cancelled.clone();

            tokio::spawn(async move {
                // Delay a bit so that join() happens sooner than spawns
                time::delay_for(Duration::from_millis(100)).await;

                // Spawn a couple of tasks on the handle
                for _ in 0..NUM_TASKS {
                    let num_cancelled = num_cancelled.clone();
                    handle.spawn(|tripwire| {
                        async move {
                            forever_stream(tripwire).await;
                            num_cancelled.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                }

                // Finally, signal that tasks are ready
                handle.ready();
            });
        }

        // Join tasks
        handle.join(None).await.expect("BUG: join() failed");

        let num_cancelled = num_cancelled.load(Ordering::SeqCst);
        assert_eq!(num_cancelled, NUM_TASKS);
    }

    // Test that if cleanup after halt takes too long, handler will return the right error
    #[tokio::test]
    async fn test_halthandle_timeout() {
        let handle = HaltHandle::new();

        handle.spawn(|tripwire| {
            async {
                forever_stream(tripwire).await;

                // Delay cleanup on purpose here
                time::delay_for(Duration::from_secs(9001)).await;
            }
        });

        handle.ready();
        handle.halt();
        let res = handle.join(Some(Duration::from_millis(100))).await;

        // Verify we've got a timeout
        match &res {
            Err(HaltError::Timeout) => (),
            _ => panic!(
                "BUG: join result was supposed to be HaltError::Timeout but was instead: {:?}",
                res
            ),
        }
    }

    // Verify panicking works
    #[tokio::test]
    async fn test_halthandle_panic() {
        let handle = HaltHandle::new();

        handle.spawn(|_| {
            async {
                panic!("Things aren't going well");
            }
        });

        handle.ready();
        handle.halt();
        let res = handle.join(Some(Duration::from_millis(100))).await;

        // Verify we've got a join error
        match &res {
            Err(HaltError::Join(_)) => (),
            _ => panic!(
                "BUG: join result was supposed to be HaltError::Join but was instead: {:?}",
                res
            ),
        }
    }
}