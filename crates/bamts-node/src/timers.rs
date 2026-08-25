//! Node-compatible timer provider backed by a single Tokio `DelayQueue`.
//!
//! The JS/embedding thread never touches Tokio directly. Each active timer
//! provider owns one dedicated worker thread that builds one current-thread
//! Tokio runtime with the time driver enabled and owns the sole
//! `DelayQueue<TimerWakeup>`. All queue creation, insertion, removal, and
//! polling happen on that worker under its own runtime, so the JS thread never
//! calls `block_on` and never relies on an ambient Tokio handle — constructing
//! and using a [`NodeHost`](crate::NodeHost) inside an ambient runtime is safe.
//!
//! No JavaScript `Value` ever crosses this boundary: the worker stores only
//! opaque `u64` timer IDs, absolute-ms deadlines, and private `DelayQueue`
//! keys, which never leave the worker and are never reused as JS IDs. The
//! caller keeps its own pending-ID set so empty waits return immediately and a
//! timer that fires and is cancelled in the same window is reported to nobody.

use std::collections::BTreeMap;
use std::future;
use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, TryRecvError};
use std::task::Poll;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bamts_runtime::{CancellationToken, TimerError, TimerProvider, TimerWakeup};
use tokio::runtime::Builder;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::time::{DelayQueue, delay_queue};

/// A timer wakeup paired with its private arming identity.
struct ArmedWakeup {
    wakeup: TimerWakeup,
    generation: u64,
}

/// A command issued by the JS thread and consumed by the timer worker.
enum Command {
    /// Insert a timer that expires after `delay`, echoing `deadline_ms` back in
    /// its wakeup so the Machine can order deliveries deterministically.
    Schedule {
        id: u64,
        deadline_ms: u64,
        generation: u64,
        delay: Duration,
    },
    /// Remove the timer with this ID if it is still armed.
    Cancel { id: u64 },
}

/// Either half of the worker's single-thread event loop can complete first.
enum Event {
    Command(Option<Command>),
    Expired(Option<delay_queue::Expired<ArmedWakeup>>),
}

/// The JS-side handle to a running timer worker thread.
struct Worker {
    /// `None` only transiently while dropping, to signal shutdown before join.
    command_tx: Option<UnboundedSender<Command>>,
    expiry_rx: StdReceiver<ArmedWakeup>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// Spawns the worker thread and blocks until its runtime is built.
    ///
    /// Thread-spawn, runtime-build, and startup-handshake failures all surface
    /// as [`TimerError`]; none of them panic the calling thread.
    fn spawn() -> Result<Self, TimerError> {
        let (command_tx, command_rx) = unbounded_channel::<Command>();
        let (expiry_tx, expiry_rx) = std::sync::mpsc::channel::<ArmedWakeup>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let handle = std::thread::Builder::new()
            .name("bamts-node-timers".to_owned())
            .spawn(move || run_worker(command_rx, expiry_tx, &ready_tx))
            .map_err(|error| {
                TimerError::new(format!("timer worker thread failed to start: {error}"))
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                command_tx: Some(command_tx),
                expiry_rx,
                handle: Some(handle),
            }),
            Ok(Err(message)) => {
                let _ = handle.join();
                Err(TimerError::new(message))
            }
            Err(_) => {
                let _ = handle.join();
                Err(TimerError::new(
                    "timer worker exited before signalling readiness",
                ))
            }
        }
    }

    /// Sends a command to the worker, mapping a dead channel to a `TimerError`.
    fn send(&self, command: Command) -> Result<(), TimerError> {
        self.command_tx
            .as_ref()
            .expect("an active worker retains its command sender")
            .send(command)
            .map_err(|_| TimerError::new("timer worker stopped accepting commands"))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Dropping the sole command sender ends the worker loop; then join so
        // the runtime and `DelayQueue` are torn down before we return.
        self.command_tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Builds the worker runtime, reports readiness, and runs the event loop.
fn run_worker(
    command_rx: UnboundedReceiver<Command>,
    expiry_tx: StdSender<ArmedWakeup>,
    ready_tx: &StdSender<Result<(), String>>,
) {
    let runtime = match Builder::new_current_thread().enable_time().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("timer runtime failed to build: {error}")));
            return;
        }
    };
    if ready_tx.send(Ok(())).is_err() {
        // The caller gave up on startup; there is nothing to serve.
        return;
    }
    runtime.block_on(worker_loop(command_rx, expiry_tx));
}

/// The worker's single-threaded loop over one `DelayQueue`.
///
/// Command polling is biased ahead of expiry polling so a cancel that arrives
/// in the same wake as an expiry is applied before the timer is delivered.
async fn worker_loop(
    mut command_rx: UnboundedReceiver<Command>,
    expiry_tx: StdSender<ArmedWakeup>,
) {
    let mut queue: DelayQueue<ArmedWakeup> = DelayQueue::new();
    let mut keys: BTreeMap<u64, delay_queue::Key> = BTreeMap::new();

    loop {
        let event = future::poll_fn(|cx| {
            if let Poll::Ready(command) = command_rx.poll_recv(cx) {
                return Poll::Ready(Event::Command(command));
            }
            if !queue.is_empty()
                && let Poll::Ready(expired) = queue.poll_expired(cx)
            {
                return Poll::Ready(Event::Expired(expired));
            }
            Poll::Pending
        })
        .await;

        match event {
            Event::Command(Some(Command::Schedule {
                id,
                deadline_ms,
                generation,
                delay,
            })) => {
                if let Some(previous) = keys.remove(&id) {
                    queue.try_remove(&previous);
                }
                let key = queue.insert(
                    ArmedWakeup {
                        wakeup: TimerWakeup { id, deadline_ms },
                        generation,
                    },
                    delay,
                );
                keys.insert(id, key);
            }
            Event::Command(Some(Command::Cancel { id })) => {
                if let Some(key) = keys.remove(&id) {
                    // Keys are never reused as JS IDs and never leave the worker.
                    queue.try_remove(&key);
                }
            }
            // All command senders dropped: shut the worker down.
            Event::Command(None) => break,
            Event::Expired(Some(expired)) => {
                let wakeup = expired.into_inner();
                keys.remove(&wakeup.wakeup.id);
                if expiry_tx.send(wakeup).is_err() {
                    // NodeHost is gone; stop serving.
                    break;
                }
            }
            // Queue drained to empty between wakeups; re-arm on the next loop.
            Event::Expired(None) => {}
        }
    }
}

/// Node's `TimerProvider` for a single Machine lifetime.
///
/// The worker thread is created lazily on the first successful
/// [`schedule`](TimerProvider::schedule) so an unused capability costs nothing
/// and [`NodeHost::new`](crate::NodeHost::new) stays infallible.
pub(crate) struct NodeTimers {
    /// Monotonic base for provider deadlines, captured at construction.
    base: Instant,
    /// Generation assigned to the next successful arming.
    next_generation: u64,
    /// Armed IDs mapped to the private generation of their current arming.
    pending: BTreeMap<u64, u64>,
    worker: Option<Worker>,
}

impl NodeTimers {
    pub(crate) fn new() -> Self {
        Self {
            base: Instant::now(),
            next_generation: 1,
            pending: BTreeMap::new(),
            worker: None,
        }
    }

    /// Absolute provider-monotonic millisecond deadline for `delay_ms`.
    fn deadline_ms(&self, delay_ms: u32) -> u64 {
        let elapsed = u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX);
        elapsed.saturating_add(u64::from(delay_ms))
    }

    /// Returns the running worker, spawning it on first use.
    fn worker(&mut self) -> Result<&Worker, TimerError> {
        if self.worker.is_none() {
            self.worker = Some(Worker::spawn()?);
        }
        Ok(self
            .worker
            .as_ref()
            .expect("worker was just initialised above"))
    }

    fn accept_wakeup(&mut self, armed: ArmedWakeup) -> Option<TimerWakeup> {
        let id = armed.wakeup.id;
        (self.pending.get(&id) == Some(&armed.generation)).then(|| {
            self.pending.remove(&id);
            armed.wakeup
        })
    }

    #[cfg(test)]
    pub(crate) fn worker_active(&self) -> bool {
        self.worker.is_some()
    }
}

impl TimerProvider for NodeTimers {
    fn schedule(&mut self, id: u64, delay_ms: u32) -> Result<u64, TimerError> {
        // Compute the deadline before touching the worker so the returned value
        // is stable even though the actual insert happens on the worker thread.
        let deadline_ms = self.deadline_ms(delay_ms);
        let generation = self.next_generation;
        let next_generation = generation
            .checked_add(1)
            .ok_or_else(|| TimerError::new("timer generation exhausted"))?;
        let worker = self.worker()?;
        worker.send(Command::Schedule {
            id,
            deadline_ms,
            generation,
            delay: Duration::from_millis(u64::from(delay_ms)),
        })?;
        self.next_generation = next_generation;
        self.pending.insert(id, generation);
        Ok(deadline_ms)
    }

    fn cancel(&mut self, id: u64) -> Result<bool, TimerError> {
        // The caller-side pending set answers "was it armed?" and makes a
        // cancel that races a just-fired expiry safe: the worker `try_remove`
        // is a no-op and the stale wakeup is dropped at poll/wait time.
        if self.pending.remove(&id).is_none() {
            return Ok(false);
        }
        if let Some(worker) = self.worker.as_ref() {
            worker.send(Command::Cancel { id })?;
        }
        Ok(true)
    }

    fn poll_expired(&mut self, output: &mut Vec<TimerWakeup>) -> Result<(), TimerError> {
        if self.worker.is_none() {
            return Ok(());
        }
        loop {
            let received = self
                .worker
                .as_ref()
                .expect("worker presence checked before loop")
                .expiry_rx
                .try_recv();
            match received {
                Ok(armed) => {
                    if let Some(wakeup) = self.accept_wakeup(armed) {
                        output.push(wakeup);
                    }
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err(TimerError::new("timer worker expiry channel disconnected"));
                }
            }
        }
    }

    fn wait_expired(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Option<TimerWakeup>, TimerError> {
        loop {
            // An empty pending set never blocks: nothing can arrive that we
            // would report, so return immediately.
            if self.pending.is_empty() {
                return Ok(None);
            }
            if cancel.is_cancelled() {
                return Ok(None);
            }
            let received = match self.worker.as_ref() {
                Some(worker) => worker.expiry_rx.recv_timeout(Duration::from_millis(10)),
                None => return Ok(None),
            };
            match received {
                Ok(armed) => {
                    if let Some(wakeup) = self.accept_wakeup(armed) {
                        return Ok(Some(wakeup));
                    }
                    // Stale wakeup for a cancelled or re-armed ID; keep waiting
                    // while live timers remain (re-checked at the top of the loop).
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Re-check cancellation at the top of the loop.
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(TimerError::new("timer worker expiry channel disconnected"));
                }
            }
        }
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{ArmedWakeup, Command, NodeTimers, worker_loop};
    use std::sync::mpsc::TryRecvError;
    use std::time::Duration;

    use bamts_runtime::{TimerProvider, TimerWakeup};
    use tokio::runtime::Builder;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn rescheduling_removes_the_old_deadline_before_it_can_fire() {
        let runtime = Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime builds");
        let (command_tx, command_rx) = unbounded_channel();
        let (expiry_tx, expiry_rx) = std::sync::mpsc::channel();

        command_tx
            .send(Command::Schedule {
                id: 7,
                deadline_ms: 10,
                generation: 1,
                delay: Duration::from_millis(10),
            })
            .expect("first timer command sends");
        command_tx
            .send(Command::Schedule {
                id: 7,
                deadline_ms: 500,
                generation: 2,
                delay: Duration::from_millis(500),
            })
            .expect("replacement timer command sends");

        runtime.block_on(async move {
            let worker = tokio::spawn(worker_loop(command_rx, expiry_tx));
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(matches!(expiry_rx.try_recv(), Err(TryRecvError::Empty)));

            command_tx
                .send(Command::Cancel { id: 7 })
                .expect("replacement cancellation sends");
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(matches!(expiry_rx.try_recv(), Err(TryRecvError::Empty)));

            drop(command_tx);
            worker.await.expect("worker exits cleanly");
            assert!(matches!(
                expiry_rx.try_recv(),
                Err(TryRecvError::Disconnected)
            ));
        });
    }

    #[test]
    fn same_deadline_rearm_rejects_stale_generation() {
        let mut timers = NodeTimers::new();
        timers.pending.insert(7, 2);

        assert!(
            timers
                .accept_wakeup(ArmedWakeup {
                    wakeup: TimerWakeup {
                        id: 7,
                        deadline_ms: 50,
                    },
                    generation: 1,
                })
                .is_none()
        );
        assert!(timers.has_pending());

        let wakeup = timers
            .accept_wakeup(ArmedWakeup {
                wakeup: TimerWakeup {
                    id: 7,
                    deadline_ms: 50,
                },
                generation: 2,
            })
            .expect("current arming is delivered");
        assert_eq!(wakeup.id, 7);
        assert_eq!(wakeup.deadline_ms, 50);
        assert!(!timers.has_pending());
    }
}
