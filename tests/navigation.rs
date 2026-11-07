#[path = "../src/ui/logic.rs"]
mod logic;

use std::ffi::OsStr;

use hogmap::filter::Match;
use hogmap::index::{Index, Kind, SizeMetric, Sizes, State};
use hogmap::treemap::{self, LayoutParams, NodeRef, Rect};
use logic::{HighlightFilter, History, Location, Members, fitting_prefix, hit_test};

fn fixture() -> (Index, hogmap::index::EntryId, hogmap::index::EntryId) {
    let mut index = Index::new("sample".into()).unwrap();
    let directory = index
        .insert(
            index.root(),
            "photos".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let file = index
        .insert(
            directory,
            "photo.jpg".into(),
            Kind::File,
            Sizes {
                logical: Some(2 * 1024 * 1024),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    index
        .insert(
            index.root(),
            "notes.txt".into(),
            Kind::File,
            Sizes {
                logical: Some(100),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    (index, directory, file)
}

#[test]
fn history_branches_and_resolves_names_in_a_new_snapshot() {
    let (index, directory, _) = fixture();
    let photos = Location::directory(&index, directory);
    let mut history = History::default();
    history.visit(photos.clone());
    assert!(history.can_back());
    history.back();
    assert_eq!(history.current(), Location::default());
    history.forward();
    assert_eq!(history.current(), photos);
    let mut fresh = Index::new("sample".into()).unwrap();
    fresh
        .insert(
            fresh.root(),
            "other".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let new_photos = fresh
        .insert(
            fresh.root(),
            "photos".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    assert_ne!(new_photos, directory);
    assert_eq!(history.current().resolve(&fresh), Some((new_photos, None)));
    assert_eq!(photos.up(), Location::default());
    history.back();
    history.visit(Location {
        path: vec!["other".into()],
        members: None,
    });
    assert!(!history.can_forward());
}

#[test]
fn aggregate_history_keeps_members_and_up_returns_to_parent() {
    let (index, directory, file) = fixture();
    let mut location = Location::directory(&index, directory);
    location.members = Some(Members {
        names: vec!["photo.jpg".into()],
        grouped_by: SizeMetric::Logical,
    });
    assert_eq!(
        location.resolve(&index),
        Some((directory, Some(vec![file])))
    );
    // A metric switch keeps the same names open but no longer calls them that metric's small items.
    assert!(location.members_grouped_by(SizeMetric::Logical));
    assert!(!location.members_grouped_by(SizeMetric::Allocated));
    assert_eq!(location.up(), Location::directory(&index, directory));
    assert!(
        Location {
            path: vec!["missing".into()],
            members: None
        }
        .resolve(&index)
        .is_none()
    );
}

#[test]
fn a_metric_switch_keeps_entries_but_only_aggregates_with_the_same_members() {
    let mut index = Index::new("sample".into()).unwrap();
    let root = index.root();
    let file = |index: &mut Index, parent, name: &str, logical, allocated| {
        index
            .insert(
                parent,
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(logical),
                    allocated: Some(allocated),
                },
                None,
                State::Complete,
            )
            .unwrap()
    };
    // Logically "big.bin" is large and the "small" files are grouped; by allocation it is the
    // reverse. The "uniform" folder weighs the same in both metrics.
    let big = file(&mut index, root, "big.bin", 1_000_000, 100);
    for number in 0..3 {
        file(
            &mut index,
            root,
            &format!("small_{number}.bin"),
            100,
            1_000_000,
        );
    }
    let uniform = index
        .insert(
            root,
            "uniform".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    file(&mut index, uniform, "large.bin", 1_000_000, 1_000_000);
    for number in 0..3 {
        file(&mut index, uniform, &format!("tiny_{number}.bin"), 100, 100);
    }
    // Both small files stay grouped in either metric, but each metric sorts them the other way.
    let swapped = index
        .insert(
            root,
            "swapped".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    file(&mut index, swapped, "large.bin", 1_000_000, 1_000_000);
    file(&mut index, swapped, "a.bin", 100, 200);
    file(&mut index, swapped, "b.bin", 200, 100);
    index.set_state(root, State::Complete).unwrap();
    let rect = Rect::new(0.0, 0.0, 1000.0, 1000.0);
    let params = LayoutParams {
        min_pixel_area: 5000.0,
        header_height: 0.0,
    };
    let logical =
        treemap::layout_with_metric(&index, root, rect, params, SizeMetric::Logical).unwrap();
    let allocated =
        treemap::layout_with_metric(&index, root, rect, params, SizeMetric::Allocated).unwrap();

    let changed = NodeRef::Aggregate { parent: root };
    let same = NodeRef::Aggregate { parent: uniform };
    let reordered = NodeRef::Aggregate { parent: swapped };
    let members = |layout: &treemap::Layout, node| {
        layout
            .placed
            .iter()
            .find(|cell| cell.node == node)
            .map(|cell| cell.members.clone())
            .unwrap_or_else(|| panic!("missing {node:?}"))
    };
    for node in [changed, same, reordered] {
        members(&logical, node);
        members(&allocated, node);
    }
    let (mut before, mut after) = (members(&logical, reordered), members(&allocated, reordered));
    assert_ne!(before, after, "the fixture must reorder the group");
    before.sort();
    after.sort();
    assert_eq!(before, after);
    // The same members in another order are still the same group.
    assert_eq!(
        logic::retained_selection(Some(reordered), Some(&logical), Some(&allocated)),
        Some(reordered)
    );
    // Same parent, different members: the selection would point at another group.
    assert_eq!(
        logic::retained_selection(Some(changed), Some(&logical), Some(&allocated)),
        None
    );
    assert_eq!(
        logic::retained_selection(Some(same), Some(&logical), Some(&allocated)),
        Some(same)
    );
    // An entry stays selected even when the new metric places it inside an aggregate.
    assert_eq!(
        logic::retained_selection(Some(NodeRef::Entry(big)), Some(&logical), Some(&allocated)),
        Some(NodeRef::Entry(big))
    );
    // Without a new layout (it failed), no aggregate can be confirmed.
    assert_eq!(
        logic::retained_selection(Some(same), Some(&logical), None),
        None
    );
}

#[test]
fn an_overflowed_metric_offers_the_other_only_when_its_totals_exist() {
    let mut index = Index::new("sample".into()).unwrap();
    for (name, logical, allocated) in [("max.bin", 10, u64::MAX), ("one.bin", 20, 1)] {
        index
            .insert(
                index.root(),
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(logical),
                    allocated: Some(allocated),
                },
                None,
                State::Complete,
            )
            .unwrap();
    }
    assert_eq!(
        logic::metric_totals(&index, SizeMetric::Logical)
            .unwrap()
            .known_bytes,
        30
    );
    let overflow = logic::metric_totals(&index, SizeMetric::Allocated).unwrap_err();
    assert_eq!(overflow.metric, SizeMetric::Allocated);
    assert!(overflow.other_available);
}

#[test]
fn hit_testing_prefers_descendants_and_excludes_outside_edges() {
    let (index, _, file) = fixture();
    let layout = treemap::layout(
        &index,
        index.root(),
        Rect::new(0.0, 0.0, 200.0, 100.0),
        LayoutParams {
            min_pixel_area: 1.0,
            header_height: 10.0,
        },
    )
    .unwrap();
    let cell = layout
        .placed
        .iter()
        .find(|cell| cell.node == NodeRef::Entry(file))
        .unwrap();
    let hit = hit_test(&layout, cell.rect.x + 1.0, cell.rect.y + 1.0).unwrap();
    assert_eq!(layout.placed[hit].node, NodeRef::Entry(file));
    assert_eq!(hit_test(&layout, 200.0, 50.0), None);
    assert_eq!(hit_test(&layout, f32::NAN, 1.0), None);
}

#[test]
fn labels_reserve_ellipsis_and_never_split_a_character() {
    assert_eq!(
        fitting_prefix(&[7.0, 7.0, 7.0], 21.0, 5.0),
        Some((3, false))
    );
    assert_eq!(fitting_prefix(&[7.0, 7.0, 7.0], 19.0, 5.0), Some((2, true)));
    assert_eq!(fitting_prefix(&[7.0], 4.0, 5.0), None);
    assert_eq!(fitting_prefix(&[7.0], 0.0, 5.0), None);
}

#[test]
fn match_counter_is_hidden_without_filter_text() {
    assert!(!logic::show_match_count(""));
    // The parser treats whitespace-only input as the default (inactive) filter.
    assert!(!logic::show_match_count(" \t "));
    assert!(logic::show_match_count("*.bin"));
    assert!(logic::show_match_count(" >1mb "));
    assert!(logic::show_match_count(">")); // Invalid input retains the previous highlight.
}

#[test]
fn highlighting_keeps_context_totals_and_previous_valid_filter() {
    let (index, directory, file) = fixture();
    let summary = index.summary().unwrap();
    let mut filter = HighlightFilter::default();
    assert!(filter.set("*.jpg;>1mb"));
    let matches = filter.matches(&index);
    assert_eq!(matches[directory.position()], Match::Yes);
    assert_eq!(
        logic::highlight(NodeRef::Entry(file), &[], &matches),
        Match::Yes
    );
    let notes = index
        .entries()
        .find(|(_, entry)| entry.name == OsStr::new("notes.txt"))
        .unwrap()
        .0;
    assert_eq!(
        logic::highlight(NodeRef::Entry(notes), &[], &matches),
        Match::No
    );
    assert_eq!(
        logic::highlight(
            NodeRef::Aggregate {
                parent: index.root()
            },
            &[notes, file],
            &matches
        ),
        Match::Yes
    );
    assert!(!filter.set(">1.5mb"));
    assert!(filter.error.is_some());
    assert_eq!(filter.matches(&index), matches);
    assert_eq!(index.summary().unwrap(), summary);
}

#[test]
fn lifecycle_states_follow_the_terminal_before_cancellation() {
    use hogmap::scan::Terminal;
    use logic::{ScanState, scan_state};
    assert_eq!(scan_state(true, false, None), ScanState::Scanning);
    assert_eq!(
        scan_state(false, false, Some(Terminal::Finished)),
        ScanState::Finished
    );
    assert_eq!(scan_state(true, true, None), ScanState::Cancelled);
    // Cancel can land after the producer queued its terminal: the terminal still wins.
    assert_eq!(
        scan_state(true, true, Some(Terminal::Finished)),
        ScanState::Finished
    );
    assert_eq!(
        scan_state(true, false, Some(Terminal::Failed(7))),
        ScanState::Failed
    );
    assert_eq!(
        scan_state(false, false, Some(Terminal::Failed(7))),
        ScanState::Failed
    );
    assert_eq!(scan_state(false, false, None), ScanState::Failed);
}

#[test]
fn unknown_filter_decisions_are_not_non_matches() {
    let mut index = Index::new("sample".into()).unwrap();
    let file = index
        .insert(
            index.root(),
            "photo.jpg".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let mut filter = HighlightFilter::default();
    filter.set(">1mb");
    let matches = filter.matches(&index);
    assert_eq!(matches[file.position()], Match::Unknown);
    assert_eq!(matches[index.root().position()], Match::Unknown);
}
