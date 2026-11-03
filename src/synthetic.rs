//! Deterministic synthetic producer for the scanner contract.
//!
//! It builds a fixed tree from a seed and streams it through the same bounded
//! channel and [`ScanReducer`](crate::scan::ScanReducer) as a real producer.
//! Nothing here reads a disk. This is the fixture for contract and layout work,
//! not a substitute for a filesystem scanner.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::sync::mpsc::TrySendError;

use crate::index::{FileIdentity, Kind, Sizes};
use crate::scan::{
    Batch, DirectoryOutcome, NodeKey, Progress, ScanCancellation, ScanEvent, ScanEventKind,
    ScanGeneration, ScanRecord, ScanSender, SendOutcome, Terminal, send_event, send_terminal,
};

/// Synthetic failure code for an inaccessible directory. It is not a Win32 error.
pub const SYNTHETIC_ACCESS_DENIED: u32 = 0xFFFF_FF01;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntheticSpec {
    pub root_name: OsString,
    /// Directories below the root.
    pub directories: u32,
    pub files: u32,
    /// First directories below the root reported with [`SYNTHETIC_ACCESS_DENIED`].
    pub inaccessible_directories: u32,
    /// Extra file entries that share the identity of an earlier file.
    pub hardlink_pairs: u32,
    /// First files whose logical and allocated sizes are unknown.
    pub unknown_size_files: u32,
    /// Emit coalescable progress every N records; zero disables the periodic ones. A final
    /// summary is sent before the terminal even then.
    pub progress_every: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProducerOutcome {
    Finished,
    Cancelled,
    Disconnected,
}

/// Streams one synthetic tree. The root is emitted first and every parent
/// before its children. The receiver may be closed at any time.
pub fn run_synthetic(
    generation: ScanGeneration,
    seed: u64,
    spec: SyntheticSpec,
    sender: ScanSender,
    cancellation: &ScanCancellation,
) -> ProducerOutcome {
    if cancellation.is_cancelled() {
        return abort(&sender, generation, SendFailure::Cancelled);
    }
    let plan = build_plan(seed, &spec);
    let mut batch = Batch::new();
    let mut progress = Progress::default();
    for (index, record) in plan.records.iter().enumerate() {
        if cancellation.is_cancelled() {
            return abort(&sender, generation, SendFailure::Cancelled);
        }
        if !batch.push(record.clone()) {
            if let Err(failure) = flush(&sender, generation, &mut batch, cancellation) {
                return abort(&sender, generation, failure);
            }
            let _ = batch.push(record.clone());
        }
        progress.records += 1;

        for key in &plan.finish_after[index] {
            let failed = plan.inaccessible.contains(key);
            if failed {
                progress.directories_failed += 1;
            } else {
                progress.directories_done += 1;
            }
            if let Err(failure) =
                send_directory_outcome(&sender, generation, &mut batch, *key, failed, cancellation)
            {
                return abort(&sender, generation, failure);
            }
        }

        if spec.progress_every > 0
            && progress.records % u64::from(spec.progress_every) == 0
            && !send_progress(&sender, generation, progress)
        {
            return ProducerOutcome::Disconnected;
        }
    }

    for key in &plan.finish_after[plan.records.len()] {
        let failed = plan.inaccessible.contains(key);
        if failed {
            progress.directories_failed += 1;
        } else {
            progress.directories_done += 1;
        }
        if let Err(failure) =
            send_directory_outcome(&sender, generation, &mut batch, *key, failed, cancellation)
        {
            return abort(&sender, generation, failure);
        }
    }
    if let Err(failure) = flush(&sender, generation, &mut batch, cancellation) {
        return abort(&sender, generation, failure);
    }
    if !send_progress(&sender, generation, progress) {
        return ProducerOutcome::Disconnected;
    }
    if send_terminal(&sender, generation, Terminal::Finished) {
        ProducerOutcome::Finished
    } else {
        ProducerOutcome::Disconnected
    }
}

#[derive(Clone, Copy)]
enum SendFailure {
    Cancelled,
    Disconnected,
}

fn abort(sender: &ScanSender, generation: ScanGeneration, failure: SendFailure) -> ProducerOutcome {
    match failure {
        SendFailure::Cancelled => {
            if send_terminal(sender, generation, Terminal::Cancelled) {
                ProducerOutcome::Cancelled
            } else {
                ProducerOutcome::Disconnected
            }
        }
        SendFailure::Disconnected => ProducerOutcome::Disconnected,
    }
}

fn flush(
    sender: &ScanSender,
    generation: ScanGeneration,
    batch: &mut Batch,
    cancellation: &ScanCancellation,
) -> Result<(), SendFailure> {
    if batch.is_empty() {
        return Ok(());
    }
    let event = ScanEvent {
        generation,
        event: ScanEventKind::Batch(std::mem::take(batch)),
    };
    match send_event(sender, event, cancellation) {
        SendOutcome::Sent => Ok(()),
        SendOutcome::Cancelled => Err(SendFailure::Cancelled),
        SendOutcome::Disconnected => Err(SendFailure::Disconnected),
    }
}

fn send_progress(sender: &ScanSender, generation: ScanGeneration, progress: Progress) -> bool {
    let event = ScanEvent {
        generation,
        event: ScanEventKind::Progress(progress),
    };
    match sender.try_send(event) {
        Ok(()) | Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn send_directory_outcome(
    sender: &ScanSender,
    generation: ScanGeneration,
    batch: &mut Batch,
    key: NodeKey,
    failed: bool,
    cancellation: &ScanCancellation,
) -> Result<(), SendFailure> {
    let outcome = DirectoryOutcome {
        key,
        code: failed.then_some(SYNTHETIC_ACCESS_DENIED),
    };
    if !batch.push_outcome(outcome) {
        flush(sender, generation, batch, cancellation)?;
        assert!(batch.push_outcome(outcome));
    }
    Ok(())
}

struct Plan {
    records: Vec<ScanRecord>,
    finish_after: Vec<Vec<NodeKey>>,
    inaccessible: HashSet<NodeKey>,
}

fn build_plan(seed: u64, spec: &SyntheticSpec) -> Plan {
    let mut rng = SplitMix64::new(seed);
    let root_key = NodeKey::new(0);
    let mut next_key = 1_u64;
    let mut records = vec![ScanRecord {
        key: root_key,
        parent: None,
        name: spec.root_name.clone(),
        kind: Kind::Directory,
        sizes: Sizes::default(),
        identity: None,
    }];
    let mut directory_keys = vec![root_key];
    let mut accessible = vec![root_key];
    let mut inaccessible = HashSet::new();
    let inaccessible_count = spec.inaccessible_directories.min(spec.directories);
    for index in 0..spec.directories {
        let parent = accessible[rng.below(accessible.len())];
        let key = NodeKey::new(next_key);
        next_key += 1;
        let blocked = index < inaccessible_count;
        records.push(ScanRecord {
            key,
            parent: Some(parent),
            name: format!("dir_{index:05}").into(),
            kind: Kind::Directory,
            sizes: Sizes::default(),
            identity: None,
        });
        directory_keys.push(key);
        if blocked {
            inaccessible.insert(key);
        } else {
            accessible.push(key);
        }
    }

    let unknown_count = spec.unknown_size_files.min(spec.files);
    let mut files = Vec::with_capacity(spec.files as usize);
    for index in 0..spec.files {
        let parent = accessible[rng.below(accessible.len())];
        let key = NodeKey::new(next_key);
        next_key += 1;
        let sizes = if index < unknown_count {
            Sizes::default()
        } else {
            random_sizes(&mut rng)
        };
        let identity = Some(FileIdentity {
            volume: 1,
            file: u128::from(index) + 1,
        });
        records.push(ScanRecord {
            key,
            parent: Some(parent),
            name: format!("file_{index:05}.bin").into(),
            kind: Kind::File,
            sizes,
            identity,
        });
        files.push((sizes, identity));
    }

    let pair_count = spec.hardlink_pairs.min(spec.files) as usize;
    for (index, (sizes, identity)) in files.iter().take(pair_count).enumerate() {
        let parent = accessible[rng.below(accessible.len())];
        let key = NodeKey::new(next_key);
        next_key += 1;
        records.push(ScanRecord {
            key,
            parent: Some(parent),
            name: format!("link_{index:05}.bin").into(),
            kind: Kind::File,
            sizes: *sizes,
            identity: *identity,
        });
    }

    let mut last_child = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(parent) = record.parent {
            last_child.insert(parent, index);
        }
    }
    let mut finish_after = vec![Vec::new(); records.len() + 1];
    for key in directory_keys {
        let index = last_child.get(&key).copied().unwrap_or(records.len());
        finish_after[index].push(key);
    }
    for keys in &mut finish_after {
        keys.sort();
    }

    Plan {
        records,
        finish_after,
        inaccessible,
    }
}

fn random_sizes(rng: &mut SplitMix64) -> Sizes {
    let bucket = rng.next_u64() % 100;
    let logical = match bucket {
        0..=9 => 0,
        10..=74 => rng.next_u64() % (64 * 1024),
        75..=94 => rng.next_u64() % (4 * 1024 * 1024),
        _ => rng.next_u64() % (256 * 1024 * 1024),
    };
    let allocated = logical.div_ceil(4096) * 4096;
    Sizes {
        logical: Some(logical),
        allocated: Some(allocated),
    }
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }
}
