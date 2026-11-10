use std::collections::HashMap;
use std::f32::consts::PI;

use hogmap::index::{EntryId, FileIdentity, Index, Kind, SizeMetric, Sizes, State};
use hogmap::treemap::{
    Layout, LayoutError, LayoutParams, NodeRef, Placed, Rect, layout, layout_members_with_metric,
    layout_with_metric,
};

fn params(min_pixel_area: f32, header_height: f32) -> LayoutParams {
    LayoutParams {
        min_pixel_area,
        header_height,
        max_depth: usize::MAX,
    }
}

fn insert_dir(index: &mut Index, parent: EntryId, name: &str) -> EntryId {
    index
        .insert(
            parent,
            name.into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap()
}

fn insert_file(index: &mut Index, parent: EntryId, name: &str, bytes: Option<u64>) -> EntryId {
    index
        .insert(
            parent,
            name.into(),
            Kind::File,
            Sizes {
                logical: bytes,
                allocated: bytes,
            },
            None,
            State::Complete,
        )
        .unwrap()
}

fn insert_sized_file(index: &mut Index, parent: EntryId, name: &str, sizes: Sizes) -> EntryId {
    index
        .insert(
            parent,
            name.into(),
            Kind::File,
            sizes,
            None,
            State::Complete,
        )
        .unwrap()
}

fn insert_linked_file(
    index: &mut Index,
    parent: EntryId,
    name: &str,
    bytes: u64,
    identity: FileIdentity,
) -> EntryId {
    index
        .insert(
            parent,
            name.into(),
            Kind::File,
            Sizes {
                logical: Some(bytes),
                allocated: Some(bytes),
            },
            Some(identity),
            State::Complete,
        )
        .unwrap()
}

#[test]
fn repeated_layout_tracks_append_states_metrics_resolution_and_members() {
    let mut index = Index::new("Cache fixture".into()).unwrap();
    let root = index.root();
    index.set_state(root, State::Complete).unwrap();
    let identity = FileIdentity { volume: 1, file: 7 };
    let first = index
        .insert(
            root,
            "first".into(),
            Kind::File,
            Sizes {
                logical: Some(10),
                allocated: Some(20),
            },
            Some(identity),
            State::Complete,
        )
        .unwrap();
    let draw = |index: &Index, metric| {
        layout_with_metric(
            index,
            root,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            params(1.0, 0.0),
            metric,
        )
        .unwrap()
    };
    assert_eq!(draw(&index, SizeMetric::Logical).known_sum, 10);
    let pending = index
        .insert(
            root,
            "pending".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Pending,
        )
        .unwrap();
    index
        .insert(
            pending,
            "unknown allocation".into(),
            Kind::File,
            Sizes {
                logical: Some(5),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    let growing = draw(&index, SizeMetric::Logical);
    assert_eq!(growing.known_sum, 15);
    assert!(growing.has_unknown);
    index.set_state(pending, State::Complete).unwrap();
    assert!(!draw(&index, SizeMetric::Logical).has_unknown);
    let allocated = draw(&index, SizeMetric::Allocated);
    assert_eq!(allocated.known_sum, 20);
    assert!(allocated.has_unknown);
    assert_eq!(draw(&index, SizeMetric::Logical).known_sum, 15);
    index.set_state(first, State::Stale).unwrap();
    assert!(draw(&index, SizeMetric::Logical).has_unknown);
    index.set_state(first, State::Complete).unwrap();
    assert!(!draw(&index, SizeMetric::Logical).has_unknown);
    index
        .insert(
            root,
            "alias".into(),
            Kind::File,
            Sizes {
                logical: Some(30),
                allocated: Some(40),
            },
            Some(identity),
            State::Complete,
        )
        .unwrap();
    assert_eq!(draw(&index, SizeMetric::Logical).known_sum, 45);
    index
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(12),
                allocated: Some(24),
            },
        )
        .unwrap();
    assert_eq!(draw(&index, SizeMetric::Logical).known_sum, 47);
    assert_eq!(draw(&index, SizeMetric::Allocated).known_sum, 64);
    assert_eq!(
        index.insert(
            root,
            "sealed".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete
        ),
        Err(hogmap::index::IndexError::ResolvedSnapshot)
    );
    let subset = layout_members_with_metric(
        &index,
        root,
        &[first],
        Rect::new(0.0, 0.0, 100.0, 100.0),
        params(1.0, 0.0),
        SizeMetric::Logical,
    )
    .unwrap();
    assert_eq!(subset.known_sum, 12);
    assert_eq!(draw(&index, SizeMetric::Logical).known_sum, 47);
}

#[test]
fn depth_limit_counts_levels_below_the_drawn_root_including_zero() {
    let mut index = Index::new("Depth fixture".into()).unwrap();
    let root = index.root();
    let first = insert_dir(&mut index, root, "first");
    let second = insert_dir(&mut index, first, "second");
    let third = insert_dir(&mut index, second, "third");
    insert_file(&mut index, third, "leaf", Some(7));
    index.set_state(root, State::Complete).unwrap();
    for (max_depth, expected_cells) in [(0, 1), (2, 3), (usize::MAX, 5)] {
        let result = layout(
            &index,
            root,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            LayoutParams {
                min_pixel_area: 1.0,
                header_height: 0.0,
                max_depth,
            },
        )
        .unwrap();
        assert_eq!(result.known_sum, 7);
        assert!(!result.has_unknown);
        assert_eq!(result.placed.len(), expected_cells, "depth={max_depth}");
    }
}

struct Basic {
    index: Index,
    root: EntryId,
    docs: EntryId,
    media: EntryId,
    doc_a: EntryId,
    doc_b: EntryId,
    media_file: EntryId,
    empty: EntryId,
    unknown: EntryId,
}

fn basic() -> Basic {
    let mut index = Index::new("Volume".into()).unwrap();
    let root = index.root();
    let docs = insert_dir(&mut index, root, "Docs");
    let media = insert_dir(&mut index, root, "Media");
    let doc_a = insert_file(&mut index, docs, "a.bin", Some(60));
    let doc_b = insert_file(&mut index, docs, "b.bin", Some(30));
    let media_file = insert_file(&mut index, media, "c.bin", Some(10));
    let empty = insert_file(&mut index, root, "empty.bin", Some(0));
    let unknown = index
        .insert(
            docs,
            "mystery.bin".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Inaccessible,
        )
        .unwrap();
    index.set_state(root, State::Complete).unwrap();
    Basic {
        index,
        root,
        docs,
        media,
        doc_a,
        doc_b,
        media_file,
        empty,
        unknown,
    }
}

fn find(layout: &Layout, node: NodeRef) -> &Placed {
    layout
        .placed
        .iter()
        .find(|placed| placed.node == node)
        .unwrap_or_else(|| panic!("missing {node:?}"))
}

fn parent_of(index: &Index, placed: &Placed) -> Option<EntryId> {
    match placed.node {
        NodeRef::Entry(id) => index.get(id).and_then(|entry| entry.parent),
        NodeRef::Aggregate { parent } => Some(parent),
    }
}

fn contains(outer: Rect, inner: Rect, epsilon: f32) -> bool {
    inner.x >= outer.x - epsilon
        && inner.y >= outer.y - epsilon
        && inner.right() <= outer.right() + epsilon
        && inner.bottom() <= outer.bottom() + epsilon
}

fn assert_proportional(rect: Rect, content_area: f32, weight: u64, total: u64) {
    let expected = content_area * weight as f32 / total as f32;
    let tolerance = content_area.max(1.0) * 0.01;
    assert!(
        (rect.area() - expected).abs() <= tolerance,
        "{:?} should be near {expected}, got {}",
        rect,
        rect.area()
    );
}

#[test]
fn layout_contains_every_child_inside_its_parent_content() {
    let basic = basic();
    let result = layout(
        &basic.index,
        basic.root,
        Rect::new(0.0, 0.0, 800.0, 600.0),
        params(16.0, 20.0),
    )
    .unwrap();
    for placed in &result.placed {
        if let Some(parent) = parent_of(&basic.index, placed) {
            let parent_content = find(&result, NodeRef::Entry(parent)).content;
            assert!(
                contains(parent_content, placed.rect, 0.1),
                "{:?} escapes {:?}",
                placed.node,
                parent_content
            );
        }
    }
}

#[test]
fn layout_keeps_siblings_disjoint() {
    let basic = basic();
    let result = layout(
        &basic.index,
        basic.root,
        Rect::new(0.0, 0.0, 800.0, 600.0),
        params(16.0, 20.0),
    )
    .unwrap();
    let mut groups: HashMap<Option<EntryId>, Vec<&Placed>> = HashMap::new();
    for placed in &result.placed {
        if let Some(parent) = parent_of(&basic.index, placed) {
            groups.entry(Some(parent)).or_default().push(placed);
        }
    }
    for siblings in groups.values() {
        for (index, a) in siblings.iter().enumerate() {
            for b in &siblings[index + 1..] {
                let separated = a.rect.right() <= b.rect.x + 0.1
                    || b.rect.right() <= a.rect.x + 0.1
                    || a.rect.bottom() <= b.rect.y + 0.1
                    || b.rect.bottom() <= a.rect.y + 0.1;
                assert!(separated, "{:?} overlaps {:?}", a.rect, b.rect);
            }
        }
    }
}

#[test]
fn layout_conserves_each_expanded_content_area() {
    let basic = basic();
    let result = layout(
        &basic.index,
        basic.root,
        Rect::new(0.0, 0.0, 800.0, 600.0),
        params(16.0, 20.0),
    )
    .unwrap();
    let mut child_areas: HashMap<EntryId, f32> = HashMap::new();
    for placed in &result.placed {
        if let Some(parent) = parent_of(&basic.index, placed) {
            *child_areas.entry(parent).or_default() += placed.rect.area();
        }
    }
    for (parent, sum) in child_areas {
        let content = find(&result, NodeRef::Entry(parent)).content;
        let tolerance = content.area().max(1.0) * 0.01;
        assert!(
            (sum - content.area()).abs() <= tolerance,
            "children of {parent:?} sum {sum}, content is {}",
            content.area()
        );
    }
}

#[test]
fn layout_makes_cells_proportional_to_known_bytes() {
    let basic = basic();
    let result = layout(
        &basic.index,
        basic.root,
        Rect::new(0.0, 0.0, 1000.0, 1000.0),
        params(1.0, 0.0),
    )
    .unwrap();
    let root_content = find(&result, NodeRef::Entry(basic.root)).content;
    assert_proportional(
        find(&result, NodeRef::Entry(basic.docs)).rect,
        root_content.area(),
        90,
        100,
    );
    assert_proportional(
        find(&result, NodeRef::Entry(basic.media)).rect,
        root_content.area(),
        10,
        100,
    );
    assert_eq!(
        find(&result, NodeRef::Entry(basic.media_file)).known_sum,
        10
    );
    let docs_content = find(&result, NodeRef::Entry(basic.docs)).content;
    assert_proportional(
        find(&result, NodeRef::Entry(basic.doc_a)).rect,
        docs_content.area(),
        60,
        90,
    );
    assert_proportional(
        find(&result, NodeRef::Entry(basic.doc_b)).rect,
        docs_content.area(),
        30,
        90,
    );
}

#[test]
fn layout_is_deterministic() {
    let basic = basic();
    let first = layout(
        &basic.index,
        basic.root,
        Rect::new(10.0, 20.0, 640.0, 480.0),
        params(24.0, 18.0),
    )
    .unwrap();
    let second = layout(
        &basic.index,
        basic.root,
        Rect::new(10.0, 20.0, 640.0, 480.0),
        params(24.0, 18.0),
    )
    .unwrap();
    assert_eq!(first, second);
}

#[test]
fn layout_aggregates_small_siblings_and_preserves_totals() {
    let mut index = Index::new("Volume".into()).unwrap();
    let root = index.root();
    insert_file(&mut index, root, "big.bin", Some(1_000_000));
    for number in 0..100 {
        insert_linked_file(
            &mut index,
            root,
            &format!("small_{number:03}.bin"),
            100,
            FileIdentity { volume: 1, file: 7 },
        );
    }
    index.set_state(root, State::Complete).unwrap();
    let result = layout(
        &index,
        root,
        Rect::new(0.0, 0.0, 1000.0, 1000.0),
        params(5000.0, 0.0),
    )
    .unwrap();
    assert_eq!(result.known_sum, 1_010_000);
    let aggregate = find(&result, NodeRef::Aggregate { parent: root });
    assert_eq!(aggregate.known_sum, 10_000);
    assert_eq!(aggregate.members.len(), 100);
    assert!(aggregate.members.iter().all(|id| {
        index
            .get(*id)
            .is_some_and(|entry| entry.name.to_string_lossy().starts_with("small_"))
    }));
    let root_content = find(&result, NodeRef::Entry(root)).content;
    let mut child_area = 0.0;
    let mut child_weight = 0;
    for placed in &result.placed {
        if parent_of(&index, placed) == Some(root) {
            child_area += placed.rect.area();
            child_weight += placed.known_sum;
        }
    }
    assert!((child_area - root_content.area()).abs() <= root_content.area() * 0.01);
    assert_eq!(child_weight, result.known_sum);
    assert_eq!(result.placed.len(), 3);
}

#[test]
fn layout_reports_unknown_sizes_without_giving_them_area() {
    let basic = basic();
    let result = layout(
        &basic.index,
        basic.root,
        Rect::new(0.0, 0.0, 800.0, 600.0),
        params(16.0, 20.0),
    )
    .unwrap();
    assert!(result.has_unknown);
    assert!(find(&result, NodeRef::Entry(basic.root)).has_unknown);
    assert!(find(&result, NodeRef::Entry(basic.docs)).has_unknown);
    assert!(!find(&result, NodeRef::Entry(basic.media)).has_unknown);
    assert!(result.placed.iter().all(|placed| {
        placed.node != NodeRef::Entry(basic.unknown) && placed.node != NodeRef::Entry(basic.empty)
    }));
    let mut placed_weight = 0;
    for placed in &result.placed {
        if let NodeRef::Entry(id) = placed.node
            && basic
                .index
                .get(id)
                .is_some_and(|entry| entry.kind == Kind::File)
        {
            placed_weight += placed.known_sum;
        }
    }
    assert_eq!(placed_weight, result.known_sum);
}

#[test]
fn layout_reserves_a_header_and_collapses_it_when_it_does_not_fit() {
    let basic = basic();
    let roomy = layout(
        &basic.index,
        basic.root,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        params(1.0, 20.0),
    )
    .unwrap();
    let root = find(&roomy, NodeRef::Entry(basic.root));
    let header = root.header.expect("root header fits");
    assert_eq!(header.height, 20.0);
    assert_eq!(root.content.height, 80.0);
    assert_eq!(root.content.y, 20.0);
    let docs = find(&roomy, NodeRef::Entry(basic.docs));
    assert!(docs.header.is_some());

    let tight = layout(
        &basic.index,
        basic.root,
        Rect::new(0.0, 0.0, 100.0, 30.0),
        params(1.0, 20.0),
    )
    .unwrap();
    let root = find(&tight, NodeRef::Entry(basic.root));
    assert!(root.header.is_none());
    assert_eq!(root.content, root.rect);
    let docs = find(&tight, NodeRef::Entry(basic.docs));
    assert!(docs.header.is_none());
    assert_eq!(docs.content, docs.rect);
}

#[test]
fn layout_handles_a_deep_tree_without_recursion() {
    let mut index = Index::new("Volume".into()).unwrap();
    let mut parent = index.root();
    for number in 0..10_000 {
        parent = insert_dir(&mut index, parent, &format!("d{number}"));
    }
    insert_file(&mut index, parent, "leaf.bin", Some(1));
    index.set_state(index.root(), State::Complete).unwrap();
    let result = layout(
        &index,
        index.root(),
        Rect::new(0.0, 0.0, 1000.0, 1000.0),
        params(1.0, 0.0),
    )
    .unwrap();
    assert_eq!(result.known_sum, 1);
    assert_eq!(result.placed.len(), 10_002);
}

#[test]
fn layout_rejects_invalid_geometry_and_foreign_roots() {
    let basic = basic();
    let rect = Rect::new(0.0, 0.0, 100.0, 100.0);
    assert_eq!(
        layout(
            &basic.index,
            basic.root,
            Rect::new(0.0, 0.0, -1.0, 100.0),
            params(1.0, 0.0)
        ),
        Err(LayoutError::InvalidRect)
    );
    assert_eq!(
        layout(
            &basic.index,
            basic.root,
            Rect::new(0.0, 0.0, PI, f32::NAN),
            params(1.0, 0.0)
        ),
        Err(LayoutError::InvalidRect)
    );
    assert_eq!(
        layout(
            &basic.index,
            basic.root,
            Rect::new(0.0, 0.0, f32::MAX, f32::MAX),
            params(1.0, 0.0)
        ),
        Err(LayoutError::InvalidRect)
    );
    assert_eq!(
        layout(&basic.index, basic.root, rect, params(0.0, 0.0)),
        Err(LayoutError::InvalidParams)
    );
    let fresh = Index::new("Other".into()).unwrap();
    assert_eq!(
        layout(&fresh, basic.doc_a, rect, params(1.0, 0.0)),
        Err(LayoutError::UnknownRoot)
    );
}

#[test]
fn layout_gives_each_hardlink_name_its_own_area() {
    let mut index = Index::new("Volume".into()).unwrap();
    let root = index.root();
    let first = insert_dir(&mut index, root, "First");
    let second = insert_dir(&mut index, root, "Second");
    let identity = FileIdentity {
        volume: 1,
        file: 42,
    };
    let original = insert_linked_file(&mut index, first, "one.bin", 100, identity);
    let link = insert_linked_file(&mut index, second, "two.bin", 100, identity);
    index.set_state(root, State::Complete).unwrap();
    let summary = index.summary().unwrap();
    assert_eq!(summary.known_logical_bytes, 200);
    assert_eq!(summary.unique_logical_bytes, 100);
    assert_eq!(summary.shared_logical_bytes, 100);

    let result = layout(
        &index,
        root,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        params(1.0, 0.0),
    )
    .unwrap();
    assert_eq!(result.known_sum, 200);
    assert_eq!(
        result,
        layout(
            &index,
            root,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            params(1.0, 0.0)
        )
        .unwrap()
    );
    for id in [first, second, original, link] {
        assert_eq!(find(&result, NodeRef::Entry(id)).known_sum, 100);
    }
    assert_proportional(find(&result, NodeRef::Entry(first)).rect, 10000.0, 100, 200);
    assert_proportional(
        find(&result, NodeRef::Entry(second)).rect,
        10000.0,
        100,
        200,
    );
    let zoomed = layout(
        &index,
        second,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        params(1.0, 0.0),
    )
    .unwrap();
    assert_eq!(zoomed.known_sum, 100);
    assert_eq!(find(&zoomed, NodeRef::Entry(link)).known_sum, 100);
}

#[test]
fn unknown_hardlink_sizes_have_no_area_without_hiding_known_names() {
    let mut index = Index::new("Volume".into()).unwrap();
    let root = index.root();
    let first = insert_dir(&mut index, root, "First");
    let second = insert_dir(&mut index, root, "Second");
    let identity = Some(FileIdentity {
        volume: 1,
        file: 43,
    });
    let mut link = |parent, name: &str, allocated| {
        index
            .insert(
                parent,
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(100),
                    allocated,
                },
                identity,
                State::Complete,
            )
            .unwrap()
    };
    let original = link(first, "one.bin", None);
    let later = link(second, "two.bin", Some(4096));
    index.set_state(root, State::Complete).unwrap();
    let rect = Rect::new(0.0, 0.0, 100.0, 100.0);

    let logical = layout(&index, root, rect, params(1.0, 0.0)).unwrap();
    assert_eq!(find(&logical, NodeRef::Entry(original)).known_sum, 100);
    assert_eq!(find(&logical, NodeRef::Entry(later)).known_sum, 100);
    assert_eq!(logical.known_sum, 200);

    // The first link has no allocated size, so the later one carries the allocated charge.
    let allocated =
        layout_with_metric(&index, root, rect, params(1.0, 0.0), SizeMetric::Allocated).unwrap();
    assert_eq!(allocated.known_sum, 4096);
    assert!(allocated.has_unknown);
    assert_eq!(find(&allocated, NodeRef::Entry(second)).known_sum, 4096);
    assert_eq!(find(&allocated, NodeRef::Entry(later)).known_sum, 4096);
    for absent in [original, first] {
        assert!(
            allocated
                .placed
                .iter()
                .all(|placed| placed.node != NodeRef::Entry(absent))
        );
    }
}

#[test]
fn apparent_overflow_is_reported_even_when_unique_bytes_fit() {
    for corrected in [false, true] {
        let mut index = Index::new("Synthetic".into()).unwrap();
        let root = index.root();
        let identity = FileIdentity {
            volume: 1,
            file: 99,
        };
        insert_linked_file(
            &mut index,
            root,
            "charged",
            if corrected { 1 } else { u64::MAX },
            identity,
        );
        insert_linked_file(
            &mut index,
            root,
            "alias",
            if corrected { u64::MAX - 1 } else { 1 },
            identity,
        );
        if corrected {
            index
                .apply_authoritative_sizes(
                    identity,
                    Sizes {
                        logical: Some(2),
                        allocated: Some(2),
                    },
                )
                .unwrap();
            let summary = index.summary_for(SizeMetric::Logical).unwrap();
            assert_eq!(summary.unique_bytes, 2);
            assert_eq!(
                summary.apparent_bytes(),
                Err(hogmap::index::IndexError::SizeOverflow)
            );
        }
        for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
            assert_eq!(
                layout_with_metric(
                    &index,
                    root,
                    Rect::new(0.0, 0.0, 100.0, 100.0),
                    params(1.0, 0.0),
                    metric
                ),
                Err(LayoutError::SizeOverflow)
            );
        }
    }
}

#[test]
fn layout_weighs_the_selected_metric_and_keeps_the_logical_path() {
    let mut index = Index::new("Volume".into()).unwrap();
    let root = index.root();
    let big = insert_sized_file(
        &mut index,
        root,
        "big.bin",
        Sizes {
            logical: Some(1_000_000),
            allocated: Some(100),
        },
    );
    let smalls: Vec<EntryId> = (0..3)
        .map(|number| {
            insert_sized_file(
                &mut index,
                root,
                &format!("small_{number}.bin"),
                Sizes {
                    logical: Some(100),
                    allocated: Some(1_000_000),
                },
            )
        })
        .collect();
    index.set_state(root, State::Complete).unwrap();
    let rect = Rect::new(0.0, 0.0, 1000.0, 1000.0);

    let logical =
        layout_with_metric(&index, root, rect, params(5000.0, 0.0), SizeMetric::Logical).unwrap();
    assert_eq!(
        logical,
        layout(&index, root, rect, params(5000.0, 0.0)).unwrap()
    );
    assert_eq!(logical.known_sum, 1_000_300);
    let logical_aggregate = find(&logical, NodeRef::Aggregate { parent: root });
    assert_eq!(logical_aggregate.known_sum, 300);
    assert_eq!(logical_aggregate.members, smalls);
    assert_proportional(
        find(&logical, NodeRef::Entry(big)).rect,
        logical.content.area(),
        1_000_000,
        1_000_300,
    );

    let allocated = layout_with_metric(
        &index,
        root,
        rect,
        params(5000.0, 0.0),
        SizeMetric::Allocated,
    )
    .unwrap();
    assert_eq!(allocated.known_sum, 3_000_100);
    let allocated_aggregate = find(&allocated, NodeRef::Aggregate { parent: root });
    assert_eq!(allocated_aggregate.known_sum, 100);
    assert_eq!(allocated_aggregate.members, vec![big]);
    for id in &smalls {
        assert_proportional(
            find(&allocated, NodeRef::Entry(*id)).rect,
            allocated.content.area(),
            1_000_000,
            3_000_100,
        );
    }

    // Opening the logical aggregate's members re-weighs the same IDs by the chosen metric.
    let logical_detail = layout_members_with_metric(
        &index,
        root,
        &logical_aggregate.members,
        rect,
        params(1.0, 0.0),
        SizeMetric::Logical,
    )
    .unwrap();
    assert_eq!(logical_detail.known_sum, 300);
    let allocated_detail = layout_members_with_metric(
        &index,
        root,
        &logical_aggregate.members,
        rect,
        params(1.0, 0.0),
        SizeMetric::Allocated,
    )
    .unwrap();
    assert_eq!(allocated_detail.known_sum, 3_000_000);
    assert_eq!(allocated_detail.placed.len(), 4);
}

#[test]
fn an_unknown_size_in_the_selected_metric_has_no_area_but_the_other_metric_places_it() {
    let mut index = Index::new("Volume".into()).unwrap();
    let root = index.root();
    let file = insert_sized_file(
        &mut index,
        root,
        "fallback.bin",
        Sizes {
            logical: Some(4096),
            allocated: None,
        },
    );
    index.set_state(root, State::Complete).unwrap();
    let rect = Rect::new(0.0, 0.0, 100.0, 100.0);

    let logical = layout(&index, root, rect, params(1.0, 0.0)).unwrap();
    assert!(!logical.has_unknown);
    assert_eq!(logical.known_sum, 4096);
    assert!(
        logical
            .placed
            .iter()
            .any(|placed| placed.node == NodeRef::Entry(file))
    );

    let allocated =
        layout_with_metric(&index, root, rect, params(1.0, 0.0), SizeMetric::Allocated).unwrap();
    assert!(allocated.has_unknown);
    assert_eq!(allocated.known_sum, 0);
    assert!(
        allocated
            .placed
            .iter()
            .all(|placed| placed.node != NodeRef::Entry(file)),
        "an unknown allocated size gets no area and is not zero"
    );
    assert_eq!(index.get(file).unwrap().sizes.allocated, None);
}
