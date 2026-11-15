use hogmap::filter::{ClassTable, Filter};
use hogmap::index::{Index, Kind, SizeMetric, Sizes, State};
use hogmap::treemap::{self, GroupBy, LayoutParams, NodeRef, Rect};

fn fixture() -> Index {
    let mut index = Index::new("Fixture".into()).unwrap();
    index.set_state(index.root(), State::Complete).unwrap();
    let folder = index
        .insert(
            index.root(),
            "folder".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    for (parent, name, bytes) in [
        (folder, "nested.bin", Some(40)),
        (index.root(), "photo.JPG", Some(60)),
        (index.root(), "unknown.jpg", None),
        (index.root(), "readme.txt", Some(20)),
        (index.root(), "plain", Some(10)),
    ] {
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
            .unwrap();
    }
    index
}
fn params(depth: usize) -> LayoutParams {
    LayoutParams {
        min_pixel_area: f32::MIN_POSITIVE,
        header_height: 20.0,
        max_depth: depth,
    }
}

#[test]
fn flat_and_nested_keep_child_weights_unknowns_and_filtered_totals_but_change_composition() {
    let index = fixture();
    let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
    for filtered in [false, true] {
        let matches = Filter::parse("*.jpg;*.bin")
            .unwrap()
            .matches(&index)
            .matches;
        let draw = |depth| {
            if filtered {
                treemap::layout_filtered(
                    &index,
                    index.root(),
                    None,
                    rect,
                    params(depth),
                    SizeMetric::Logical,
                    &matches,
                )
                .unwrap()
            } else {
                treemap::layout_with_metric(
                    &index,
                    index.root(),
                    rect,
                    params(depth),
                    SizeMetric::Logical,
                )
                .unwrap()
            }
        };
        let nested = draw(64);
        let flat = draw(1);
        assert_eq!(
            (flat.known_sum, flat.has_unknown),
            (if filtered { 100 } else { 130 }, true)
        );
        assert_eq!(
            (nested.known_sum, nested.has_unknown),
            (flat.known_sum, flat.has_unknown)
        );
        assert_eq!(nested.placed.len(), flat.placed.len() + 1);
        for cell in &flat.placed {
            let corresponding = nested
                .placed
                .iter()
                .find(|candidate| candidate.node == cell.node)
                .unwrap();
            assert_eq!(
                (cell.known_sum, cell.has_unknown, &cell.members),
                (
                    corresponding.known_sum,
                    corresponding.has_unknown,
                    &corresponding.members
                )
            );
        }
        assert!(nested.placed.iter().any(|cell| matches!(cell.node, NodeRef::Entry(id) if index.get(id).unwrap().name == "nested.bin")));
        assert!(!flat.placed.iter().any(|cell| matches!(cell.node, NodeRef::Entry(id) if index.get(id).unwrap().name == "nested.bin")));
    }
}

#[test]
fn extension_and_class_groups_have_exact_members_sizes_unknowns_and_no_filtered_empty_groups() {
    let index = fixture();
    let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
    let classes = ClassTable::default();
    for (by, expected) in [
        (
            GroupBy::Extension,
            vec![
                (".jpg", 2, 60, true),
                (".txt", 1, 20, false),
                ("Other", 2, 50, false),
            ],
        ),
        (
            GroupBy::Class,
            vec![
                ("Other", 2, 50, false),
                ("documents", 1, 20, false),
                ("images", 2, 60, true),
            ],
        ),
    ] {
        let layout = treemap::layout_grouped(
            &index,
            index.root(),
            rect,
            params(64),
            SizeMetric::Logical,
            (&classes, by),
            None,
        )
        .unwrap();
        let groups: Vec<_> = layout
            .placed
            .iter()
            .skip(1)
            .map(|cell| {
                (
                    cell.group_name.as_deref().unwrap(),
                    cell.members.len(),
                    cell.known_sum,
                    cell.has_unknown,
                )
            })
            .collect();
        assert_eq!(groups, expected);
        assert_eq!((layout.known_sum, layout.has_unknown), (130, true));
        let matches = Filter::parse("*.jpg").unwrap().matches(&index).matches;
        let filtered = treemap::layout_grouped(
            &index,
            index.root(),
            rect,
            params(1),
            SizeMetric::Logical,
            (&classes, by),
            Some(&matches),
        )
        .unwrap();
        assert_eq!(filtered.placed.len(), 2);
        assert_eq!(
            filtered.placed[1].group_name.as_deref(),
            Some(if by == GroupBy::Extension {
                ".jpg"
            } else {
                "images"
            })
        );
        assert_eq!(
            (
                filtered.placed[1].members.len(),
                filtered.placed[1].known_sum,
                filtered.placed[1].has_unknown
            ),
            (2, 60, true)
        );
        let opened = treemap::layout_filtered(
            &index,
            index.root(),
            Some(&filtered.placed[1].members),
            rect,
            params(1),
            SizeMetric::Logical,
            &matches,
        )
        .unwrap();
        assert_eq!((opened.known_sum, opened.has_unknown), (60, true));
    }
}
