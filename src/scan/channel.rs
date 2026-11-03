//! Bounded event transport with notification-based cancellation and disconnect.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use super::{ScanEvent, SendOutcome};

/// Holding this lock while checking external state closes the check-to-wait race.
#[derive(Debug, Default)]
pub(crate) struct ScanWake {
    epoch: Mutex<u64>,
    ready: Condvar,
}
impl ScanWake {
    pub(crate) fn notify(&self) {
        let mut epoch = self.epoch.lock().unwrap_or_else(|error| error.into_inner());
        *epoch = epoch.wrapping_add(1);
        self.ready.notify_all();
    }
    pub(crate) fn listen(&self) -> std::sync::MutexGuard<'_, u64> {
        self.epoch.lock().unwrap_or_else(|error| error.into_inner())
    }
    pub(crate) fn wait<'a>(&self, guard: std::sync::MutexGuard<'a, u64>) {
        drop(
            self.ready
                .wait(guard)
                .unwrap_or_else(|error| error.into_inner()),
        );
    }
}

#[derive(Debug, Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    waiters: Mutex<Vec<Weak<ScanWake>>>,
}

/// Cancelling wakes both the work queue and blocked channel senders.
#[derive(Clone, Debug, Default)]
pub struct ScanCancellation(Arc<CancellationInner>);
impl ScanCancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        let waiters: Vec<_> = self
            .0
            .waiters
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for waiter in waiters {
            waiter.notify();
        }
    }
    pub(crate) fn register(&self, wake: &Arc<ScanWake>) {
        let mut waiters = self.0.waiters.lock().unwrap();
        waiters.retain(|waiter| waiter.strong_count() > 0);
        if !waiters
            .iter()
            .any(|waiter| waiter.ptr_eq(&Arc::downgrade(wake)))
        {
            waiters.push(Arc::downgrade(wake));
        }
    }
}

struct ChannelState {
    events: VecDeque<ScanEvent>,
    senders: usize,
    receiver_alive: bool,
}
struct EventChannel {
    state: Mutex<ChannelState>,
    wake: Arc<ScanWake>,
    capacity: usize,
    repaint: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

pub struct ScanSender(Arc<EventChannel>);
pub struct ScanReceiver(Arc<EventChannel>);

/// Capacity counts whole capped batches, not individual metadata records.
pub fn event_channel(capacity: usize) -> (ScanSender, ScanReceiver) {
    let channel = Arc::new(EventChannel {
        state: Mutex::new(ChannelState {
            events: VecDeque::new(),
            senders: 1,
            receiver_alive: true,
        }),
        wake: Arc::new(ScanWake::default()),
        capacity: capacity.max(1),
        repaint: Mutex::new(None),
    });
    (ScanSender(channel.clone()), ScanReceiver(channel))
}
impl Clone for ScanSender {
    fn clone(&self) -> Self {
        self.0.state.lock().unwrap().senders += 1;
        Self(self.0.clone())
    }
}
impl Drop for ScanSender {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().senders -= 1;
        self.notify();
    }
}
impl ScanSender {
    fn notify(&self) {
        self.0.wake.notify();
        // Release the lock before calling out, so a waker that touches the channel cannot deadlock
        // and producers are not serialised behind the callback.
        let repaint = self.0.repaint.lock().unwrap().clone();
        if let Some(repaint) = repaint {
            repaint();
        }
    }
    /// A wakeup belongs to transport, never to the plain-data event payload.
    pub fn set_waker(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.0.repaint.lock().unwrap() = Some(Arc::new(wake));
    }
    pub fn try_send(&self, event: ScanEvent) -> Result<(), TrySendError<ScanEvent>> {
        let mut state = self.0.state.lock().unwrap();
        if !state.receiver_alive {
            return Err(TrySendError::Disconnected(event));
        }
        if state.events.len() >= self.0.capacity {
            return Err(TrySendError::Full(event));
        }
        state.events.push_back(event);
        drop(state);
        self.notify();
        Ok(())
    }
    pub(super) fn send_cancellable(
        &self,
        event: ScanEvent,
        cancel: Option<&ScanCancellation>,
    ) -> SendOutcome {
        self.send_inner(event, cancel).0
    }
    fn send_inner(
        &self,
        event: ScanEvent,
        cancel: Option<&ScanCancellation>,
    ) -> (SendOutcome, Option<ScanEvent>) {
        if let Some(cancel) = cancel {
            cancel.register(&self.0.wake);
        }
        loop {
            let listener = self.0.wake.listen();
            if cancel.is_some_and(ScanCancellation::is_cancelled) {
                return (SendOutcome::Cancelled, Some(event));
            }
            let mut state = self.0.state.lock().unwrap();
            if !state.receiver_alive {
                return (SendOutcome::Disconnected, Some(event));
            }
            if state.events.len() < self.0.capacity {
                state.events.push_back(event);
                drop(state);
                drop(listener);
                self.notify();
                return (SendOutcome::Sent, None);
            }
            drop(state);
            self.0.wake.wait(listener);
        }
    }
    /// An uncancellable control send; terminal delivery retains the drain-or-drop rule.
    pub fn send(&self, event: ScanEvent) -> Result<(), ScanEvent> {
        match self.send_inner(event, None).1 {
            None => Ok(()),
            Some(event) => Err(event),
        }
    }
}
impl ScanReceiver {
    pub fn try_recv(&self) -> Result<ScanEvent, TryRecvError> {
        let mut state = self.0.state.lock().unwrap();
        if let Some(event) = state.events.pop_front() {
            drop(state);
            self.0.wake.notify();
            Ok(event)
        } else if state.senders == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }
    pub fn has_pending(&self) -> bool {
        !self.0.state.lock().unwrap().events.is_empty()
    }
    pub fn wait_ready(&self) {
        loop {
            let listener = self.0.wake.listen();
            let state = self.0.state.lock().unwrap();
            if !state.events.is_empty() || state.senders == 0 {
                return;
            }
            drop(state);
            self.0.wake.wait(listener);
        }
    }
    /// Waits without consuming; a timeout allows a headless consumer to report liveness.
    pub fn wait_ready_timeout(&self, timeout: Duration) {
        let listener = self.0.wake.listen();
        let state = self.0.state.lock().unwrap();
        if !state.events.is_empty() || state.senders == 0 {
            return;
        }
        drop(state);
        drop(self.0.wake.ready.wait_timeout(listener, timeout).unwrap());
    }
    pub fn recv(&self) -> Result<ScanEvent, RecvTimeoutError> {
        self.recv_deadline(None)
    }
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ScanEvent, RecvTimeoutError> {
        self.recv_deadline(Some(Instant::now() + timeout))
    }
    fn recv_deadline(&self, deadline: Option<Instant>) -> Result<ScanEvent, RecvTimeoutError> {
        loop {
            let listener = self.0.wake.listen();
            let mut state = self.0.state.lock().unwrap();
            if let Some(event) = state.events.pop_front() {
                drop(state);
                drop(listener);
                self.0.wake.notify();
                return Ok(event);
            }
            if state.senders == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            drop(state);
            if let Some(deadline) = deadline {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return Err(RecvTimeoutError::Timeout);
                };
                drop(self.0.wake.ready.wait_timeout(listener, remaining).unwrap());
            } else {
                self.0.wake.wait(listener);
            }
        }
    }
}
impl Drop for ScanReceiver {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.receiver_alive = false;
        state.events.clear();
        drop(state);
        self.0.wake.notify();
    }
}
