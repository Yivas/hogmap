//! Scanner-to-UI protocol and the reducer that applies it to an [`Index`].
//!
//! The payloads are plain data: no `EntryId`, `io::Error`, closures or shared
//! references. A future elevated NTFS producer can send the same events over
//! IPC while the UI keeps owning the index. Node keys are producer-local; they
//! are not persistent file identifiers.
//!
//! The record contract does not include attributes yet. A producer must not
//! recurse into reparse points or hydrate cloud placeholders regardless; typed
//! attributes will be added when the Win32 producer needs to classify them,
//! not by guessing Win32 bits here.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::mpsc::TryRecvError;

mod channel;
pub(crate) use channel::ScanWake;
pub use channel::{ScanCancellation, ScanReceiver, ScanSender, event_channel};
use std::time::{Duration, Instant};

use crate::index::{EntryId, FileIdentity, Index, Kind, Sizes, State};

/// Maximum records in one [`Batch`], independent of the byte estimate.
pub const MAX_BATCH_RECORDS: usize = 4096;
/// Maximum estimated payload bytes in one [`Batch`].
pub const MAX_BATCH_BYTES: usize = 1 << 20;
/// Starting queue depth for a bounded producer/consumer channel.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 16;

/// Identifies one scan session. A consumer rejects events from another generation.
/// This is not authentication: it cannot tell who produced an event, so it is not a
/// security control for a future IPC transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScanGeneration(u64);

impl ScanGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Producer-local node identifier. A consumer maps it to an [`EntryId`] per generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeKey(u64);

impl NodeKey {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// One metadata record. The producer must emit a parent before its children;
/// the consumer does not buffer out-of-order records for later resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRecord {
    pub key: NodeKey,
    /// `None` marks the scan root. Exactly one root record is expected.
    pub parent: Option<NodeKey>,
    pub name: OsString,
    pub kind: Kind,
    pub sizes: Sizes,
    pub identity: Option<FileIdentity>,
}

impl ScanRecord {
    /// Rough payload size used for batch limits; it is not a serialized size.
    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.name.len())
    }
}

/// An outcome follows all immediate child records of this directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryOutcome {
    pub key: NodeKey,
    pub code: Option<u32>,
}

/// A bounded group of records and directory outcomes, capped by combined items and bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    records: Vec<ScanRecord>,
    outcomes: Vec<DirectoryOutcome>,
    estimated_bytes: usize,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.outcomes.is_empty()
    }

    pub fn outcomes(&self) -> &[DirectoryOutcome] {
        &self.outcomes
    }

    pub fn push_outcome(&mut self, outcome: DirectoryOutcome) -> bool {
        let extra = std::mem::size_of::<DirectoryOutcome>();
        if self.records.len() + self.outcomes.len() >= MAX_BATCH_RECORDS
            || (!self.is_empty() && self.estimated_bytes.saturating_add(extra) > MAX_BATCH_BYTES)
        {
            return false;
        }
        self.outcomes.push(outcome);
        self.estimated_bytes += extra;
        true
    }

    pub fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub fn records(&self) -> &[ScanRecord] {
        &self.records
    }

    /// Adds a record unless the batch is full. An empty batch accepts an
    /// oversized record rather than losing it.
    pub fn push(&mut self, record: ScanRecord) -> bool {
        let extra = record.estimated_bytes();
        let exceeds_records = self.records.len() + self.outcomes.len() >= MAX_BATCH_RECORDS;
        let exceeds_bytes =
            !self.is_empty() && self.estimated_bytes.saturating_add(extra) > MAX_BATCH_BYTES;
        if exceeds_records || exceeds_bytes {
            return false;
        }
        self.records.push(record);
        self.estimated_bytes = self.estimated_bytes.saturating_add(extra);
        true
    }
}

/// Activity counters. They have no total or percentage because enumeration
/// usually lacks a trustworthy denominator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub records: u64,
    pub directories_done: u64,
    pub directories_failed: u64,
}

/// The only valid endings. A producer sends exactly one, after all its work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    Finished,
    Cancelled,
    Failed(u32),
}

/// One kind of progress through a scan. `DirectoryDone` means the directory's
/// immediate children were enumerated; it says nothing about their subtrees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanEventKind {
    Batch(Batch),
    DirectoryDone(NodeKey),
    DirectoryFailed { key: NodeKey, code: u32 },
    Progress(Progress),
    Terminal(Terminal),
}

/// A generation-tagged event. A reducer ignores every event from another generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanEvent {
    pub generation: ScanGeneration,
    pub event: ScanEventKind,
}

impl ScanEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self.event, ScanEventKind::Terminal(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// The producer must stop and send its `Cancelled` terminal.
    Cancelled,
    /// The consumer is gone; no terminal event can be observed.
    Disconnected,
}

/// Sends an essential event, waiting for queue space while checking cancellation.
/// Progress events are coalescable and should use a single `try_send` instead.
pub fn send_event(
    sender: &ScanSender,
    event: ScanEvent,
    cancellation: &ScanCancellation,
) -> SendOutcome {
    sender.send_cancellable(event, Some(cancellation))
}

/// Sends the single terminal event. It ignores the cancellation flag so the
/// consumer can still observe the ending, and blocks while the queue is full.
///
/// A consumer that requests cancellation must keep draining until it sees this
/// terminal or a disconnect, or drop the [`ScanReceiver`]. Because this send ignores
/// cancellation, a consumer that stops draining without dropping the receiver
/// leaves the producer blocked here forever.
pub fn send_terminal(sender: &ScanSender, generation: ScanGeneration, terminal: Terminal) -> bool {
    sender
        .send(ScanEvent {
            generation,
            event: ScanEventKind::Terminal(terminal),
        })
        .is_ok()
}

/// Protocol problems and expected cancellations. Nothing is silently discarded:
/// every rejected record or event increments a counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanDiagnostics {
    pub stale_generation_events: u64,
    pub unknown_parent_records: u64,
    pub duplicate_key_records: u64,
    pub invalid_records: u64,
    pub unknown_directory_events: u64,
    pub duplicate_directory_outcomes: u64,
    pub events_after_terminal: u64,
    pub partial_directories: u64,
    pub directory_failures: u64,
    pub last_directory_failure_code: Option<u32>,
    pub missing_terminal: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Applied events or resumed batch slices, not unique channel messages.
    pub events: usize,
    pub records: usize,
    pub directories: usize,
    /// The reducer's terminal, if one has been accepted.
    pub terminal: Option<Terminal>,
    pub disconnected: bool,
    /// Time applying events, excluding receive and idle wait.
    pub apply_time: Duration,
}

/// Owns the fresh index for one generation and applies bounded event batches.
#[derive(Debug)]
pub struct ScanReducer {
    generation: ScanGeneration,
    index: Option<Index>,
    keys: ProducerKeys,
    diagnostics: ScanDiagnostics,
    progress: Progress,
    terminal: Option<Terminal>,
    memory_before_release: (usize, usize),
    pending_batch: Option<PendingBatch>,
}

/// A cursor never overtakes its records: all parents precede children, outcomes follow every
/// record in this batch, and no later event (including the single terminal) is consumed until
/// it is empty. Replacing the generation drops this cursor with the old reducer and receiver.
#[derive(Debug)]
struct PendingBatch {
    batch: Batch,
    record: usize,
    outcome: usize,
}

/// Dense built-in producer keys use four-byte slots. Sparse/untrusted keys never dictate
/// an arbitrary allocation: at most twice the accepted key count plus a small reorder window.
#[derive(Debug, Default)]
struct ProducerKeys {
    dense: Vec<Option<EntryId>>,
    sparse: HashMap<NodeKey, EntryId>,
    count: usize,
}
impl ProducerKeys {
    fn get(&self, key: NodeKey) -> Option<EntryId> {
        usize::try_from(key.value())
            .ok()
            .and_then(|key| self.dense.get(key))
            .copied()
            .flatten()
            .or_else(|| self.sparse.get(&key).copied())
    }
    fn insert(&mut self, key: NodeKey, id: EntryId) {
        if let Ok(position) = usize::try_from(key.value())
            && position
                <= self
                    .count
                    .saturating_mul(2)
                    .saturating_add(MAX_BATCH_RECORDS)
        {
            if position >= self.dense.len() {
                self.dense.resize(position + 1, None);
            }
            self.dense[position] = Some(id);
        } else {
            self.sparse.insert(key, id);
        }
        self.count += 1;
    }
    fn memory_bytes(&self) -> usize {
        self.dense.capacity() * std::mem::size_of::<Option<EntryId>>()
            + crate::index::hash_bytes::<(NodeKey, EntryId)>(self.sparse.capacity())
    }
}

impl ScanReducer {
    pub fn new(generation: ScanGeneration) -> Self {
        Self {
            generation,
            index: None,
            keys: ProducerKeys::default(),
            diagnostics: ScanDiagnostics::default(),
            progress: Progress::default(),
            terminal: None,
            memory_before_release: (0, 0),
            pending_batch: None,
        }
    }

    /// Index and producer-key allocation estimates at terminal/disconnect, not peak memory.
    pub fn memory_before_release(&self) -> (usize, usize) {
        self.memory_before_release
    }

    pub fn generation(&self) -> ScanGeneration {
        self.generation
    }

    /// `None` until the root record creates the index.
    pub fn index(&self) -> Option<&Index> {
        self.index.as_ref()
    }

    /// Mutable access for an explicit post-pass only after a Finished terminal was consumed.
    pub fn finished_index_mut(&mut self) -> Option<&mut Index> {
        if self.terminal == Some(Terminal::Finished) {
            self.index.as_mut()
        } else {
            None
        }
    }

    pub fn diagnostics(&self) -> &ScanDiagnostics {
        &self.diagnostics
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }

    pub fn terminal(&self) -> Option<Terminal> {
        self.terminal
    }

    /// Applies whole batches and control events, stopping near `max_records`.
    pub fn drain(&mut self, receiver: &ScanReceiver, max_records: usize) -> DrainReport {
        let record_budget = max_records.max(1);
        let event_budget = record_budget.saturating_mul(4).max(64);
        self.drain_limited(receiver, record_budget, event_budget, None)
    }

    /// Drains at most `max_events` events/slices, checking a positive deadline every 32 records
    /// and after each outcome. A single record can exceed it; a nonempty queue always advances.
    /// A zero budget retains the original whole-event progress guarantee.
    pub fn drain_budgeted(
        &mut self,
        receiver: &ScanReceiver,
        max_events: usize,
        time_budget: Duration,
    ) -> DrainReport {
        self.drain_limited(receiver, usize::MAX, max_events.max(1), Some(time_budget))
    }

    fn drain_limited(
        &mut self,
        receiver: &ScanReceiver,
        record_budget: usize,
        event_budget: usize,
        time_budget: Option<Duration>,
    ) -> DrainReport {
        let started = Instant::now();
        let mut report = DrainReport::default();
        while report.events < event_budget && report.records < record_budget {
            if report.events > 0 && time_budget.is_some_and(|budget| started.elapsed() >= budget) {
                break;
            }
            if let Some(mut pending) = self.pending_batch.take() {
                let apply_started = Instant::now();
                let complete = self.resume_batch(&mut pending, started, time_budget, &mut report);
                report.apply_time += apply_started.elapsed();
                report.events += 1;
                if !complete {
                    self.pending_batch = Some(pending);
                    break;
                }
                continue;
            }
            match receiver.try_recv() {
                Ok(event) => {
                    let belongs = event.generation == self.generation;
                    if belongs
                        && self.terminal.is_none()
                        && time_budget.is_some_and(|budget| !budget.is_zero())
                        && matches!(event.event, ScanEventKind::Batch(_))
                    {
                        if let ScanEventKind::Batch(batch) = event.event {
                            self.pending_batch = Some(PendingBatch {
                                batch,
                                record: 0,
                                outcome: 0,
                            });
                        }
                        continue;
                    }
                    let records = match &event.event {
                        ScanEventKind::Batch(batch) => {
                            if belongs {
                                report.directories += batch
                                    .records()
                                    .iter()
                                    .filter(|record| record.kind == Kind::Directory)
                                    .count();
                            }
                            batch.len()
                        }
                        _ => 0,
                    };
                    let terminal = belongs && event.is_terminal();
                    let apply_started = Instant::now();
                    self.apply(event);
                    report.apply_time += apply_started.elapsed();
                    report.events += 1;
                    if belongs {
                        report.records += records;
                    }
                    if terminal {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    report.disconnected = true;
                    if self.terminal.is_none() {
                        self.diagnostics.missing_terminal = true;
                        self.partialize_pending();
                        self.release_keys();
                    }
                    break;
                }
            }
        }
        report.terminal = self.terminal;
        report
    }

    fn resume_batch(
        &mut self,
        pending: &mut PendingBatch,
        started: Instant,
        budget: Option<Duration>,
        report: &mut DrainReport,
    ) -> bool {
        while let Some(record) = pending.batch.records().get(pending.record) {
            self.apply_record(record);
            report.records += 1;
            report.directories += usize::from(record.kind == Kind::Directory);
            pending.record += 1;
            if pending.record.is_multiple_of(32)
                && budget.is_some_and(|budget| !budget.is_zero() && started.elapsed() >= budget)
            {
                return false;
            }
        }
        while let Some(outcome) = pending.batch.outcomes().get(pending.outcome) {
            self.finish_directory(outcome.key, outcome.code);
            pending.outcome += 1;
            if budget.is_some_and(|budget| !budget.is_zero() && started.elapsed() >= budget) {
                return pending.outcome == pending.batch.outcomes().len();
            }
        }
        true
    }

    fn apply(&mut self, event: ScanEvent) {
        if event.generation != self.generation {
            self.diagnostics.stale_generation_events += 1;
            return;
        }
        if self.terminal.is_some() {
            self.diagnostics.events_after_terminal += 1;
            return;
        }
        match event.event {
            ScanEventKind::Batch(batch) => {
                for record in batch.records() {
                    self.apply_record(record);
                }
                for outcome in batch.outcomes() {
                    self.finish_directory(outcome.key, outcome.code);
                }
            }
            ScanEventKind::DirectoryDone(key) => self.finish_directory(key, None),
            ScanEventKind::DirectoryFailed { key, code } => {
                self.finish_directory(key, Some(code));
            }
            ScanEventKind::Progress(progress) => self.progress = progress,
            ScanEventKind::Terminal(terminal) => {
                self.terminal = Some(terminal);
                self.partialize_pending();
                self.release_keys();
            }
        }
    }

    fn apply_record(&mut self, record: &ScanRecord) {
        if self.keys.get(record.key).is_some() {
            self.diagnostics.duplicate_key_records += 1;
            return;
        }
        if self.index.is_none() {
            self.apply_root_record(record);
            return;
        }
        let Some(parent_key) = record.parent else {
            self.diagnostics.invalid_records += 1;
            return;
        };
        let Some(parent_id) = self.keys.get(parent_key) else {
            self.diagnostics.unknown_parent_records += 1;
            return;
        };
        let state = if record.kind == Kind::Directory {
            State::Pending
        } else {
            State::Complete
        };
        let inserted = match self.index.as_mut() {
            Some(index) => index.insert(
                parent_id,
                record.name.clone(),
                record.kind,
                record.sizes,
                record.identity,
                state,
            ),
            None => return,
        };
        match inserted {
            Ok(id) => {
                self.keys.insert(record.key, id);
            }
            Err(_) => self.diagnostics.invalid_records += 1,
        }
    }

    fn apply_root_record(&mut self, record: &ScanRecord) {
        if record.parent.is_some() {
            self.diagnostics.unknown_parent_records += 1;
            return;
        }
        if record.kind != Kind::Directory {
            self.diagnostics.invalid_records += 1;
            return;
        }
        match Index::new(record.name.clone()) {
            Ok(index) => {
                let root = index.root();
                self.index = Some(index);
                self.keys.insert(record.key, root);
            }
            Err(_) => self.diagnostics.invalid_records += 1,
        }
    }

    fn finish_directory(&mut self, key: NodeKey, failure_code: Option<u32>) {
        let Some(id) = self.keys.get(key) else {
            self.diagnostics.unknown_directory_events += 1;
            return;
        };
        let is_directory = self
            .index
            .as_ref()
            .and_then(|index| index.get(id))
            .is_some_and(|entry| entry.kind == Kind::Directory);
        if !is_directory {
            self.diagnostics.unknown_directory_events += 1;
            return;
        }
        if self
            .index
            .as_ref()
            .and_then(|index| index.get(id))
            .is_none_or(|entry| entry.state != State::Pending)
        {
            self.diagnostics.duplicate_directory_outcomes += 1;
            return;
        }
        let state = if failure_code.is_some() {
            State::Inaccessible
        } else {
            State::Complete
        };
        let updated = match self.index.as_mut() {
            Some(index) => index.set_state(id, state).is_ok(),
            None => false,
        };
        if !updated {
            self.diagnostics.invalid_records += 1;
            return;
        }
        if let Some(code) = failure_code {
            self.diagnostics.directory_failures += 1;
            self.diagnostics.last_directory_failure_code = Some(code);
        }
    }

    /// Every later event is rejected before key resolution, so the map is dead weight once
    /// the scan has ended. The UI keeps the reducer for its snapshot, not for more records.
    fn release_keys(&mut self) {
        self.memory_before_release = (
            self.index
                .as_ref()
                .map_or(0, |index| index.memory_estimate().total_bytes()),
            self.keys.memory_bytes(),
        );
        self.keys = ProducerKeys::default();
    }

    fn partialize_pending(&mut self) {
        // Unknown sizes do not require state changes. The common successful terminal must not
        // walk millions of complete files merely to discover there is nothing to partialize.
        if self.index.as_ref().is_none_or(Index::all_entries_complete) {
            return;
        }
        let count = self.index.as_ref().map_or(0, Index::len);
        let mut partial = 0;
        for position in 0..count {
            let id = EntryId::at(position).expect("stored ID fits");
            if self
                .index
                .as_ref()
                .and_then(|index| index.get(id))
                .is_none_or(|entry| entry.kind != Kind::Directory || entry.state != State::Pending)
            {
                continue;
            }
            let updated = match self.index.as_mut() {
                Some(index) => index.set_state(id, State::Partial).is_ok(),
                None => false,
            };
            if updated {
                partial += 1;
            } else {
                self.diagnostics.invalid_records += 1;
            }
        }
        self.diagnostics.partial_directories += partial;
    }
}
