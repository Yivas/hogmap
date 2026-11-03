mod logic;
mod paint;
pub mod text;
mod theme;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, Modifiers};
use hogmap::filter::Match;
use hogmap::index::{EntryId, Index};
use hogmap::scan::{DEFAULT_CHANNEL_CAPACITY, ScanEvent, ScanGeneration, ScanReducer};
use hogmap::synthetic::{SyntheticSpec, run_synthetic};
use hogmap::treemap::{self, Layout, LayoutParams, NodeRef, Rect};
use logic::{HighlightFilter, History, Location};
use theme::Theme;

const LAYOUT_INTERVAL: Duration = Duration::from_millis(250);
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

struct Cache {
    layout: Layout,
    cells: Vec<paint::Cell>,
    matches: HashMap<EntryId, Match>,
    version: u64,
    location: Location,
    location_label: String,
    rect: Rect,
}

/// The indexed name of an entry; the map and the status line use the same label for a location.
fn entry_name(index: &Index, id: EntryId) -> String {
    index.get(id).map_or_else(String::new, |entry| {
        entry.name.to_string_lossy().into_owned()
    })
}

pub struct HogMap {
    /// Kept so `save` can read the theme from egui itself instead of holding a second copy.
    context: egui::Context,
    /// A choice from the View menu, applied at the start of the next frame so every panel of that
    /// frame uses the same palette.
    pending_theme: Option<Theme>,
    reducer: ScanReducer,
    receiver: Option<Receiver<ScanEvent>>,
    producer: Option<JoinHandle<()>>,
    cancellation: Arc<AtomicBool>,
    cancelled: bool,
    version: u64,
    history: History,
    selected: Option<NodeRef>,
    filter_text: String,
    filter_focused: bool,
    filter: HighlightFilter,
    filter_changed: bool,
    cache: Option<Cache>,
    last_layout: Option<Instant>,
    error: Option<String>,
    about: bool,
}

impl HogMap {
    pub fn new(context: &eframe::CreationContext<'_>) -> std::io::Result<Self> {
        // The native window stays hidden until the first frame is painted, so restoring the
        // stored choice here is enough to start in the saved theme. A missing, corrupt or future
        // value leaves the dark default.
        let theme = context.storage.and_then(Theme::load).unwrap_or_default();
        theme::apply(&context.egui_ctx, theme);
        let mut app = Self {
            context: context.egui_ctx.clone(),
            pending_theme: None,
            reducer: ScanReducer::new(ScanGeneration::new(0)),
            receiver: None,
            producer: None,
            cancellation: Arc::new(AtomicBool::new(false)),
            cancelled: false,
            version: 0,
            history: History::default(),
            selected: None,
            filter_text: String::new(),
            filter_focused: false,
            filter: HighlightFilter::default(),
            filter_changed: false,
            cache: None,
            last_layout: None,
            error: None,
            about: false,
        };
        app.start_scan()?;
        Ok(app)
    }

    /// Returns true when the replaced producer had to be joined and its thread panicked.
    fn stop_producer(&mut self) -> bool {
        self.cancellation.store(true, Ordering::Relaxed);
        // Drop the receiver before joining: the producer's terminal send ignores cancellation.
        self.receiver.take();
        if let Some(producer) = self.producer.take()
            && producer.join().is_err()
        {
            self.error = Some(text::PRODUCER_PANIC.into());
            return true;
        }
        false
    }

    fn start_scan(&mut self) -> std::io::Result<()> {
        let previous_failed = self.stop_producer();
        let generation = ScanGeneration::new(self.reducer.generation().value().wrapping_add(1));
        let (sender, receiver) = sync_channel(DEFAULT_CHANNEL_CAPACITY);
        let cancellation = Arc::new(AtomicBool::new(false));
        let flag = cancellation.clone();
        let producer = thread::Builder::new()
            .name("synthetic-scan".into())
            .spawn(move || {
                run_synthetic(
                    generation,
                    42,
                    SyntheticSpec {
                        root_name: text::SAMPLE.into(),
                        directories: 48,
                        files: 2400,
                        inaccessible_directories: 2,
                        hardlink_pairs: 12,
                        unknown_size_files: 8,
                        progress_every: 64,
                    },
                    sender,
                    &flag,
                );
            })?;
        self.reducer = ScanReducer::new(generation);
        self.cancellation = cancellation;
        self.receiver = Some(receiver);
        self.producer = Some(producer);
        self.cancelled = false;
        self.selected = None;
        self.cache = None;
        self.last_layout = None;
        // A panic in the replaced producer is this scan's only report; do not hide it.
        if !previous_failed {
            self.error = None;
        }
        self.version += 1;
        Ok(())
    }

    fn cancel(&mut self) {
        if self.receiver.is_some() {
            self.cancellation.store(true, Ordering::Relaxed);
            self.cancelled = true;
        }
    }

    fn drain(&mut self, context: &egui::Context) {
        if let Some(receiver) = &self.receiver {
            // One complete batch at most, plus a bounded set of control events, per frame.
            let report = self.reducer.drain(receiver, 1);
            if report.events > 0 || report.disconnected {
                self.version += 1;
            }
            if report.terminal.is_some() || report.disconnected {
                self.receiver.take();
            }
            context.request_repaint_after(FRAME_INTERVAL);
        }
    }

    fn navigate(&mut self, location: Location) {
        self.history.visit(location);
        self.selected = None;
    }

    fn can_zoom(&self) -> bool {
        self.selected.is_some_and(|node| match node {
            NodeRef::Aggregate { .. } => true,
            NodeRef::Entry(id) => self
                .reducer
                .index()
                .and_then(|index| index.get(id))
                .is_some_and(|entry| entry.kind == hogmap::index::Kind::Directory),
        })
    }

    fn zoom(&mut self, node: NodeRef) {
        let Some(index) = self.reducer.index() else {
            return;
        };
        let location =
            match node {
                NodeRef::Entry(id) => {
                    if index
                        .get(id)
                        .is_none_or(|entry| entry.kind != hogmap::index::Kind::Directory)
                    {
                        return;
                    }
                    Location::directory(index, id)
                }
                NodeRef::Aggregate { parent } => {
                    let Some(cell) = self.cache.as_ref().and_then(|cache| {
                        cache.layout.placed.iter().find(|cell| cell.node == node)
                    }) else {
                        return;
                    };
                    let mut location = Location::directory(index, parent);
                    location.members = Some(
                        cell.members
                            .iter()
                            .filter_map(|id| index.get(*id).map(|entry| entry.name.clone()))
                            .collect(),
                    );
                    location
                }
            };
        self.navigate(location);
    }

    fn shortcuts(&mut self, context: &egui::Context) {
        if context.input_mut(|input| {
            // Match egui's blur rule: a modified Escape must not cancel behind the field's back.
            !input.modifiers.any() && input.consume_key(Modifiers::NONE, Key::Escape)
        }) {
            // The highlight field owns Escape while it has focus: leave the field, keep scanning.
            // Egui clears a text edit's focus during the pass, so this frame no longer sees it as
            // focused; the response recorded by the last draw is the evidence.
            if self.filter_focused {
                self.filter_focused = false;
            } else {
                self.cancel();
            }
        }
        if context.text_edit_focused() {
            return;
        }
        if context.input_mut(|input| input.consume_key(Modifiers::ALT, Key::ArrowLeft)) {
            self.history.back();
            self.selected = None;
        }
        if context.input_mut(|input| input.consume_key(Modifiers::ALT, Key::ArrowRight)) {
            self.history.forward();
            self.selected = None;
        }
        if context.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Backspace)) {
            self.navigate(self.history.current().up());
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("controls").show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 3.0);
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button(text::FILE, |ui| {
                    if ui.button(text::NEW_SCAN).clicked() {
                        if let Err(error) = self.start_scan() {
                            self.error = Some(format!("{}: {error}", text::PRODUCER_ERROR));
                        }
                        ui.close();
                    }
                    if ui.button(text::EXIT).clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button(text::VIEW, |ui| {
                    if ui
                        .add_enabled(self.can_zoom(), egui::Button::new(text::ZOOM_IN))
                        .clicked()
                    {
                        if let Some(node) = self.selected {
                            self.zoom(node);
                        }
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.history.current() != Location::default(),
                            egui::Button::new(text::ZOOM_OUT),
                        )
                        .clicked()
                    {
                        self.navigate(self.history.current().up());
                        ui.close();
                    }
                    if ui.button(text::HOME).clicked() {
                        self.navigate(Location::default());
                        ui.close();
                    }
                    ui.separator();
                    // Both items are drawn every frame so the checkmark and the accessible state
                    // always match egui's theme; choosing one is applied on the next frame,
                    // without invalidating the cached layout or cells.
                    let current = Theme::current(ui.ctx());
                    let mut chosen = current;
                    ui.selectable_value(&mut chosen, Theme::Dark, text::THEME_DARK);
                    ui.selectable_value(&mut chosen, Theme::Light, text::THEME_LIGHT);
                    if chosen != current {
                        self.pending_theme = Some(chosen);
                        ui.close();
                    }
                });
                ui.menu_button(text::HELP, |ui| {
                    if ui.button(text::ABOUT).clicked() {
                        self.about = true;
                        ui.close();
                    }
                });
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.history.can_back(), egui::Button::new(text::BACK))
                    .clicked()
                {
                    self.history.back();
                    self.selected = None;
                }
                if ui
                    .add_enabled(self.history.can_forward(), egui::Button::new(text::FORWARD))
                    .clicked()
                {
                    self.history.forward();
                    self.selected = None;
                }
                if ui
                    .add_enabled(
                        self.history.current() != Location::default(),
                        egui::Button::new(text::UP),
                    )
                    .clicked()
                {
                    self.navigate(self.history.current().up());
                }
                if ui.button(text::HOME).clicked() {
                    self.navigate(Location::default());
                }
                ui.separator();
                if ui
                    .add_enabled(
                        self.receiver.is_some() && !self.cancelled,
                        egui::Button::new(text::CANCEL),
                    )
                    .clicked()
                {
                    self.cancel();
                }
            });
            ui.horizontal(|ui| {
                ui.label(text::FILTER);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.filter_text)
                        .hint_text(text::FILTER_HINT)
                        .desired_width(f32::INFINITY)
                        .char_limit(4097),
                );
                self.filter_focused = response.has_focus();
                if response.changed() && self.filter.set(&self.filter_text) {
                    self.filter_changed = true;
                }
            });
            let error_color = ui.visuals().error_fg_color;
            if let Some(error) = self.filter.error {
                ui.colored_label(error_color, text::filter_error(error));
            }
            if let Some(error) = &self.error {
                ui.colored_label(error_color, error);
            }
        });
    }

    fn status(&self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status").show(ui, |ui| {
            let state = text::scan_state(logic::scan_state(
                self.receiver.is_some(),
                self.cancelled,
                self.reducer.terminal(),
            ));
            if let Some(index) = self.reducer.index()
                && let Ok(summary) = index.summary()
            {
                // The cached label names the same resolved members as the drawn map, so the
                // small-item count cannot disagree with it; before the first layout only the path
                // is known.
                let location = match &self.cache {
                    Some(cache) => cache.location_label.clone(),
                    None => text::location(
                        &entry_name(index, index.root()),
                        &self.history.current().path,
                        None,
                    ),
                };
                let status = text::status(state, &location, summary, self.reducer.diagnostics());
                ui.add(egui::Label::new(&status).truncate())
                    .on_hover_text(status);
            } else {
                ui.label(state);
            }
        });
    }

    fn map(&mut self, ui: &mut egui::Ui) {
        let (area, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click());
        let rect = Rect::new(area.min.x, area.min.y, area.width(), area.height());
        let palette = Theme::current(ui.ctx()).palette();
        let Some(index) = self.reducer.index() else {
            ui.painter().text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                text::WAITING,
                egui::FontId::proportional(13.0),
                palette.message,
            );
            return;
        };
        if rect.width <= 1.0 || rect.height <= 1.0 {
            return;
        }
        let location = self.history.current();
        let dirty = self.cache.as_ref().is_none_or(|cache| {
            cache.version != self.version || cache.location != location || cache.rect != rect
        });
        let due = self
            .last_layout
            .is_none_or(|last| last.elapsed() >= LAYOUT_INTERVAL);
        if dirty && (self.receiver.is_none() || due) {
            // Resolve components against this generation; a missing path during rescan shows the
            // root temporarily, without replacing the history entry before its parent arrives.
            let (root, members) = location.resolve(index).unwrap_or((index.root(), None));
            let content_area = rect.area().max(1.0);
            let min_area = members.as_ref().map_or(400.0, |members| {
                // At least one member must escape the next aggregate, even on a tiny viewport.
                (content_area / (members.len().max(1) as f32 * 4.0)).clamp(0.001, 400.0)
            });
            let params = LayoutParams {
                min_pixel_area: min_area,
                header_height: 20.0,
            };
            let subset = members.as_deref().filter(|members| !members.is_empty());
            let subset_members = subset.map(<[EntryId]>::len);
            let location_label =
                text::location(&entry_name(index, root), &location.path, subset_members);
            let result = match subset {
                Some(members) => treemap::layout_members(index, root, members, rect, params),
                None => treemap::layout(index, root, rect, params),
            };
            match result {
                Ok(layout) => {
                    let cells = paint::prepare(ui, index, &layout, subset_members);
                    self.cache = Some(Cache {
                        layout,
                        cells,
                        matches: self.filter.matches(index),
                        version: self.version,
                        location,
                        location_label,
                        rect,
                    });
                    self.last_layout = Some(Instant::now());
                    self.filter_changed = false;
                }
                Err(error) => {
                    self.error = Some(format!("{}: {error:?}", text::LAYOUT_ERROR));
                    self.cache = None;
                }
            }
        }
        if self.filter_changed
            && let Some(cache) = &mut self.cache
        {
            cache.matches = self.filter.matches(index);
            self.filter_changed = false;
        }
        let Some(cache) = &self.cache else { return };
        // The cell controls below cover the map, so its own response is not hovered; ask whether
        // the pointer is over the map layer instead.
        let pointed = ui
            .ctx()
            .pointer_hover_pos()
            .filter(|_| ui.rect_contains_pointer(area))
            .and_then(|position| logic::hit_test(&cache.layout, position.x, position.y));
        paint::draw(
            ui,
            palette,
            &cache.layout,
            &cache.cells,
            &cache.matches,
            self.selected,
            pointed,
        );
        if cache.layout.known_sum == 0 {
            ui.painter().text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                text::EMPTY,
                egui::FontId::proportional(13.0),
                palette.message,
            );
        }
        let hovered = response
            .hover_pos()
            .and_then(|position| logic::hit_test(&cache.layout, position.x, position.y));
        if let Some(hovered) = hovered {
            response
                .clone()
                .on_hover_text(&cache.cells[hovered].tooltip);
        }
        let mut zoom = None;
        for (position, cell) in cache.layout.placed.iter().enumerate() {
            let interactive = paint::rect(cell.header.unwrap_or(cell.rect));
            let control = ui.interact(
                interactive,
                ui.id().with(("cell", position)),
                egui::Sense::click(),
            );
            control
                .clone()
                .on_hover_text(&cache.cells[position].tooltip);
            control.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    true,
                    self.selected == Some(cell.node),
                    &cache.cells[position].tooltip,
                )
            });
            if control.has_focus() {
                paint::focus_ring(ui.painter(), interactive, palette);
            }
            if control.clicked() {
                self.selected = Some(cell.node);
            }
            if control.double_clicked() {
                zoom = Some(cell.node);
            }
        }
        if let Some(position) = hovered {
            if response.clicked() {
                self.selected = Some(cache.layout.placed[position].node);
            }
            if response.double_clicked() {
                zoom = Some(cache.layout.placed[position].node);
            }
        }
        if let Some(node) = zoom {
            self.zoom(node);
        }
    }
}

impl eframe::App for HogMap {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain(context);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        // Applying a menu choice here keeps the whole frame on one palette; applying it inside
        // `controls` would leave the menu and the top panel on the previous one.
        if let Some(theme) = self.pending_theme.take() {
            theme::apply(&context, theme);
        }
        self.shortcuts(&context);
        self.controls(ui);
        self.status(ui);
        let map_background = Theme::current(&context).palette().map_background;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(ui.style())
                    .fill(map_background)
                    .inner_margin(2.0),
            )
            .show(ui, |ui| self.map(ui));
        if self.about {
            egui::Window::new(text::ABOUT)
                .open(&mut self.about)
                .resizable(false)
                .show(&context, |ui| {
                    ui.label(text::ABOUT_BODY);
                });
        }
    }

    fn persist_egui_memory(&self) -> bool {
        // Egui memory also holds widget and window state, which is more than the theme this app
        // stores; keeping it off leaves the file with the window geometry and the theme only.
        false
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // The stored choice is read back from egui, the same source the menu and the painter use;
        // a choice made in the current frame is saved before its deferred application.
        let theme = self
            .pending_theme
            .unwrap_or_else(|| Theme::current(&self.context));
        theme.store(storage);
    }
}

impl Drop for HogMap {
    fn drop(&mut self) {
        self.stop_producer();
    }
}
