use super::{ConflictResolution, History, HogMap, ScanGeneration, text};
use eframe::egui;
use hogmap::{snapshot, text_export};
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Open,
    Save,
    Export,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Cancelled,
    Selected(PathBuf),
}

impl HogMap {
    pub(super) fn can_save_snapshot(&self) -> bool {
        self.reducer.index().is_some()
            && self.reducer.terminal().is_some()
            && !self.work_pending()
            && self.receiver.is_none()
    }
    pub(super) fn request_file(&mut self, action: Action) {
        if self.closing || self.pending_file.is_some() {
            return;
        }
        if action == Action::Open && !self.work_pending() && self.receiver.is_none()
            || action != Action::Open && self.can_save_snapshot()
        {
            self.pending_file = Some(action);
        }
    }
    pub(super) fn begin_export(&mut self) {
        if !self.can_save_snapshot() {
            return;
        }
        let (Some(index), Some(cache)) = (
            self.ghosts.display.as_ref().or(self.reducer.index()),
            &self.cache,
        ) else {
            return;
        };
        if cache.key.metric != self.metric
            || cache.key.location != self.history.current()
            || self.filter_changed
        {
            return;
        }
        self.export_preview = Some(text_export::render(text_export::View {
            index,
            layout: &cache.layout,
            location: &cache.location_label,
            expression: &self.filter.active_expression,
            filtered: self.geometry_filter,
            metric: self.metric,
        }));
    }
    pub(super) fn show_export_preview(&mut self, context: &egui::Context) {
        let Some(report) = &self.export_preview else {
            return;
        };
        let lines = report.lines().count();
        let preview = report.lines().take(20).collect::<Vec<_>>().join("\n");
        let mut save = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::new("export-preview"))
            .frame(egui::Frame::popup(&context.global_style()).inner_margin(16.0))
            .show(context, |ui| {
                ui.set_width((context.content_rect().width() - 64.0).clamp(280.0, 640.0));
                ui.heading(text::EXPORT_PREVIEW);
                ui.label(text::export_line_count(lines));
                ui.label(text::EXPORT_PRIVACY);
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                egui::ScrollArea::both()
                    .max_height((context.content_rect().height() - 220.0).clamp(80.0, 300.0))
                    .show(ui, |ui| {
                        ui.monospace(preview);
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    save = ui.button(text::EXPORT_SAVE).clicked();
                    cancel = ui.button(text::CANCEL).clicked();
                });
            });
        modal.response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Window, true, text::EXPORT_PREVIEW)
        });
        if save {
            self.request_file(Action::Export);
        } else if cancel || modal.should_close() {
            self.export_preview = None;
        }
    }
    pub(super) fn process_file_action(
        &mut self,
        choose: impl FnOnce(Action) -> Result<Choice, String>,
    ) {
        let Some(action) = self.pending_file.take() else {
            return;
        };
        if action == Action::Open && (self.work_pending() || self.receiver.is_some())
            || action != Action::Open && !self.can_save_snapshot()
        {
            return;
        }
        let path = match choose(action) {
            Ok(Choice::Cancelled) => return,
            Ok(Choice::Selected(path)) => path,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let result = match action {
            Action::Open => std::fs::File::open(&path)
                .map_err(|e| snapshot::Error::Read(e.kind()))
                .and_then(|file| snapshot::decode(std::io::BufReader::new(file)))
                .map(|snapshot| self.install_snapshot(path, snapshot)),
            Action::Save => {
                let header = self.volume.as_ref().map(|volume| snapshot::Header {
                    root: volume.root.clone(),
                    label: volume.label.clone(),
                    free_bytes: volume.free_bytes,
                    total_bytes: volume.total_bytes,
                    storage_reserve_bytes: volume.storage_reserve_bytes,
                    capacity: volume.capacity,
                });
                let save = snapshot::Save {
                    root_path: if self.snapshot_file.is_some() {
                        self.saved_root.as_deref()
                    } else {
                        self.scan_root.as_deref()
                    },
                    metric: self.metric,
                    terminal: self.reducer.terminal().expect("enabled stable snapshot"),
                    index: self.reducer.index().expect("enabled index"),
                    evidence: self.volume_evidence.as_ref(),
                    header: header.as_ref(),
                };
                snapshot::write_file(&path, |writer| snapshot::encode(writer, &save))
            }
            Action::Export => {
                let Some(report) = self.export_preview.as_ref() else {
                    return;
                };
                snapshot::write_file(&path, |writer| {
                    writer
                        .write_all(report.as_bytes())
                        .map_err(|e| snapshot::Error::Write(e.kind()))
                })
                .map(|()| self.export_preview = None)
            }
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.menu_notice = Some(text::FILE_DONE.into());
            }
            Err(error) => {
                let message = error.to_string();
                if self.chooser.open {
                    self.chooser.error = Some(message.clone());
                }
                self.error = Some(message);
            }
        }
        self.status_cache = None;
    }
    pub(super) fn install_snapshot(&mut self, path: PathBuf, snapshot: snapshot::Snapshot) {
        let generation = ScanGeneration::new(self.reducer.generation().value().wrapping_add(1));
        self.reducer =
            hogmap::scan::ScanReducer::from_snapshot(generation, snapshot.index, snapshot.terminal);
        self.ghosts.clear();
        self.ghost_baseline = None;
        self.scan_root = None; // No shell menu, rescan or filesystem lookup from untrusted stored paths.
        self.saved_root = snapshot.root_path;
        self.snapshot_file = Some(path);
        self.scan_started = Some(std::time::Instant::now());
        self.scan_elapsed = std::time::Duration::ZERO;
        self.cancelled = false;
        self.metric = snapshot.metric;
        self.volume_evidence = snapshot.evidence;
        self.volume = snapshot.header.map(|header| super::dialog::DriveChoice {
            root: header.root,
            label: header.label,
            free_bytes: header.free_bytes,
            total_bytes: header.total_bytes,
            storage_reserve_bytes: header.storage_reserve_bytes,
            capacity: header.capacity,
        });
        self.resolution = ConflictResolution::Unavailable;
        self.observations = None;
        self.scan_report = None;
        self.history = History::default();
        self.selected = None;
        self.pending_menu = None;
        self.export_preview = None;
        self.filter_text.clear();
        self.filter.set("");
        self.geometry_filter = false;
        self.chooser.open = false;
        self.cache = None;
        self.volume_map = None;
        self.status_cache = None;
        self.layout_failure = None;
        self.last_layout = None;
        self.resolution_revision = 0;
        self.version = self.version.wrapping_add(1);
        self.filter_changed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hogmap::index::{Index, SizeMetric, State};
    use hogmap::scan::Terminal;
    fn app(context: &egui::Context) -> HogMap {
        let mut index = Index::new("Fixture".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        let mut app = HogMap::with_reducer(
            context.clone(),
            hogmap::scan::ScanReducer::new(ScanGeneration::new(0)),
        );
        app.install_snapshot(
            "saved.hogmap".into(),
            snapshot::Snapshot {
                root_path: Some(r"X:\untrusted".into()),
                metric: SizeMetric::Allocated,
                terminal: Terminal::Finished,
                index,
                evidence: None,
                header: None,
            },
        );
        app
    }
    #[test]
    fn opening_a_snapshot_is_not_a_live_scan_and_cancel_keeps_the_existing_view() {
        let context = egui::Context::default();
        let mut app = app(&context);
        assert!(app.scan_root.is_none());
        assert_eq!(app.saved_root, Some(r"X:\untrusted".into()));
        assert!(app.receiver.is_none() && app.producer.is_none());
        assert_eq!(app.metric, SizeMetric::Allocated);
        assert_eq!(
            text::snapshot_header(app.snapshot_file.as_ref().unwrap()),
            "Snapshot: saved.hogmap · not live"
        );
        let generation = app.reducer.generation();
        app.rescan();
        assert!(!app.work_pending());
        for action in [Action::Save, Action::Open] {
            app.request_file(action);
            app.process_file_action(|_| Ok(Choice::Cancelled));
            assert_eq!(app.reducer.generation(), generation);
            assert!(app.error.is_none());
        }
    }
    #[test]
    fn file_shortcuts_leave_expression_typing_alone() {
        let context = egui::Context::default();
        let mut app = app(&context);
        for key in [egui::Key::S, egui::Key::O, egui::Key::E] {
            let id = egui::Id::new("snapshot-shortcut-expression");
            let _ = context.run_ui(egui::RawInput::default(), |ui| {
                ui.add(egui::TextEdit::singleline(&mut app.filter_text).id(id))
                    .request_focus();
            });
            let input = egui::RawInput {
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL,
                }],
                ..Default::default()
            };
            let _ = context.run_ui(input, |ui| {
                ui.add(egui::TextEdit::singleline(&mut app.filter_text).id(id));
                app.shortcuts(&context);
            });
            assert!(app.pending_file.is_none() && app.export_preview.is_none());
            context.memory_mut(|memory| memory.surrender_focus(id));
            let _ = context.run_ui(egui::RawInput::default(), |_| {});
        }
    }
}
