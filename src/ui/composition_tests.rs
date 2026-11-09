//! Synthetic headless egui coverage, not native visual acceptance.
use super::*;
use hogmap::composition::VolumeComponents;
use hogmap::index::{FileIdentity, Kind, Sizes, State};
use hogmap::scan::{Batch, DirectoryOutcome, NodeKey, ScanEvent, ScanEventKind, ScanRecord};
use hogmap::volume::*;
use std::sync::mpsc::channel;
use std::time::SystemTime;

fn facts() -> VolumeEvidence {
    let sample = |free| OriginCapacitySample {
        total_bytes: Some(1200),
        free_bytes: Some(free),
        storage_reserve_bytes: Some(100),
        total_reserved_bytes: Some(150),
        reserve_issue: None,
        metadata: CapacityMetadata {
            source: CapacitySource::WindowsActualGlobalFilesystem,
            captured_at: SystemTime::UNIX_EPOCH,
            issue: None,
        },
    };
    VolumeEvidence {
        scope: Ok(RootScope::WholeVolume),
        filesystem: Ok(ObservedFilesystem::Ntfs),
        before: Ok(sample(200)),
        after: Ok(sample(250)),
        validation: Ok(OriginValidation {
            directory_identity: Ok(ObservedContinuity::UnchangedAtEndpoints),
            normalized_path: Ok(ObservedContinuity::UnchangedAtEndpoints),
        }),
        directories: DirectoryNamespaceObservations::default(),
    }
}
fn app(context: &egui::Context) -> HogMap {
    let mut app = HogMap::with_context(context.clone());
    let generation = ScanGeneration::new(25);
    app.reducer = ScanReducer::new(generation);
    let mut batch = Batch::new();
    for (key, parent, name, kind, allocated) in [
        (1, None, "Synthetic", Kind::Directory, None),
        (2, Some(1), "first", Kind::File, Some(400)),
        (3, Some(1), "folder", Kind::Directory, None),
        (4, Some(3), "child", Kind::File, Some(200)),
    ] {
        assert!(batch.push(ScanRecord {
            key: NodeKey::new(key),
            parent: parent.map(NodeKey::new),
            name: name.into(),
            kind,
            sizes: Sizes {
                logical: allocated.map(|n| n / 2),
                allocated
            },
            identity: allocated.map(|_| FileIdentity {
                volume: 1,
                file: key as u128
            })
        }));
    }
    for key in [1, 3] {
        assert!(batch.push_outcome(DirectoryOutcome {
            key: NodeKey::new(key),
            code: None
        }));
    }
    let (sender, receiver) = event_channel(4);
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
    app.reducer.drain(&receiver, 4096);
    app.metric = SizeMetric::Allocated;
    app.volume_evidence = Some(facts());
    app.chooser.open = false;
    app
}
fn frame(
    context: &egui::Context,
    app: &mut HogMap,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, egui::Rect, egui::Id) {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        )),
        events,
        ..Default::default()
    };
    let mut area = egui::Rect::NOTHING;
    let mut id = egui::Id::NULL;
    let output = context.run_ui(input, |ui| {
        area = ui.available_rect_before_wrap();
        id = ui.id();
        app.map(ui);
    });
    (output, area, id)
}
fn named_nodes(output: &egui::FullOutput) -> Vec<&egui::accesskit::Node> {
    output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("headless accessibility enabled")
        .nodes
        .iter()
        .map(|(_, node)| node)
        .filter(|node| {
            node.label().or(node.value()).is_some_and(|name| {
                name.starts_with("Free space") || name.starts_with("Reserved by Windows")
            })
        })
        .collect()
}
fn assert_near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.002, "{actual} vs {expected}");
}

#[test]
fn apparent_overflow_only_offers_a_metric_that_can_be_drawn() {
    let context = egui::Context::default();
    for both_overflow in [true, false] {
        for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
            let mut app = HogMap::with_context(context.clone());
            let generation = app.reducer.generation();
            let mut batch = Batch::new();
            let alias = Sizes {
                logical: Some(if both_overflow || metric == SizeMetric::Logical {
                    u64::MAX - 1
                } else {
                    5
                }),
                allocated: Some(if both_overflow || metric == SizeMetric::Allocated {
                    u64::MAX - 1
                } else {
                    5
                }),
            };
            for (key, name, kind, sizes) in [
                (1, "Synthetic", Kind::Directory, Sizes::default()),
                (
                    2,
                    "charged",
                    Kind::File,
                    Sizes {
                        logical: Some(1),
                        allocated: Some(1),
                    },
                ),
                (3, "alias", Kind::File, alias),
            ] {
                assert!(batch.push(ScanRecord {
                    key: NodeKey::new(key),
                    parent: (key != 1).then_some(NodeKey::new(1)),
                    name: name.into(),
                    kind,
                    sizes,
                    identity: (key != 1).then_some(FileIdentity { volume: 1, file: 9 }),
                }));
            }
            assert!(batch.push_outcome(DirectoryOutcome {
                key: NodeKey::new(1),
                code: None
            }));
            let (sender, receiver) = event_channel(4);
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
            app.reducer.drain(&receiver, 4096);
            let index = app.reducer.finished_index_mut().unwrap();
            index
                .apply_authoritative_sizes(
                    FileIdentity { volume: 1, file: 9 },
                    Sizes {
                        logical: Some(2),
                        allocated: Some(2),
                    },
                )
                .unwrap();
            let summary = index.summary_for(metric).unwrap();
            assert_eq!(
                (
                    summary.known_bytes,
                    summary.resolution_delta_bytes,
                    summary.unique_bytes,
                    summary.shared_bytes
                ),
                (u64::MAX, 1, 2, u64::MAX - 1)
            );
            app.metric = metric;
            let _ = frame(&context, &mut app, Vec::new());
            assert!(app.cache.is_none());
            let message = &app.current_layout_failure().unwrap().message;
            if both_overflow {
                assert!(!message.contains("is available in View"), "{message}");
                assert!(
                    message.contains("Neither logical nor allocated size can be drawn."),
                    "{message}"
                );
            } else {
                assert!(message.contains("is available in View"), "{message}");
                assert!(!message.contains("Neither"), "{message}");
            }
            let index = app.reducer.index().unwrap();
            let totals = logic::metric_totals(index, metric);
            let overflow = totals.as_ref().unwrap_err();
            assert_eq!(overflow.other_available, !both_overflow);
            let detail = text::status("Finished", "Synthetic", &totals, app.reducer.diagnostics());
            assert_eq!(
                detail.contains("Neither logical nor allocated size can be drawn."),
                both_overflow,
                "{detail}"
            );
            assert_eq!(
                detail.contains("is available in View"),
                !both_overflow,
                "{detail}"
            );
            if !both_overflow {
                app.metric = logic::other_metric(metric);
                let _ = frame(&context, &mut app, Vec::new());
                assert!(app.current_layout_failure().is_none());
                assert_eq!(app.cache.as_ref().unwrap().layout.known_sum, 7);
            }
        }
    }
}

#[test]
fn whole_volume_cells_share_the_tree_scale_without_d_or_file_actions() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = app(&context);
    let index = app.reducer.finished_index_mut().unwrap();
    index
        .insert(
            index.root(),
            "alias".into(),
            Kind::File,
            Sizes {
                logical: Some(100),
                allocated: Some(200),
            },
            Some(FileIdentity { volume: 1, file: 2 }),
            State::Complete,
        )
        .unwrap();
    index
        .apply_authoritative_sizes(
            FileIdentity { volume: 1, file: 2 },
            Sizes {
                logical: Some(150),
                allocated: Some(300),
            },
        )
        .unwrap();
    assert_eq!(
        index
            .summary_for(SizeMetric::Allocated)
            .unwrap()
            .unique_bytes,
        500
    );
    let mut tree = None;
    for theme in [Theme::Dark, Theme::Light] {
        theme::apply(&context, theme);
        let (output, area, _) = frame(&context, &mut app, Vec::new());
        let rect = app.cache.as_ref().unwrap().key.rect;
        assert_near(rect.width, area.width() * 14.0 / 21.0); // apparent 700 : free 250 : reserve 100
        assert_eq!(app.cache.as_ref().unwrap().layout.known_sum, 700);
        let cells = named_nodes(&output);
        assert_eq!(cells.len(), 2);
        let mut drawn_width = rect.width;
        for node in cells {
            let name = node.label().or(node.value()).unwrap();
            assert!(name.contains("Not a file; no file actions"));
            assert!(name.contains("Not a complete disk partition"));
            assert!(name.contains("Estimated difference (not drawn): before 200 B, after 150 B"));
            assert!(!node.supports_action(egui::accesskit::Action::Click));
            assert!(node.supports_action(egui::accesskit::Action::Focus));
            let bounds = node.bounds().unwrap();
            let width = (bounds.x1 - bounds.x0) as f32;
            assert_near(
                width,
                area.width()
                    * if name.starts_with("Free space") {
                        5.0 / 21.0
                    } else {
                        2.0 / 21.0
                    },
            );
            drawn_width += width;
        }
        assert_near(drawn_width, area.width());
        if let Some(previous) = tree {
            assert_eq!(rect, previous);
        } else {
            tree = Some(rect);
        }
    }
    let (_, _, root_id) = frame(&context, &mut app, Vec::new());
    let free_id = root_id.with(("volume-part", composition::VolumePart::Free));
    context.memory_mut(|memory| memory.request_focus(free_id));
    let enter = egui::Event::Key {
        key: Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    };
    let _ = frame(&context, &mut app, vec![enter]);
    assert!(context.memory(|memory| memory.has_focus(free_id)));
    assert_eq!(app.selected, None);
    assert_eq!(app.history.current(), Location::default());
}

#[test]
fn logical_folder_zoom_and_unknown_capacity_never_inherit_volume_cells() {
    let context = egui::Context::default();
    context.enable_accesskit();
    for mode in 0..5 {
        let mut app = app(&context);
        match mode {
            0 => app.metric = SizeMetric::Logical,
            1 => app.volume_evidence.as_mut().unwrap().scope = Ok(RootScope::Folder),
            2 => {
                let index = app.reducer.index().unwrap();
                let directory = index
                    .entries()
                    .find(|(_, entry)| entry.name == "folder")
                    .unwrap()
                    .0;
                app.navigate(Location::directory(index, directory));
            }
            3 => app.volume_evidence.as_mut().unwrap().after = Err(OriginIssue::RootNotCaptured),
            _ => app.history.visit(Location {
                path: Vec::new(),
                members: Some(Members {
                    names: vec!["first".into()],
                    grouped_by: SizeMetric::Allocated,
                }),
            }),
        }
        let (output, area, _) = frame(&context, &mut app, Vec::new());
        assert!(named_nodes(&output).is_empty(), "mode {mode}");
        assert_near(app.cache.as_ref().unwrap().key.rect.width, area.width());
    }
    let mut app = app(&context);
    app.volume_evidence
        .as_mut()
        .unwrap()
        .after
        .as_mut()
        .unwrap()
        .storage_reserve_bytes = None;
    let (output, area, _) = frame(&context, &mut app, Vec::new());
    let cells = named_nodes(&output);
    assert_eq!(cells.len(), 1);
    assert!(
        cells[0]
            .label()
            .or(cells[0].value())
            .unwrap()
            .starts_with("Free space")
    );
    assert_near(
        app.cache.as_ref().unwrap().key.rect.width,
        area.width() * 12.0 / 17.0,
    );
    assert_eq!(
        app.composition().components,
        VolumeComponents {
            free_bytes: Some(250),
            reserve_bytes: None
        }
    );
}

fn add_conflict(app: &mut HogMap) {
    let index = app.reducer.finished_index_mut().unwrap();
    index
        .insert(
            index.root(),
            "alias".into(),
            Kind::File,
            Sizes {
                logical: Some(201),
                allocated: Some(401),
            },
            Some(FileIdentity { volume: 1, file: 2 }),
            State::Complete,
        )
        .unwrap();
}
fn cancelled_batch(app: &HogMap) -> ConflictRereadBatch {
    let cancel = ScanCancellation::new();
    cancel.cancel();
    let index = app.reducer.index().unwrap();
    let mut plan = ConflictRereadPlan::new(index, std::path::Path::new("not-opened"));
    assert!(plan.prepare(index, &cancel, Duration::ZERO));
    plan.reread(&cancel)
}

#[test]
fn conflict_cancellation_preserves_charges_and_stale_results_are_discarded() {
    let context = egui::Context::default();
    let mut app = app(&context);
    add_conflict(&mut app);
    let before = app
        .reducer
        .index()
        .unwrap()
        .summary_for(SizeMetric::Allocated)
        .unwrap();
    let generation = app.reducer.generation();
    let plan = ConflictRereadPlan::new(
        app.reducer.index().unwrap(),
        std::path::Path::new("not-opened"),
    );
    app.resolution = ConflictResolution::Preparing(generation, plan);
    app.cancel();
    assert!(app.cancelled && app.cancellation.is_cancelled());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(app.resolution, ConflictResolution::Complete(_)) {
        app.advance_resolution();
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let ConflictResolution::Complete(report) = &app.resolution else {
        unreachable!()
    };
    assert_eq!(
        (
            report.candidate_identities,
            report.attempted_identities,
            report.resolved_identities,
            report.unresolved_identities
        ),
        (1, 0, 0, 1)
    );
    assert_eq!(
        report
            .issues
            .get(&hogmap::scan_win::ConflictRereadIssue::Cancelled),
        Some(&1)
    );
    assert_eq!(
        app.reducer
            .index()
            .unwrap()
            .summary_for(SizeMetric::Allocated)
            .unwrap(),
        before
    );
    let stale = RereadCompletion {
        generation: ScanGeneration::new(24),
        batch: cancelled_batch(&app),
    };
    assert!(!app.accept_reread(stale));
    assert!(matches!(app.resolution, ConflictResolution::Complete(_)));
    assert_eq!(
        app.reducer
            .index()
            .unwrap()
            .summary_for(SizeMetric::Allocated)
            .unwrap(),
        before
    );
    assert!(app.composition().differences.is_none());
}

#[test]
fn blocked_conflict_worker_is_retired_on_rescan_and_never_joined_by_close() {
    let context = egui::Context::default();
    let mut app = app(&context);
    add_conflict(&mut app);
    let generation = app.reducer.generation();
    let batch = cancelled_batch(&app);
    let (release, wait) = channel();
    app.resolution = ConflictResolution::Reading(thread::spawn(move || {
        wait.recv().unwrap();
        RereadCompletion { generation, batch }
    }));
    app.request_scan(ScanRequest::Sample);
    app.advance_scan();
    assert!(app.cancellation.is_cancelled());
    assert_eq!(app.reducer.generation(), generation);
    assert_eq!(app.pending_rereads.len(), 1);
    assert!(app.pending_scan.is_some());
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.reducer.generation() == generation {
        app.drain(&context);
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert!(app.pending_rereads.is_empty());
    assert!(app.observations.is_none() && app.volume_evidence.is_none());
    assert!(matches!(app.resolution, ConflictResolution::Waiting));

    let context = egui::Context::default();
    let mut closing = self::app(&context);
    add_conflict(&mut closing);
    let batch = cancelled_batch(&closing);
    let (release, wait) = channel();
    closing.resolution = ConflictResolution::Reading(thread::spawn(move || {
        wait.recv().unwrap();
        RereadCompletion { generation, batch }
    }));
    closing.request_close();
    assert!(closing.work_pending() && closing.cancellation.is_cancelled());
    drop(closing); // Returns before the worker is released.
    release.send(()).unwrap();
}
