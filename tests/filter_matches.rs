use hogmap::filter::{Filter, Match};
use hogmap::index::{FileIdentity, Index, IndexError, Kind, Sizes, State};

#[test]
fn incremental_matches_preserve_empty_context_unknowns_aliases_and_listed_logical_sizes() {
    use Match::{No, Unknown, Yes};
    let check = |index: &Index, filter: &Filter, expected: &[Match], counts| {
        let cached = filter.matches(index);
        assert_eq!(cached.matches, expected);
        assert_eq!((cached.matched_files, cached.unknown_files), counts);
        // Independent full evaluation, including directory context, without the snapshot cache.
        let mut full: Vec<_> = index
            .entries()
            .map(|(_, entry)| {
                if entry.kind == Kind::File {
                    filter.evaluate(entry.name, entry.sizes.logical)
                } else {
                    No
                }
            })
            .collect();
        for (id, entry) in index.entries().rev() {
            if let Some(parent) = entry.parent {
                let child = full[id.position()];
                let current = full[parent.position()];
                full[parent.position()] = if child == Yes || current == Yes {
                    Yes
                } else if child == Unknown || current == Unknown {
                    Unknown
                } else {
                    No
                };
            }
        }
        assert_eq!(cached.matches, full);
    };
    let empty = Filter::default();
    let real = Filter::parse("*.bin;>10b").unwrap();
    let mut index = Index::new("Filter fixture".into()).unwrap();
    let root = index.root();
    check(&index, &empty, &[No], (0, 0));
    let directory = index
        .insert(
            root,
            "empty".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Pending,
        )
        .unwrap();
    check(&index, &empty, &[No, No], (0, 0));
    check(&index, &real, &[No, No], (0, 0));
    let unknown = index
        .insert(
            directory,
            "mystery.bin".into(),
            Kind::File,
            Sizes {
                logical: None,
                allocated: Some(1),
            },
            None,
            State::Complete,
        )
        .unwrap();
    check(&index, &real, &[Unknown, Unknown, Unknown], (0, 1));
    let identity = FileIdentity { volume: 1, file: 8 };
    index
        .insert(
            root,
            "first.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(20),
                allocated: Some(100),
            },
            Some(identity),
            State::Complete,
        )
        .unwrap();
    check(&index, &real, &[Yes, Unknown, Unknown, Yes], (1, 1));
    index
        .insert(
            directory,
            "alias.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(5),
                allocated: Some(200),
            },
            Some(identity),
            State::Complete,
        )
        .unwrap();
    check(&index, &real, &[Yes, Unknown, Unknown, Yes, No], (1, 1));
    check(&index, &empty, &[Yes, Yes, Yes, Yes, Yes], (3, 0));
    check(&index, &real, &[Yes, Unknown, Unknown, Yes, No], (1, 1));
    index
        .insert(
            directory,
            "big.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(30),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    check(&index, &real, &[Yes, Yes, Unknown, Yes, No, Yes], (2, 1));
    index.set_state(unknown, State::Inaccessible).unwrap();
    index.set_state(directory, State::Complete).unwrap();
    check(&index, &real, &[Yes, Yes, Unknown, Yes, No, Yes], (2, 1));
    index
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(40),
                allocated: Some(400),
            },
        )
        .unwrap();
    check(&index, &real, &[Yes, Yes, Unknown, Yes, No, Yes], (2, 1));
    assert_eq!(
        index.insert(
            root,
            "sealed".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete
        ),
        Err(IndexError::ResolvedSnapshot)
    );
    check(&index, &real, &[Yes, Yes, Unknown, Yes, No, Yes], (2, 1));
    check(
        &index,
        &Filter::parse("*.bin;<10b").unwrap(),
        &[Yes, Yes, Unknown, No, Yes, No],
        (1, 1),
    );
}

fn file(
    index: &mut Index,
    parent: hogmap::index::EntryId,
    name: &str,
    logical: Option<u64>,
    allocated: Option<u64>,
) -> hogmap::index::EntryId {
    index
        .insert(
            parent,
            name.into(),
            Kind::File,
            Sizes { logical, allocated },
            None,
            State::Complete,
        )
        .unwrap()
}

#[test]
fn folder_components_masks_classes_and_tags_obey_positive_or_and_exclusions() {
    use Match::{No, Yes};
    use hogmap::filter::ClassTable;
    use std::time::SystemTime;
    let mut index = Index::new("root".into()).unwrap();
    let root = index.root();
    let temp = index
        .insert(
            root,
            "TEMP".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let nested = index
        .insert(
            temp,
            "nested".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let a = file(&mut index, nested, "photo.JPG", Some(20), Some(8));
    let b = file(&mut index, root, "temp", Some(30), Some(16));
    let c = file(&mut index, root, "report.CUSTOM", Some(40), Some(32));
    for (expression, expected) in [
        (r"\temp", [Yes, No, No]),
        (r"|\temp", [No, Yes, Yes]),
        (r"\te?p;report.*", [Yes, No, Yes]),
        (r"\temp;*.custom;s>25b", [No, No, Yes]),
        (r"\nested;|*.jpg", [No, No, No]),
        (":class:images", [Yes, No, No]),
        ("-:class:images", [No, Yes, Yes]),
    ] {
        let filter = Filter::parse(expression).unwrap();
        assert_eq!(
            [a, b, c].map(|id| filter.evaluate_entry(&index, id)),
            expected,
            "{expression}"
        );
    }
    let mut classes = ClassTable::default();
    classes.set("work", &["custom"]).unwrap();
    let filter =
        Filter::parse_with_classes(":class:work", &classes, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(
        [a, b, c].map(|id| filter.evaluate_entry(&index, id)),
        [No, No, Yes]
    );
    index.set_tags(temp, 1).unwrap();
    index.set_tags(c, 8).unwrap();
    for (expression, expected) in [
        (":1", [Yes, No, No]),
        ("+:red", [Yes, No, No]),
        ("-:red", [No, Yes, Yes]),
        (":4", [No, No, Yes]),
        (":blue", [No, No, Yes]),
        (":tag:", [Yes, No, Yes]),
        ("-:tag:", [No, Yes, No]),
        ("*.jpg;:blue", [Yes, No, Yes]),
        ("*.jpg;-:red", [No, No, No]),
        (":red;:blue;>25b", [No, No, Yes]),
    ] {
        assert_eq!(
            [a, b, c].map(|id| Filter::parse(expression)
                .unwrap()
                .evaluate_entry(&index, id)),
            expected,
            "{expression}"
        );
    }
    assert_eq!(index.tags(a), Some(1));
    assert_eq!(index.tags(c), Some(8));
    index.set_tags(a, 0).unwrap();
    assert_eq!(index.tags(a), Some(0));
    assert_eq!(
        Filter::parse(":red").unwrap().matches(&index).matched_files,
        0
    );
    assert!(index.set_tags(a, 16).is_err());
    for (bits, alias) in [(2, ":yellow"), (4, ":green")] {
        index.set_tags(b, bits).unwrap();
        assert_eq!(Filter::parse(alias).unwrap().evaluate_entry(&index, b), Yes);
    }
    assert_eq!(
        Filter::parse(":red")
            .unwrap()
            .evaluate(std::ffi::OsStr::new("a"), Some(1)),
        Match::Unknown
    );
}

#[test]
fn metric_sizes_and_strict_ages_use_observations_and_a_fixed_reference_instant() {
    use Match::{No, Unknown, Yes};
    use hogmap::filter::{ClassTable, Metadata};
    use std::time::{Duration, SystemTime};
    let mut index = Index::new("root".into()).unwrap();
    let root = index.root();
    let id = file(&mut index, root, "a.bin", Some(2048), Some(512));
    let unknown = file(&mut index, root, "b.bin", None, None);
    index
        .set_filter_metadata(
            id,
            Metadata {
                created: 116_444_745_000_000_000,
                modified: 116_444_745_900_000_000,
                accessed: 116_444_736_000_000_000,
                ..Metadata::default()
            },
        )
        .unwrap();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    for (expression, expected) in [
        ("s>1mb", No),
        ("s>1k", Yes),
        ("d<1kb", Yes),
        ("d>1k", No),
        (">2kb", No),
        ("s<2k", No),
        ("d<512b", No),
        ("c>99s", Yes),
        ("c<100s", No),
        ("c>100s", No),
        ("m<11s", Yes),
        ("m>10s", No),
        ("a<1h", Yes),
        ("a>1h", No),
        ("a<1d", Yes),
        ("a<1w", Yes),
        ("a<1m", No),
    ] {
        let filter = Filter::parse_with_classes(expression, &ClassTable::default(), now).unwrap();
        assert_eq!(filter.evaluate_entry(&index, id), expected, "{expression}");
        assert_eq!(
            filter.evaluate_entry(&index, unknown),
            Unknown,
            "{expression}"
        );
    }
    let future =
        Filter::parse_with_classes("m<1d", &ClassTable::default(), SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(future.evaluate_entry(&index, id), Unknown);
    assert_eq!(
        Filter::parse("missing;s>1kb")
            .unwrap()
            .evaluate_entry(&index, unknown),
        No
    );
}

#[test]
fn attributes_preserve_known_false_unknown_and_required_forbidden_semantics() {
    use Match::{No, Unknown, Yes};
    use hogmap::filter::Metadata;
    let mut index = Index::new("root".into()).unwrap();
    let root = index.root();
    let known = file(&mut index, root, "known", Some(10), Some(10));
    let unknown = file(&mut index, root, "unknown", Some(10), Some(10));
    for (name, bits) in [
        ("system", 4),
        ("hidden", 2),
        ("compressed", 2048),
        ("encrypted", 16384),
        ("offline", 4096),
        ("sparse", 512),
    ] {
        index
            .set_filter_metadata(
                known,
                Metadata {
                    attributes: Some(bits),
                    ..Metadata::default()
                },
            )
            .unwrap();
        for (prefix, expected) in [("", Yes), ("+", Yes), ("-", No)] {
            let filter = Filter::parse(&format!("{prefix}:attr:{name}")).unwrap();
            assert_eq!(filter.evaluate_entry(&index, known), expected);
            assert_eq!(filter.evaluate_entry(&index, unknown), Unknown);
        }
        index
            .set_filter_metadata(
                known,
                Metadata {
                    attributes: Some(0),
                    ..Metadata::default()
                },
            )
            .unwrap();
        assert_eq!(
            Filter::parse(&format!(":attr:{name}"))
                .unwrap()
                .evaluate_entry(&index, known),
            No
        );
    }
    for ads in [false, true] {
        index
            .set_filter_metadata(
                known,
                Metadata {
                    ads: Some(ads),
                    ..Metadata::default()
                },
            )
            .unwrap();
        assert_eq!(
            Filter::parse(":attr:ads")
                .unwrap()
                .evaluate_entry(&index, known),
            if ads { Yes } else { No }
        );
        assert_eq!(
            Filter::parse("-:attr:ads")
                .unwrap()
                .evaluate_entry(&index, known),
            if ads { No } else { Yes }
        );
    }
    assert_eq!(
        Filter::parse(":attr:ads")
            .unwrap()
            .evaluate_entry(&index, unknown),
        Unknown
    );
    for (expression, file_result, directory_result) in [
        (":attr:file", Yes, No),
        ("-:attr:file", No, Yes),
        (":attr:dir", No, Yes),
        ("-:attr:dir", Yes, No),
    ] {
        let filter = Filter::parse(expression).unwrap();
        assert_eq!(filter.evaluate_entry(&index, known), file_result);
        assert_eq!(filter.evaluate_entry(&index, root), directory_result);
    }
}

#[test]
fn filtered_file_state_changes_preserve_excluded_and_undecided_geometry() {
    use hogmap::index::SizeMetric;
    use hogmap::treemap::{self, LayoutParams, Rect};
    for (expression, expected_weight, expected_unknown) in
        [("missing", 0, false), (":attr:hidden", 100, true)]
    {
        let mut index = Index::new("root".into()).unwrap();
        let root = index.root();
        index.set_state(root, State::Complete).unwrap();
        let id = index
            .insert(
                root,
                "a.bin".into(),
                Kind::File,
                Sizes {
                    logical: Some(100),
                    allocated: None,
                },
                None,
                State::Pending,
            )
            .unwrap();
        let decisions = Filter::parse(expression).unwrap().matches(&index);
        let rect = Rect::new(0.0, 0.0, 900.0, 600.0);
        let params = LayoutParams {
            min_pixel_area: 1.0,
            header_height: 0.0,
            max_depth: 10,
        };
        let initial = treemap::layout_filtered(
            &index,
            root,
            None,
            rect,
            params,
            SizeMetric::Logical,
            &decisions.matches,
        )
        .unwrap();
        assert_eq!(
            (initial.known_sum, initial.has_unknown),
            (expected_weight, expected_unknown)
        );
        for state in [State::Complete, State::Pending, State::Complete] {
            index.set_state(id, state).unwrap();
            let cached = treemap::layout_filtered(
                &index,
                root,
                None,
                rect,
                params,
                SizeMetric::Logical,
                &decisions.matches,
            )
            .unwrap();
            assert_eq!(
                (cached.known_sum, cached.has_unknown),
                (expected_weight, expected_unknown)
            );
            treemap::layout(&index, root, rect, params).unwrap();
            let rebuilt = treemap::layout_filtered(
                &index,
                root,
                None,
                rect,
                params,
                SizeMetric::Logical,
                &decisions.matches,
            )
            .unwrap();
            assert_eq!(cached, rebuilt);
        }
    }
}

#[test]
fn filtered_geometry_and_totals_exclude_no_but_keep_undecided_separate() {
    use hogmap::index::SizeMetric;
    use hogmap::treemap::{self, LayoutParams, NodeRef, Rect};
    let mut index = Index::new("root".into()).unwrap();
    let root = index.root();
    let a = file(&mut index, root, "a.jpg", Some(20), Some(8));
    let b = file(&mut index, root, "b.bin", Some(30), Some(12));
    let c = file(&mut index, root, "c.jpg", Some(40), None);
    let filter = Filter::parse("*.jpg;d<10b").unwrap();
    let results = filter.matches(&index);
    assert_eq!(
        results.matches,
        [Match::Yes, Match::Yes, Match::No, Match::Unknown]
    );
    assert_eq!((results.matched_files, results.unknown_files), (1, 1));
    let totals = results.totals(&index, SizeMetric::Logical);
    assert_eq!(
        (
            totals.matched_bytes,
            totals.undecided_bytes,
            totals.unknown_size_files
        ),
        (Some(20), Some(40), 0)
    );
    let params = LayoutParams {
        min_pixel_area: 1.0,
        header_height: 0.0,
        max_depth: 10,
    };
    let rect = Rect::new(0.0, 0.0, 900.0, 600.0);
    let highlight = treemap::layout(&index, root, rect, params).unwrap();
    let filtered = treemap::layout_filtered(
        &index,
        root,
        None,
        rect,
        params,
        SizeMetric::Logical,
        &results.matches,
    )
    .unwrap();
    assert_eq!(highlight.known_sum, 90);
    assert_eq!(filtered.known_sum, 60);
    assert!(filtered.has_unknown);
    assert!(
        filtered
            .placed
            .iter()
            .all(|cell| cell.node != NodeRef::Entry(b))
    );
    assert!(
        filtered
            .placed
            .iter()
            .any(|cell| cell.node == NodeRef::Entry(a))
    );
    assert!(
        filtered
            .placed
            .iter()
            .any(|cell| cell.node == NodeRef::Entry(c))
    );
    assert_eq!(index.summary().unwrap().known_logical_bytes, 90);
    // A fresh parsed filter produces the same public decisions and geometry as the cached query.
    let scratch = Filter::parse("*.jpg;d<10b").unwrap();
    assert_eq!(
        results.matches,
        [
            Match::Yes,
            scratch.evaluate_entry(&index, a),
            scratch.evaluate_entry(&index, b),
            scratch.evaluate_entry(&index, c)
        ]
    );
    assert_eq!(
        filtered,
        treemap::layout_filtered(
            &index,
            root,
            None,
            rect,
            params,
            SizeMetric::Logical,
            &scratch.matches(&index).matches
        )
        .unwrap()
    );
}
