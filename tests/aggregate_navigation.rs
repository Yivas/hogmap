use hogmap::index::{Index, Kind, Sizes, State};
use hogmap::treemap::{self, LayoutParams, NodeRef, Rect};

#[test]
fn opening_an_aggregate_preserves_ids_bytes_and_descendants_without_other_siblings() {
    let mut index = Index::new("sample".into()).unwrap();
    let root = index.root();
    index
        .insert(
            root,
            "large.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(10000),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    let folder = index
        .insert(
            root,
            "small".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let file = index
        .insert(
            folder,
            "nested.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(30),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    let tiny = index
        .insert(
            root,
            "tiny.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(20),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    let area = Rect::new(0.0, 0.0, 100.0, 100.0);
    let layout = treemap::layout(
        &index,
        root,
        area,
        LayoutParams {
            min_pixel_area: 100.0,
            header_height: 10.0,
            max_depth: usize::MAX,
        },
    )
    .unwrap();
    let aggregate = layout
        .placed
        .iter()
        .find(|cell| matches!(cell.node, NodeRef::Aggregate { .. }))
        .unwrap();
    assert_eq!(aggregate.known_sum, 50);
    let detail = treemap::layout_members(
        &index,
        root,
        &aggregate.members,
        area,
        LayoutParams {
            min_pixel_area: 1.0,
            header_height: 10.0,
            max_depth: usize::MAX,
        },
    )
    .unwrap();
    assert_eq!(detail.known_sum, 50);
    assert_eq!(detail.placed.len(), 4);
    assert!(
        detail
            .placed
            .iter()
            .any(|cell| cell.node == NodeRef::Entry(file) && cell.known_sum == 30)
    );
    assert!(
        detail
            .placed
            .iter()
            .any(|cell| cell.node == NodeRef::Entry(tiny) && cell.known_sum == 20)
    );
    assert!(
        treemap::layout_members(
            &index,
            root,
            &[file],
            area,
            LayoutParams {
                min_pixel_area: 1.0,
                header_height: 10.0,
                max_depth: usize::MAX,
            }
        )
        .is_err()
    );
    assert!(
        treemap::layout_members(
            &index,
            root,
            &[tiny, tiny],
            area,
            LayoutParams {
                min_pixel_area: 1.0,
                header_height: 10.0,
                max_depth: usize::MAX,
            }
        )
        .is_err()
    );
}
