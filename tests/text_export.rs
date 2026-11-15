use hogmap::filter::Filter;
use hogmap::index::{Index, Kind, SizeMetric, Sizes, State};
use hogmap::text_export::{self, View};
use hogmap::treemap::{self, LayoutParams, Rect};
const PREFIX: &str = "HogMap view text v1\n";
const SCOPE: &str = "Scope: drawn cells only; aggregates are not expanded.\nEntries without area (unknown or zero size) are not listed; unknowns remain in folder and aggregate unknown marks.\nTree (two spaces per level)\tSize\tParent percentage\tKind\n";
#[test]
fn text_export_has_literal_root_zoom_and_filtered_reports_with_unknowns() {
    let mut index = Index::new("Root".into()).unwrap();
    let root = index.root();
    let folder = index
        .insert(
            root,
            "Folder".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    index
        .insert(
            root,
            "a.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(100),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    index
        .insert(
            folder,
            "b.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(300),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    index
        .insert(
            folder,
            "unknown.bin".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    index.set_state(root, State::Complete).unwrap();
    let params = LayoutParams {
        min_pixel_area: 1.0,
        header_height: 0.0,
        max_depth: 64,
    };
    let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
    let layout = treemap::layout(&index, root, rect, params).unwrap();
    let report = text_export::render(View {
        index: &index,
        layout: &layout,
        location: "Root",
        expression: "",
        filtered: false,
        metric: SizeMetric::Logical,
    });
    // Depth-first: b.bin follows its own parent Folder, so the indents read as the hierarchy.
    assert_eq!(
        report,
        format!(
            "{PREFIX}Location: Root\nMetric: logical\nMode: Highlight\nExpression: \n{SCOPE}Root\t400 B + unknown\t-\tfolder\n  Folder\t300 B + unknown\tunknown\tfolder\n    b.bin\t300 B\tunknown\tfile\n  a.bin\t100 B\tunknown\tfile\n"
        )
    );
    let layout = treemap::layout(&index, folder, rect, params).unwrap();
    assert_eq!(
        text_export::render(View {
            index: &index,
            layout: &layout,
            location: "Root / Folder",
            expression: "",
            filtered: false,
            metric: SizeMetric::Logical
        }),
        format!(
            "{PREFIX}Location: Root / Folder\nMetric: logical\nMode: Highlight\nExpression: \n{SCOPE}Folder\t300 B + unknown\t-\tfolder\n  b.bin\t300 B\tunknown\tfile\n"
        )
    );
    let decisions = Filter::parse("a*").unwrap().matches(&index);
    let layout = treemap::layout_filtered(
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
        text_export::render(View {
            index: &index,
            layout: &layout,
            location: "Root",
            expression: "a*",
            filtered: true,
            metric: SizeMetric::Logical
        }),
        format!(
            "{PREFIX}Location: Root\nMetric: logical\nMode: Filter\nExpression: a*\n{SCOPE}Root\t100 B\t-\tfolder\n  a.bin\t100 B\t100.0%\tfile\n"
        )
    );
    let layout =
        treemap::layout_with_metric(&index, root, rect, params, SizeMetric::Allocated).unwrap();
    assert_eq!(
        text_export::render(View {
            index: &index,
            layout: &layout,
            location: "Root",
            expression: "",
            filtered: false,
            metric: SizeMetric::Allocated
        }),
        format!(
            "{PREFIX}Location: Root\nMetric: observed allocated\nMode: Highlight\nExpression: \n{SCOPE}Root\tunknown\t-\tfolder\n"
        )
    );
}
#[test]
fn text_export_labels_unexpanded_aggregates_and_escapes_line_controls() {
    let mut index = Index::new("Root".into()).unwrap();
    index.set_state(index.root(), State::Complete).unwrap();
    for name in ["a.bin", "b.bin"] {
        index
            .insert(
                index.root(),
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(100),
                    allocated: None,
                },
                None,
                State::Complete,
            )
            .unwrap();
    }
    let layout = treemap::layout(
        &index,
        index.root(),
        Rect::new(0.0, 0.0, 100.0, 100.0),
        LayoutParams {
            min_pixel_area: 6000.0,
            header_height: 0.0,
            max_depth: 64,
        },
    )
    .unwrap();
    assert_eq!(
        text_export::render(View {
            index: &index,
            layout: &layout,
            location: "Root",
            expression: "",
            filtered: false,
            metric: SizeMetric::Logical
        }),
        format!(
            "{PREFIX}Location: Root\nMetric: logical\nMode: Highlight\nExpression: \n{SCOPE}Root\t200 B\t-\tfolder\n  [2 grouped items; not expanded]\t200 B\t100.0%\taggregate\n"
        )
    );
    assert_eq!(
        text_export::escape(std::ffi::OsStr::new("name\twith\ncontrols\\")),
        "name\\twith\\ncontrols\\\\"
    );
}
