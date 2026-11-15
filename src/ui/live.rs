use super::{HogMap, Location, Members, ScanRequest, text};
use eframe::egui;
use hogmap::index::{Index, Kind, State};
use hogmap::treemap::{Layout, NodeRef};
use hogmap::{bookmarks, watch};

#[derive(Clone, Copy, Default)]
pub(super) struct Coverage {
    complete: usize,
    partial: usize,
    unknown: usize,
}
pub(super) fn coverage(index: &Index, layout: &Layout) -> Vec<Coverage> {
    let mut bands = Vec::<Coverage>::new();
    let root = match layout.placed.first().map(|p| p.node) {
        Some(NodeRef::Entry(id)) => id,
        _ => index.root(),
    };
    for cell in &layout.placed {
        let ids = match cell.node {
            NodeRef::Entry(id) => vec![id],
            _ => cell.members.clone(),
        };
        for id in ids {
            let mut depth = 0usize;
            let mut next = id;
            while next != root && depth < super::SAFETY_DEPTH {
                let Some(parent) = index.get(next).and_then(|e| e.parent) else {
                    break;
                };
                next = parent;
                depth += 1;
            }
            bands.resize(bands.len().max(depth + 1), Coverage::default());
            let Some(entry) = index.get(id) else {
                continue;
            };
            let band = &mut bands[depth];
            match entry.state {
                State::Partial | State::Inaccessible | State::Stale => band.partial += 1,
                State::Pending => band.unknown += 1,
                State::Complete
                    if entry.kind == Kind::File
                        && (entry.sizes.logical.is_none() || entry.sizes.allocated.is_none()) =>
                {
                    band.unknown += 1
                }
                State::Complete => band.complete += 1,
            }
        }
    }
    bands
}
/// The header the notices panel really built. `id` is the one egui derived for that collapsing
/// header, so a caller addresses the widget instead of guessing how its id is made. Only the
/// headless test reads the fields: the window draws the panel and ignores the answer.
#[allow(dead_code)]
pub(super) struct NoticesHeader {
    pub(super) id: egui::Id,
    pub(super) body_shown: bool,
}
impl HogMap {
    pub(super) fn record_notice(&mut self, message: String) {
        self.notices.push(&message, &message);
        self.last_console_notice = Some(message.clone());
        self.menu_notice = Some(message);
        self.status_cache = None;
    }
    /// Records a line unless the console already ends with it. Only repeats and panics use it: two
    /// real changes can carry the same count for different folders, so a batch that changed
    /// something is recorded directly and never compared by text.
    fn record_watch_notice(&mut self, message: &str) {
        if self
            .notices
            .entries()
            .next_back()
            .is_some_and(|last| last.text == message)
        {
            return;
        }
        self.record_notice(message.to_string());
    }
    pub(super) fn stop_watch(&mut self) {
        if let Some(watch) = self.watcher.take() {
            watch.stop();
            self.retired_watch = Some(watch);
        }
    }
    pub(super) fn poll_watch(&mut self, context: &egui::Context) {
        if let Some(watch) = &mut self.retired_watch {
            if watch.is_finished() {
                if !watch.collect() {
                    self.record_watch_notice(text::WATCH_PANIC);
                }
                self.retired_watch = None;
            } else {
                context.request_repaint_after(super::FRAME_INTERVAL);
            }
        }
        if let Some(update) = self.watcher.as_ref().map(watch::Watch::poll) {
            if update.changes > 0 || update.lost {
                self.apply_watch_changes(&update);
            }
            if let Some(error) = update.error {
                self.watch_enabled = false;
                self.record_notice(error);
                self.stop_watch();
            } else if update.started && !self.watch_active {
                self.watch_active = true;
                self.record_notice(text::WATCH_ACTIVE.into());
            }
        }
        // A worker can end between the poll above and here: it would leave the option on and the
        // slot occupied, so the checkbox would stay checked, the repaint timer would keep running
        // and no replacement could start. Drain whatever it wrote after that poll first, so a
        // specific failure is reported as itself instead of as a generic message, then retire it
        // through `stop_watch`: the retired branch reports a panic, and only a real panic.
        if self.watcher.as_ref().is_some_and(watch::Watch::is_finished) {
            let late = self
                .watcher
                .as_ref()
                .map(watch::Watch::poll)
                .unwrap_or_default();
            // The first poll may have run before the worker wrote this batch, so it is applied
            // here rather than dropped; the error is recorded after it, so it stays the last line.
            if late.changes > 0 || late.lost {
                self.apply_watch_changes(&late);
            }
            if let Some(error) = late.error {
                self.record_notice(error);
            }
            self.watch_enabled = false;
            self.stop_watch();
        }
        if self.watch_enabled
            && !self.closing
            && self.watcher.is_none()
            && self.retired_watch.is_none()
            && !self.work_pending()
            && self.receiver.is_none()
            && self.reducer.terminal() == Some(hogmap::scan::Terminal::Finished)
            && let Some(root) = self.scan_root.clone()
        {
            match watch::Watch::start(root) {
                Ok(watcher) => {
                    self.watcher = Some(watcher);
                    self.watch_active = false;
                    self.record_notice(text::WATCH_STARTING.into());
                }
                Err(error) => {
                    self.watch_enabled = false;
                    self.record_notice(format!("{} ({})", text::WATCH_START_ERROR, error.kind()));
                }
            }
        }
        if self.watcher.is_some() {
            context.request_repaint_after(super::FRAME_INTERVAL);
        }
    }
    fn apply_watch_changes(&mut self, update: &watch::Update) {
        // Whether this batch changed anything the map shows. A batch that changed nothing is the
        // only one allowed to be dropped as a repeat.
        let mut changed = false;
        if let Some(index) = self.reducer.finished_index_mut() {
            // A missing/new name invalidates its nearest indexed parent. Every descendant of
            // that parent is stale; an overflow has no safe target narrower than the root.
            let mut roots = Vec::new();
            if update.lost {
                roots.push(index.root());
            }
            for path in &update.paths {
                let mut parent = index.root();
                for part in path.components() {
                    let found = index
                        .entries()
                        .find(|(_, e)| e.parent == Some(parent) && e.name == part.as_os_str())
                        .map(|(id, _)| id);
                    let Some(id) = found else {
                        break;
                    };
                    parent = id;
                }
                if let Some(entry) = index.get(parent) {
                    roots.push(if entry.kind == Kind::File {
                        entry.parent.unwrap_or(index.root())
                    } else {
                        parent
                    });
                }
            }
            // Roots first, then one forward pass that only reads the parent: `insert` appends
            // below an existing parent, so the parent's flag is always already decided. This is
            // one comparison per entry instead of a scan of up to 65 roots for each of them.
            let mut stale = vec![false; index.len()];
            for id in &roots {
                stale[id.position()] = true;
            }
            for (id, entry) in index.entries() {
                debug_assert!(
                    entry.parent.is_none_or(|p| p.position() < id.position()),
                    "the forward pass needs every parent before its children"
                );
                if entry.parent.is_some_and(|p| stale[p.position()]) {
                    stale[id.position()] = true;
                }
            }
            // Only an entry that is not already stale changes anything, so a notification that
            // repeats earlier news neither rewrites the index nor drops the cached map.
            let ids: Vec<_> = index
                .entries()
                .filter_map(|(id, entry)| {
                    (stale[id.position()] && entry.state != State::Stale).then_some(id)
                })
                .collect();
            for id in &ids {
                let _ = index.set_state(*id, State::Stale);
            }
            changed = !ids.is_empty();
            if changed {
                self.ghosts.clear();
                self.volume_evidence = None;
                self.observations = None;
                self.version = self.version.wrapping_add(1);
                self.cache = None;
                self.volume_map = None;
                self.last_layout = None;
                self.filter_changed = true;
            }
        }
        // An overflow changed the whole root, whether or not any entry was fresh.
        changed |= update.lost;
        let message = if update.lost {
            text::WATCH_OVERFLOW.to_string()
        } else {
            text::watch_changes(update.changes)
        };
        if changed {
            self.record_notice(message);
        } else {
            self.record_watch_notice(&message);
        }
    }
    pub(super) fn rescan_folder(&mut self) {
        if self.snapshot_file.is_some() {
            return;
        }
        let reason = if self.ghosts.count > 0 {
            text::RESCAN_FOLDER_GHOSTS
        } else if self.history.current().members.is_some() {
            text::RESCAN_FOLDER_MEMBERS
        } else if self
            .history
            .current()
            .resolve_opt(self.reducer.index())
            .is_none()
        {
            text::RESCAN_FOLDER_MISSING
        } else {
            text::RESCAN_FOLDER_APPEND_ONLY
        };
        self.rescan();
        // reset_scan retains this notice separately, so it survives the replacement scan.
        self.rescan_reason = Some(reason.into());
        self.record_notice(reason.into());
    }
    pub(super) fn finish_bookmark_navigation(&mut self) {
        if self.receiver.is_some() || self.work_pending() {
            return;
        }
        if let Some((root, location)) = self.pending_bookmark.take() {
            if self.scan_root.as_deref().or(self.saved_root.as_deref()) == root.as_deref()
                && location.resolve_opt(self.reducer.index()).is_some()
            {
                self.navigate(location);
            } else {
                self.record_notice(text::BOOKMARK_MISSING.into());
            }
        }
    }
    pub(super) fn load_bookmarks(&mut self) {
        self.bookmark_file =
            eframe::storage_dir(text::TITLE).map(|dir| dir.join(bookmarks::FILE_NAME));
        if let Some(path) = &self.bookmark_file {
            match bookmarks::load(path) {
                Ok(entries) => self.bookmarks = entries,
                Err(error) => self.record_notice(error.to_string()),
            }
        } else {
            self.record_notice(text::BOOKMARK_STORAGE_UNAVAILABLE.into());
        }
    }
    fn save_bookmarks(&mut self, entries: Vec<bookmarks::Bookmark>) -> bool {
        let Some(path) = &self.bookmark_file else {
            self.record_notice(text::BOOKMARK_STORAGE_UNAVAILABLE.into());
            return false;
        };
        match bookmarks::save(path, &entries) {
            Ok(()) => {
                self.bookmarks = entries;
                true
            }
            Err(error) => {
                self.record_notice(error.to_string());
                false
            }
        }
    }
    pub(super) fn bookmark_menu(&mut self, ui: &mut egui::Ui) {
        if ui
            .add_enabled(
                self.reducer.index().is_some(),
                egui::Button::new(text::ADD_BOOKMARK),
            )
            .clicked()
        {
            self.bookmark_name = Some(String::new());
            ui.close();
        }
        ui.menu_button(text::BOOKMARKS, |ui| {
            ui.label(text::BOOKMARK_PRIVACY);
            let mut visit = None;
            let mut remove = None;
            for (position, bookmark) in self.bookmarks.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui
                        .button(&bookmark.name)
                        .on_hover_text(text::bookmark_detail(bookmark))
                        .clicked()
                    {
                        visit = Some(bookmark.clone());
                    }
                    if ui.small_button(text::REMOVE_BOOKMARK).clicked() {
                        remove = Some(position);
                    }
                });
            }
            if self.bookmarks.is_empty() {
                ui.label(text::NO_BOOKMARKS);
            }
            if let Some(position) = remove {
                let mut entries = self.bookmarks.clone();
                entries.remove(position);
                self.save_bookmarks(entries);
            }
            if ui.button(text::CLEAR_BOOKMARKS).clicked() {
                self.save_bookmarks(Vec::new());
            }
            if let Some(bookmark) = visit {
                let location = Location {
                    path: bookmark.path,
                    members: bookmark.members.map(|names| Members {
                        names,
                        grouped_by: self.metric,
                        group_name: None,
                    }),
                };
                let current_root = self.scan_root.as_deref().or(self.saved_root.as_deref());
                if current_root == bookmark.root.as_deref() {
                    if location.resolve_opt(self.reducer.index()).is_some() {
                        self.navigate(location);
                    } else {
                        self.record_notice(text::BOOKMARK_MISSING.into());
                    }
                } else {
                    if let Some(root) = &bookmark.root {
                        self.start_real_scan(root.clone());
                    } else {
                        self.request_scan(ScanRequest::Sample);
                    }
                    self.pending_bookmark = Some((bookmark.root, location));
                }
                ui.close();
            }
        });
    }
    pub(super) fn show_bookmark_name(&mut self, context: &egui::Context) {
        let Some(name) = &mut self.bookmark_name else {
            return;
        };
        let mut add = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::new("bookmark-name")).show(context, |ui| {
            ui.heading(text::ADD_BOOKMARK);
            ui.label(text::BOOKMARK_PRIVACY);
            if let Some(message) = &self.menu_notice {
                ui.label(message);
            }
            let label = ui.label(text::BOOKMARK_NAME);
            ui.add(egui::TextEdit::singleline(name).char_limit(bookmarks::MAX_NAME_CHARS))
                .labelled_by(label.id);
            ui.horizontal(|ui| {
                add = ui
                    .add_enabled(
                        !name.trim().is_empty(),
                        egui::Button::new(text::SAVE_BOOKMARK),
                    )
                    .clicked();
                cancel = ui.button(text::CANCEL).clicked();
            });
        });
        if add {
            let location = self.history.current();
            let bookmark = bookmarks::Bookmark {
                name: name.trim().into(),
                root: self.scan_root.clone().or(self.saved_root.clone()),
                path: location.path,
                members: location.members.map(|m| m.names),
            };
            let mut entries = self.bookmarks.clone();
            match bookmarks::add(&mut entries, bookmark) {
                Ok(()) => {
                    if self.save_bookmarks(entries) {
                        self.bookmark_name = None;
                    }
                }
                Err(error) => self.record_notice(error.to_string()),
            }
        } else if cancel || modal.should_close() {
            self.bookmark_name = None;
        }
    }
    pub(super) fn notices_panel(&mut self, ui: &mut egui::Ui) -> NoticesHeader {
        let response = egui::Panel::bottom("notices").show(ui, |ui| {
            egui::CollapsingHeader::new(text::notice_count(self.notices.entries().count()))
                // The title carries the live count, so egui would derive a new id from it and the
                // panel would collapse itself whenever a notice arrived. The salt keeps the open
                // state tied to the panel, not to the number in its title.
                .id_salt("notices-console")
                .default_open(false)
                .show(ui, |ui| {
                    ui.checkbox(&mut self.notices.silent, text::SILENCE_NOTICES)
                        .on_hover_text(text::NOTICES_SESSION);
                    egui::ScrollArea::vertical()
                        .max_height(100.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for notice in self.notices.entries() {
                                let seconds = notice
                                    .time
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs()
                                    % 86400;
                                ui.label(format!(
                                    "{:02}:{:02}:{:02} UTC · {}",
                                    seconds / 3600,
                                    seconds / 60 % 60,
                                    seconds % 60,
                                    notice.text
                                ))
                                .on_hover_text(&notice.detail);
                            }
                        });
                })
        });
        NoticesHeader {
            id: response.inner.header_response.id,
            body_shown: response.inner.body_response.is_some(),
        }
    }
    pub(super) fn coverage_strip(&self, ui: &mut egui::Ui) {
        let Some(cache) = &self.cache else {
            return;
        };
        let palette = super::Theme::current(ui.ctx()).palette();
        let pixel = 1.0 / ui.ctx().pixels_per_point();
        let area = super::paint::rect(cache.key.rect);
        for (depth, band) in cache.coverage.iter().enumerate() {
            let total = band.complete + band.partial + band.unknown;
            if total == 0 {
                continue;
            }
            let x = area.min.x + depth as f32 * pixel;
            let mut y = area.min.y;
            for (count, color) in [
                (band.complete, palette.secondary_text),
                (band.partial, palette.incomplete.fill),
                (band.unknown, palette.separator),
            ] {
                let height = area.height() * count as f32 / total as f32;
                let rect = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(pixel, height));
                ui.painter().rect_filled(rect, 0.0, color);
                y += height;
            }
            let rect = egui::Rect::from_min_size(
                egui::pos2(x, area.min.y),
                egui::vec2(pixel, area.height()),
            );
            ui.interact(
                rect,
                ui.id().with(("coverage", depth)),
                egui::Sense::hover(),
            )
            .on_hover_text(text::coverage(
                depth,
                band.complete,
                band.partial,
                band.unknown,
            ));
        }
    }
}
impl Location {
    fn resolve_opt(&self, index: Option<&Index>) -> Option<hogmap::index::EntryId> {
        let (root, members) = self.resolve(index?)?;
        if self.members.as_ref().is_some_and(|wanted| {
            members
                .as_ref()
                .is_none_or(|found| found.len() != wanted.names.len())
        }) {
            return None;
        }
        Some(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn watcher_overflow_marks_the_whole_snapshot_stale_without_changing_bytes() {
        let context = egui::Context::default();
        let mut index = Index::new("sample".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        let child = index
            .insert(
                index.root(),
                "file".into(),
                Kind::File,
                hogmap::index::Sizes {
                    logical: Some(42),
                    allocated: Some(4096),
                },
                None,
                State::Complete,
            )
            .unwrap();
        let reducer = hogmap::scan::ScanReducer::from_snapshot(
            hogmap::scan::ScanGeneration::new(1),
            index,
            hogmap::scan::Terminal::Finished,
        );
        let mut app = HogMap::with_reducer(context, reducer);
        app.apply_watch_changes(&watch::Update {
            lost: true,
            ..Default::default()
        });
        let index = app.reducer.index().unwrap();
        assert_eq!(index.get(child).unwrap().state, State::Stale);
        assert_eq!(index.summary().unwrap().known_logical_bytes, 42);
        assert!(
            app.notices
                .entries()
                .last()
                .unwrap()
                .detail
                .contains("overflow")
        );
    }
    #[test]
    fn folder_rescan_falls_back_visibly_for_append_only_ghost_and_missing_locations() {
        let context = egui::Context::default();
        let mut app = HogMap::with_reducer(
            context,
            hogmap::scan::ScanReducer::new(hogmap::scan::ScanGeneration::new(0)),
        );
        app.history.visit(Location {
            path: vec!["different-root".into()],
            members: None,
        });
        app.rescan_folder();
        assert_eq!(
            app.rescan_reason.as_deref(),
            Some(text::RESCAN_FOLDER_MISSING)
        );
        app.ghosts.count = 1;
        app.rescan_folder();
        assert_eq!(
            app.rescan_reason.as_deref(),
            Some(text::RESCAN_FOLDER_GHOSTS)
        );
        assert!(app.menu_notice.as_ref().unwrap().contains("Full rescan"));
    }
    #[test]
    fn sealed_identity_corrections_use_an_explained_full_rescan() {
        use hogmap::index::{FileIdentity, Sizes};
        let mut index = Index::new("fixture".into()).unwrap();
        let identity = FileIdentity { volume: 1, file: 7 };
        for (name, bytes) in [("charged", 10), ("alias", 20)] {
            index
                .insert(
                    index.root(),
                    name.into(),
                    Kind::File,
                    Sizes {
                        logical: Some(bytes),
                        allocated: Some(bytes),
                    },
                    Some(identity),
                    State::Complete,
                )
                .unwrap();
        }
        index.set_state(index.root(), State::Complete).unwrap();
        index
            .apply_authoritative_sizes(
                identity,
                Sizes {
                    logical: Some(12),
                    allocated: Some(12),
                },
            )
            .unwrap();
        let reducer = hogmap::scan::ScanReducer::from_snapshot(
            hogmap::scan::ScanGeneration::new(80),
            index,
            hogmap::scan::Terminal::Finished,
        );
        let mut app = HogMap::with_reducer(egui::Context::default(), reducer);
        app.rescan_folder();
        assert_eq!(
            app.rescan_reason.as_deref(),
            Some(text::RESCAN_FOLDER_APPEND_ONLY)
        );
        assert!(app.menu_notice.as_ref().unwrap().contains("corrections"));
        assert!(matches!(app.pending_scan, Some(ScanRequest::Sample)));
    }
    #[test]
    fn coverage_separates_complete_pending_unknown_and_stale_visible_entries() {
        let mut index = Index::new("fixture".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        for (name, state, allocated) in [
            ("complete", State::Complete, Some(10)),
            ("stale", State::Stale, Some(10)),
            ("unknown", State::Complete, None),
        ] {
            index
                .insert(
                    index.root(),
                    name.into(),
                    Kind::File,
                    hogmap::index::Sizes {
                        logical: Some(10),
                        allocated,
                    },
                    None,
                    state,
                )
                .unwrap();
        }
        let layout = hogmap::treemap::layout(
            &index,
            index.root(),
            hogmap::treemap::Rect::new(0.0, 0.0, 800.0, 600.0),
            hogmap::treemap::LayoutParams {
                min_pixel_area: 1.0,
                header_height: 20.0,
                max_depth: 4,
            },
        )
        .unwrap();
        let bands = coverage(&index, &layout);
        assert_eq!(
            (bands[1].complete, bands[1].partial, bands[1].unknown),
            (1, 1, 1)
        );
    }

    #[test]
    fn the_notices_panel_stays_open_when_a_notice_arrives() {
        // The title carries the live count, so a header identified by its title would be a
        // different widget after every notice and would collapse itself. The salt keeps one id.
        // The id and the body come from the header egui really built, so removing the salt makes
        // both assertions fail instead of comparing a copy of the id with itself.
        let context = egui::Context::default();
        // Snap the collapsing animation: whether the body was shown then follows the open state
        // alone, not how much time the headless pass happened to advance.
        context.global_style_mut(|style| style.animation_time = 0.0);
        let mut index = Index::new("sample".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        let reducer = hogmap::scan::ScanReducer::from_snapshot(
            hogmap::scan::ScanGeneration::new(1),
            index,
            hogmap::scan::Terminal::Finished,
        );
        let mut app = HogMap::with_reducer(context.clone(), reducer);
        let draw = |app: &mut HogMap, context: &egui::Context| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let mut header = None;
            let _ = context.run_ui(input, |ui| {
                header = Some(app.notices_panel(ui));
            });
            header.expect("the notices panel is drawn")
        };
        let closed = draw(&mut app, &context);
        assert!(!closed.body_shown, "the panel starts closed");
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
            &context, closed.id, false,
        );
        state.set_open(true);
        state.store(&context);
        app.notices.push("first", "");
        let opened = draw(&mut app, &context);
        assert_eq!(
            closed.id, opened.id,
            "the header id must not depend on the count"
        );
        assert!(
            opened.body_shown,
            "the notices panel must stay open when a notice arrives"
        );
    }

    #[test]
    fn stopping_with_the_option_on_but_no_watcher_still_records_the_stop() {
        // The option can be on before the watcher exists: the scan has not finished, or a Rescan
        // retired the previous one. Stop must record the stop in that state too, and exactly once.
        let context = egui::Context::default();
        let mut index = Index::new("sample".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        let reducer = hogmap::scan::ScanReducer::from_snapshot(
            hogmap::scan::ScanGeneration::new(1),
            index,
            hogmap::scan::Terminal::Finished,
        );
        let mut app = HogMap::with_reducer(context, reducer);
        app.watch_enabled = true;
        assert!(app.watcher.is_none(), "no watcher has started yet");
        app.cancel();
        assert!(!app.watch_enabled, "the option is turned off");
        assert_eq!(
            app.notices
                .entries()
                .filter(|notice| notice.text == text::WATCH_STOPPED)
                .count(),
            1,
            "the stop is recorded once"
        );
    }
    #[test]
    fn two_real_changes_with_the_same_count_are_both_recorded_and_a_repeat_is_not() {
        // Two folders changed by one entry each produce the same message. Comparing text would
        // keep only the first line, so the console would lose a real event; a batch that changed
        // nothing is the only repeat worth dropping.
        let context = egui::Context::default();
        let mut index = Index::new("sample".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        for name in ["one", "two"] {
            index
                .insert(
                    index.root(),
                    name.into(),
                    Kind::Directory,
                    hogmap::index::Sizes::default(),
                    None,
                    State::Complete,
                )
                .unwrap();
        }
        let reducer = hogmap::scan::ScanReducer::from_snapshot(
            hogmap::scan::ScanGeneration::new(1),
            index,
            hogmap::scan::Terminal::Finished,
        );
        let mut app = HogMap::with_reducer(context, reducer);
        let batch = |name: &str| watch::Update {
            changes: 1,
            paths: vec![std::path::PathBuf::from(name)],
            ..Default::default()
        };
        app.apply_watch_changes(&batch("one"));
        assert_eq!(app.notices.entries().count(), 1);
        app.apply_watch_changes(&batch("two"));
        assert_eq!(
            app.notices.entries().count(),
            2,
            "a second real change is a second line"
        );
        // Both subtrees are stale now, so the same batch again changes nothing and adds nothing.
        app.apply_watch_changes(&batch("two"));
        assert_eq!(
            app.notices.entries().count(),
            2,
            "a no-op repeat is dropped"
        );
    }
    #[test]
    fn a_second_notification_over_an_already_stale_subtree_changes_nothing() {
        let context = egui::Context::default();
        let mut index = Index::new("sample".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        let folder = index
            .insert(
                index.root(),
                "folder".into(),
                Kind::Directory,
                hogmap::index::Sizes::default(),
                None,
                State::Complete,
            )
            .unwrap();
        index
            .insert(
                folder,
                "file".into(),
                Kind::File,
                hogmap::index::Sizes {
                    logical: Some(7),
                    allocated: Some(4096),
                },
                None,
                State::Complete,
            )
            .unwrap();
        let reducer = hogmap::scan::ScanReducer::from_snapshot(
            hogmap::scan::ScanGeneration::new(1),
            index,
            hogmap::scan::Terminal::Finished,
        );
        let mut app = HogMap::with_reducer(context, reducer);
        let batch = watch::Update {
            changes: 1,
            paths: vec![std::path::PathBuf::from("folder")],
            ..Default::default()
        };
        app.apply_watch_changes(&batch);
        let first = app.version;
        assert_eq!(
            app.reducer.index().unwrap().get(folder).unwrap().state,
            State::Stale
        );
        app.apply_watch_changes(&batch);
        assert_eq!(app.version, first, "nothing changed, so nothing is rebuilt");
        assert_eq!(app.notices.entries().count(), 1, "and nothing is recorded");
    }
    #[test]
    fn a_watcher_that_ends_without_an_error_is_retired_without_a_panic_notice() {
        // A worker can end without writing an error: a panic before the write, or an exit that
        // returns Ok. The slot must not stay occupied with the option on and the box checked.
        let context = egui::Context::default();
        let mut index = Index::new("sample".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        let reducer = hogmap::scan::ScanReducer::from_snapshot(
            hogmap::scan::ScanGeneration::new(1),
            index,
            hogmap::scan::Terminal::Finished,
        );
        let mut app = HogMap::with_reducer(context.clone(), reducer);
        let root =
            std::env::temp_dir().join(format!("hogmap-watch-dead-fixture-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        app.scan_root = Some(root.clone());
        app.watch_enabled = true;
        app.poll_watch(&context);
        assert!(
            app.watcher.is_some(),
            "the watcher starts for a finished scan"
        );
        // End the worker the way a cancel-and-return does: no error is written.
        app.watcher.as_ref().unwrap().stop();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.watcher.as_ref().unwrap().is_finished() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        app.poll_watch(&context);
        assert!(!app.watch_enabled, "the option is turned off");
        assert!(app.watcher.is_none(), "the dead watcher is retired");
        // It ended because it was cancelled, not because it panicked, so the console must not
        // claim a crash: a generic message here would replace a real error when there is one.
        assert!(
            !app.notices
                .entries()
                .any(|notice| notice.text == text::WATCH_PANIC),
            "a clean stop is not reported as a panic"
        );
        // The retired worker is reaped, and a new watch starts once the option is on again.
        app.watch_enabled = true;
        app.poll_watch(&context);
        app.poll_watch(&context);
        assert!(
            app.watcher
                .as_ref()
                .is_some_and(|watch| !watch.is_finished()),
            "a replacement watcher starts"
        );
        app.watch_enabled = false;
        app.stop_watch();
        while app.retired_watch.is_some() {
            assert!(std::time::Instant::now() < deadline);
            app.poll_watch(&context);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::remove_dir(&root).unwrap();
    }
}
