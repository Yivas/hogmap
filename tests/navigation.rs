#[path = "../src/ui/logic.rs"]
mod logic;

use std::ffi::OsStr;

use hogmap::filter::Match;
use hogmap::index::{Index, Kind, Sizes, State};
use hogmap::treemap::{self, LayoutParams, NodeRef, Rect};
use logic::{HighlightFilter, History, Location, fitting_prefix, hit_test};

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
    location.members = Some(vec!["photo.jpg".into()]);
    assert_eq!(
        location.resolve(&index),
        Some((directory, Some(vec![file])))
    );
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
    assert_eq!(matches[&directory], Match::Yes);
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
    assert_eq!(matches[&file], Match::Unknown);
    assert_eq!(matches[&index.root()], Match::Unknown);
}
