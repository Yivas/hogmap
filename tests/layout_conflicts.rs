//! Measurement harness for the corrected-size layout path.
//!
//! Ignored by default: it builds a large index only to time one cold layout, so it must not slow
//! the normal suite. Run it with `cargo test --release --test layout_conflicts -- --ignored
//! --nocapture` to read the figure.
//!
//! The point is the difference between the two paths. Without authoritative sizes the layout reads
//! the listed size of every entry. With them it also walks the entries forward to charge each
//! identity once and asks, for every file that carries an identity, whether that identity was
//! corrected. Every file here carries one, as a real scan does, so the figure covers that whole
//! branch and not the lookup alone: the forward pass and the seen-identity set cost too, and this
//! harness does not separate them.

use hogmap::index::{FileIdentity, Index, Kind, Sizes, State};
use hogmap::treemap::{LayoutParams, Rect, layout};

const FILES: usize = 200_000;
const CONFLICTS: usize = 422;
/// Small enough to run in the normal suite; the large figure above is only for the measurement.
const SMALL_FILES: usize = 1_000;
const SMALL_CONFLICTS: usize = 8;

fn env_count(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn identity(number: u64) -> FileIdentity {
    FileIdentity {
        volume: 7,
        file: u128::from(number) | (1 << 63),
    }
}

/// Builds one directory with `files` files, each with its own identity as a real scan gives it.
/// The first `conflicts` identities appear twice with different listed sizes, which is what makes
/// them conflicted and correctable.
fn conflicted_index(files: usize, conflicts: usize) -> Index {
    let mut index = Index::new("fixture".into()).unwrap();
    let root = index.root();
    index.set_state(root, State::Complete).unwrap();
    let folder = index
        .insert(
            root,
            "folder".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    for number in 0..files {
        let shared = number < conflicts;
        let listed = if shared { 100 } else { 10 };
        index
            .insert(
                folder,
                format!("file-{number:06}").into(),
                Kind::File,
                Sizes {
                    logical: Some(listed),
                    allocated: Some(4096),
                },
                Some(identity(number as u64)),
                State::Complete,
            )
            .unwrap();
        if shared {
            // A second name for the same identity with a different size: this is the conflict.
            index
                .insert(
                    folder,
                    format!("alias-{number:06}").into(),
                    Kind::File,
                    Sizes {
                        logical: Some(200),
                        allocated: Some(4096),
                    },
                    Some(identity(number as u64)),
                    State::Complete,
                )
                .unwrap();
        }
    }
    index
}

#[test]
fn corrected_sizes_reach_the_layout_total() {
    // The corrected path is the one that asks, per file, whether its identity was corrected. A
    // filter in front of that question would be wrong if it ever answered no for a corrected
    // identity, so this pins the answer: the corrected size must reach the layout total.
    let mut index = conflicted_index(SMALL_FILES, SMALL_CONFLICTS);
    assert_eq!(index.conflicting_identity_count(), SMALL_CONFLICTS);
    for number in 0..SMALL_CONFLICTS {
        index
            .apply_authoritative_sizes(
                identity(number as u64),
                Sizes {
                    logical: Some(150),
                    allocated: Some(8192),
                },
            )
            .unwrap();
    }
    let area = Rect::new(0.0, 0.0, 1920.0, 1080.0);
    let params = LayoutParams {
        min_pixel_area: 400.0,
        header_height: 20.0,
        max_depth: 2,
    };
    let placed = layout(&index, index.root(), area, params).unwrap();
    let expected = (SMALL_CONFLICTS as u64) * 150
        + (SMALL_CONFLICTS as u64) * 200
        + ((SMALL_FILES - SMALL_CONFLICTS) as u64) * 10;
    assert_eq!(
        placed.known_sum, expected,
        "corrected sizes must reach the layout total"
    );
}

#[test]
#[ignore = "measurement harness: builds a large index to time one cold layout"]
fn corrected_sizes_are_used_and_the_cold_layout_is_measured() {
    // Overridable so the same harness can tell whether the cost follows the number of conflicts
    // or the walk over every file. The defaults reuse the conflict count observed on a 3.8M-entry
    // volume, not its shape: this fixture is one flat directory of 200k files, so its figure is
    // comparable in order of magnitude only.
    let files = env_count("HOGMAP_LAYOUT_FILES", FILES);
    let conflicts = env_count("HOGMAP_LAYOUT_CONFLICTS", CONFLICTS).min(files);
    let mut index = conflicted_index(files, conflicts);
    assert_eq!(index.conflicting_identity_count(), conflicts);
    for number in 0..conflicts {
        index
            .apply_authoritative_sizes(
                identity(number as u64),
                Sizes {
                    logical: Some(150),
                    allocated: Some(8192),
                },
            )
            .unwrap();
    }
    let area = Rect::new(0.0, 0.0, 1920.0, 1080.0);
    let params = LayoutParams {
        min_pixel_area: 400.0,
        header_height: 20.0,
        max_depth: 2,
    };
    let started = std::time::Instant::now();
    let placed = layout(&index, index.root(), area, params).unwrap();
    let corrected = started.elapsed().as_secs_f64() * 1000.0;
    // The same index without corrections, to isolate what the corrected path adds. The layout
    // cache is per index, so a fresh index is the only honest way to time the cold path twice.
    let plain_index = conflicted_index(files, conflicts);
    let started = std::time::Instant::now();
    let plain = layout(&plain_index, plain_index.root(), area, params).unwrap();
    let uncorrected = started.elapsed().as_secs_f64() * 1000.0;
    // The two layouts are not expected to hold the same number of cells: a correction changes the
    // weights, and a weight near `min_pixel_area` can move a file between being drawn and being
    // grouped. Both counts are printed so a surprising difference is visible, and neither is
    // asserted.
    println!(
        "corrected_layout_ms={corrected:.3} uncorrected_layout_ms={uncorrected:.3} cells={} plain_cells={} conflicts={conflicts} files={files}",
        placed.placed.len(),
        plain.placed.len()
    );
    // Correctness: the corrected size reaches the layout total, so the fast path cannot skip the
    // lookup for an identity that was actually corrected. The summary counts listed sizes, so it
    // is not the oracle here; the layout total is.
    let expected =
        (conflicts as u64) * 150 + (conflicts as u64) * 200 + ((files - conflicts) as u64) * 10;
    assert_eq!(
        placed.known_sum, expected,
        "corrected sizes must reach the layout total"
    );
}
