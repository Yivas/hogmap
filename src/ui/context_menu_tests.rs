//! Recorder-only secondary-click tests: no metadata, COM, native window or command invocation.
use super::*;
use hogmap::index::{Kind, Sizes};
use hogmap::scan::{Batch, DirectoryOutcome, NodeKey, ScanEvent, ScanEventKind, ScanRecord};

fn frame(app: &mut HogMap, size: egui::Vec2, events: Vec<egui::Event>, shift: bool) {
    let context = app.context.clone();
    let _ = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            modifiers: Modifiers {
                shift,
                ..Default::default()
            },
            events,
            ..Default::default()
        },
        |ui| app.map(ui),
    );
}
fn point(app: &HogMap, node: NodeRef) -> egui::Pos2 {
    let cell = app
        .cache
        .as_ref()
        .unwrap()
        .layout
        .placed
        .iter()
        .find(|cell| cell.node == node)
        .unwrap();
    paint::rect(cell.header.unwrap_or(cell.rect)).center()
}
fn secondary(app: &mut HogMap, size: egui::Vec2, position: egui::Pos2, shift: bool) {
    for pressed in [true, false] {
        frame(
            app,
            size,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Secondary,
                    pressed,
                    modifiers: Modifiers {
                        shift,
                        ..Default::default()
                    },
                },
            ],
            shift,
        );
    }
}
fn named(app: &HogMap, name: &str) -> NodeRef {
    NodeRef::Entry(
        app.reducer
            .index()
            .unwrap()
            .entries()
            .find(|(_, entry)| entry.name == std::ffi::OsStr::new(name))
            .unwrap()
            .0,
    )
}

#[test]
fn secondary_click_selects_and_records_native_paths_in_root_and_zoomed_views() {
    for theme in [Theme::Dark, Theme::Light] {
        for size in [egui::vec2(1914.0, 1035.0), egui::vec2(1280.0, 720.0)] {
            let context = egui::Context::default();
            theme::apply(&context, theme);
            context.set_pixels_per_point(1.5);
            let mut app = composition_tests::app(&context);
            app.scan_root = Some(r"C:\fixture".into());
            frame(&mut app, size, vec![], false);
            frame(&mut app, size, vec![], false);
            let first = named(&app, "first");
            let at = point(&app, first);
            secondary(&mut app, size, at, true);
            assert_eq!(app.selected, Some(first));
            let mut requests = 0;
            app.process_context_menu(|request| {
                requests += 1;
                assert_eq!(request.path, PathBuf::from(r"C:\fixture\first"));
                assert_eq!(request.position, [at.x, at.y]);
                assert_eq!(request.pixels_per_point, 1.5);
                assert!(request.extended);
                Ok(false)
            });
            assert_eq!(requests, 1);
            app.process_context_menu(|_| panic!("a consumed request must never be replayed"));
            assert!(app.menu_notice.is_none());
            let folder = named(&app, "folder");
            let at = point(&app, folder);
            secondary(&mut app, size, at, false);
            app.process_context_menu(|request| {
                assert_eq!(request.path, PathBuf::from(r"C:\fixture\folder"));
                assert!(!request.extended);
                Ok(false)
            });
            assert_eq!(app.selected, Some(folder));
            app.zoom(folder);
            frame(&mut app, size, vec![], false);
            for (node, path) in [
                (folder, r"C:\fixture\folder"),
                (named(&app, "child"), r"C:\fixture\folder\child"),
            ] {
                let at = point(&app, node);
                secondary(&mut app, size, at, false);
                app.process_context_menu(|request| {
                    assert_eq!(request.path, PathBuf::from(path));
                    Ok(false)
                });
                assert_eq!(app.selected, Some(node));
            }
        }
    }
}

#[test]
fn synthetic_aggregate_capacity_and_drive_root_cells_never_request_shell_menus() {
    let context = egui::Context::default();
    let mut app = composition_tests::app(&context);
    let size = egui::vec2(1280.0, 720.0);
    frame(&mut app, size, vec![], false);
    let first = named(&app, "first");
    let at = point(&app, first);
    secondary(&mut app, size, at, false);
    assert_eq!(app.selected, Some(first));
    assert!(
        app.pending_menu.is_none(),
        "the in-memory sample has no filesystem path"
    );
    assert_eq!(app.menu_notice.as_deref(), Some(text::SHELL_NO_ITEM));
    app.scan_root = Some(r"C:\".into());
    app.version += 1;
    frame(&mut app, size, vec![], false);
    let root = NodeRef::Entry(app.reducer.index().unwrap().root());
    let at = point(&app, root);
    secondary(&mut app, size, at, false);
    assert_eq!(app.selected, Some(root));
    assert!(app.pending_menu.is_none());
    let cells: Vec<_> = app
        .volume_map
        .as_ref()
        .unwrap()
        .cells
        .iter()
        .map(|cell| cell.rect.center())
        .collect();
    assert!(
        !cells.is_empty(),
        "the fixture must include non-file volume cells"
    );
    for at in cells {
        secondary(&mut app, size, at, false);
        assert!(app.pending_menu.is_none());
    }
    // Build a real layout aggregate with many tiny siblings; still an in-memory scan.
    let generation = ScanGeneration::new(70);
    let mut batch = Batch::new();
    for key in 1..=102 {
        assert!(batch.push(ScanRecord {
            key: NodeKey::new(key),
            parent: (key > 1).then_some(NodeKey::new(1)),
            name: if key == 1 {
                "root".into()
            } else {
                format!("item{key}").into()
            },
            kind: if key == 1 {
                Kind::Directory
            } else {
                Kind::File
            },
            sizes: Sizes {
                logical: Some(if key == 2 { 1_000_000 } else { 1 }),
                allocated: Some(if key == 2 { 1_000_000 } else { 1 })
            },
            identity: None,
        }));
    }
    assert!(batch.push_outcome(DirectoryOutcome {
        key: NodeKey::new(1),
        code: None
    }));
    let (sender, receiver) = event_channel(2);
    sender
        .send(ScanEvent {
            generation,
            event: ScanEventKind::Batch(batch),
        })
        .unwrap();
    sender
        .send(ScanEvent {
            generation,
            event: ScanEventKind::Terminal(Terminal::Finished),
        })
        .unwrap();
    app.reducer = ScanReducer::new(generation);
    app.reducer.drain(&receiver, 4096);
    app.version += 1;
    app.scan_root = Some(r"C:\fixture".into());
    frame(&mut app, size, vec![], false);
    let aggregate = app
        .cache
        .as_ref()
        .unwrap()
        .layout
        .placed
        .iter()
        .find(|cell| matches!(cell.node, NodeRef::Aggregate { .. }))
        .unwrap()
        .node;
    let at = point(&app, aggregate);
    secondary(&mut app, size, at, false);
    assert_eq!(app.selected, Some(aggregate));
    assert!(app.pending_menu.is_none());
    app.process_context_menu(|_| panic!("non-file cells must never reach the shell host"));
    app.zoom(aggregate);
    frame(&mut app, size, vec![], false);
    let subset_root = app.cache.as_ref().unwrap().layout.placed[0].node;
    let at = point(&app, subset_root);
    secondary(&mut app, size, at, false);
    assert!(
        app.pending_menu.is_none(),
        "a subset caption must not offer actions on its whole parent folder"
    );
    let member = named(&app, "item3");
    let at = point(&app, member);
    secondary(&mut app, size, at, false);
    app.process_context_menu(|request| {
        assert_eq!(request.path, PathBuf::from(r"C:\fixture\item3"));
        Ok(false)
    });
    assert_eq!(app.selected, Some(member));
}

#[test]
fn shell_results_only_change_accessible_notices_not_the_snapshot_or_scan_lifecycle() {
    let context = egui::Context::default();
    let mut app = composition_tests::app(&context);
    let size = egui::vec2(1280.0, 720.0);
    app.scan_root = Some(r"C:\fixture".into());
    frame(&mut app, size, vec![], false);
    let node = named(&app, "first");
    let version = app.version;
    let before = app
        .reducer
        .index()
        .unwrap()
        .summary_for(app.metric)
        .unwrap();
    for (result, expected) in [
        (Ok(false), None),
        (Ok(true), Some(text::SHELL_CHANGED)),
        (
            Err(text::SHELL_MISSING.to_owned()),
            Some(text::SHELL_MISSING),
        ),
        (
            Err("Windows shell: QueryContextMenu failed (HRESULT 0x80004005).".to_owned()),
            Some("Windows shell: QueryContextMenu failed (HRESULT 0x80004005)."),
        ),
    ] {
        // The previous status-only pass registers no map controls. Register and settle them
        // before press/release, just as the real renderer (which draws both panels) does.
        let mut clip = egui::Rect::NOTHING;
        for _ in 0..2 {
            let _ = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ui| {
                    clip = ui.clip_rect();
                    app.map(ui);
                },
            );
        }
        let at = point(&app, node);
        let layout = &app.cache.as_ref().unwrap().layout;
        let hit = logic::hit_test(layout, at.x, at.y).expect("settled point hits the map");
        assert_eq!(
            layout.placed[hit].node, node,
            "clip={clip:?}, root={:?}, point={at:?}",
            layout.root
        );
        assert!(
            clip.contains(at) && paint::rect(layout.root).contains(at),
            "clip={clip:?}, root={:?}, point={at:?}",
            layout.root
        );
        secondary(&mut app, size, at, false);
        assert!(app.pending_menu.is_some());
        app.process_context_menu(|_| result);
        assert_eq!(app.menu_notice.as_deref(), expected);
        let _ = context.run_ui(egui::RawInput::default(), |ui| app.status(ui));
        if let Some(expected) = expected {
            assert_eq!(app.status_cache.as_ref().unwrap().brief, expected);
            assert!(
                app.status_cache
                    .as_ref()
                    .unwrap()
                    .accessible
                    .contains(expected)
            );
        }
        assert_eq!(app.version, version);
        assert_eq!(
            app.reducer
                .index()
                .unwrap()
                .summary_for(app.metric)
                .unwrap(),
            before
        );
        assert_eq!(app.reducer.terminal(), Some(Terminal::Finished));
    }
}
