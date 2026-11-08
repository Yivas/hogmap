mod dialog;
mod icons;
mod logic;
mod paint;
pub mod text;
mod theme;

use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, Modifiers};
use hogmap::filter::Match;
use hogmap::index::{EntryId, Index, SizeMetric};
use hogmap::scan::{
    DEFAULT_CHANNEL_CAPACITY, ScanCancellation, ScanGeneration, ScanReceiver, ScanReducer,
    Terminal, event_channel,
};
use hogmap::scan_win::{ScanOptions, ScanReport, run_win_scan_with_options};
use hogmap::synthetic::{SyntheticSpec, run_synthetic};
use hogmap::treemap::{self, Layout, LayoutError, LayoutParams, NodeRef, Rect};
use hogmap::volume::{self, volume_root_for};
use logic::{HighlightFilter, History, Location, Members};
use theme::Theme;

const LAYOUT_INTERVAL: Duration = Duration::from_millis(250);
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const DRAIN_BUDGET: Duration = Duration::from_millis(4);
// Hold a close request this long so returning threads can finish; then detach the rest.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

enum ScanRequest {
    Real(PathBuf),
    Sample,
}

type PreparedScan = Result<(PathBuf, dialog::DriveChoice), String>;

/// What a map was built for. The metric is part of the key, so geometry and labels built for one
/// metric are never shown, even partly, while the other one is selected.
#[derive(Debug, PartialEq)]
struct ViewKey {
    version: u64,
    location: Location,
    rect: Rect,
    metric: SizeMetric,
}

struct Cache {
    key: ViewKey,
    layout: Layout,
    cells: Vec<paint::Cell>,
    matches: Vec<Match>,
    match_counts: (usize, usize),
    location_label: String,
}

/// A map that could not be laid out for `key`. It stays apart from scan errors, so the scan keeps
/// its lifecycle and choosing a metric that works clears it; it is not retried until the key
/// changes.
struct LayoutFailure {
    key: ViewKey,
    message: String,
}

/// The indexed name of an entry; the map and the status line use the same label for a location.
fn entry_name(index: &Index, id: EntryId) -> String {
    index.get(id).map_or_else(String::new, |entry| {
        entry.name.to_string_lossy().into_owned()
    })
}

/// The caption of an opened aggregate's members as resolved in this snapshot. They are called
/// small only while the map uses the metric that grouped them.
fn subset_caption(
    location: &Location,
    members: Option<&[EntryId]>,
    metric: SizeMetric,
) -> Option<text::Subset> {
    let members = members.filter(|members| !members.is_empty())?;
    Some(text::Subset {
        members: members.len(),
        small: location.members_grouped_by(metric),
    })
}

pub struct HogMap {
    /// Kept so `save` can read the theme from egui itself instead of holding a second copy.
    context: egui::Context,
    /// A choice from the View menu, applied at the start of the next frame so every panel of that
    /// frame uses the same palette.
    pending_theme: Option<Theme>,
    reducer: ScanReducer,
    receiver: Option<ScanReceiver>,
    producer: Option<JoinHandle<Option<ScanReport>>>,
    // At most one retired producer: a replacement cannot start until it has returned.
    pending_producers: Vec<JoinHandle<Option<ScanReport>>>,
    pending_scan: Option<ScanRequest>,
    preparation: Option<JoinHandle<PreparedScan>>,
    closing: bool,
    close_hold: Option<Instant>,
    scan_report: Option<ScanReport>,
    scan_root: Option<PathBuf>,
    cancellation: ScanCancellation,
    cancelled: bool,
    version: u64,
    history: History,
    selected: Option<NodeRef>,
    filter_text: String,
    filter_focused: bool,
    filter: HighlightFilter,
    filter_changed: bool,
    /// The size the map weighs. Chosen in View for this session only: it survives a rescan but is
    /// never saved.
    metric: SizeMetric,
    cache: Option<Cache>,
    layout_failure: Option<LayoutFailure>,
    last_layout: Option<Instant>,
    error: Option<String>,
    about: bool,
    chooser: dialog::SpaceDialog,
    scan_started: Option<Instant>,
    scan_elapsed: Duration,
    focus_filter: bool,
    volume: Option<dialog::DriveChoice>,
}

impl HogMap {
    pub fn new(context: &eframe::CreationContext<'_>) -> std::io::Result<Self> {
        // The native window stays hidden until the first frame is painted, so restoring the
        // stored choice here is enough to start in the saved theme. A missing, corrupt or future
        // value leaves the dark default.
        let theme = context.storage.and_then(Theme::load).unwrap_or_default();
        theme::apply(&context.egui_ctx, theme);
        Ok(Self::with_context(context.egui_ctx.clone()))
    }

    fn with_context(context: egui::Context) -> Self {
        Self {
            context,
            pending_theme: None,
            reducer: ScanReducer::new(ScanGeneration::new(0)),
            receiver: None,
            producer: None,
            pending_producers: Vec::new(),
            pending_scan: None,
            preparation: None,
            closing: false,
            close_hold: None,
            scan_report: None,
            scan_root: None,
            cancellation: ScanCancellation::new(),
            cancelled: false,
            version: 0,
            history: History::default(),
            selected: None,
            filter_text: String::new(),
            filter_focused: false,
            filter: HighlightFilter::default(),
            filter_changed: false,
            metric: SizeMetric::Logical,
            cache: None,
            layout_failure: None,
            last_layout: None,
            error: None,
            about: false,
            chooser: dialog::SpaceDialog::default(),
            scan_started: None,
            scan_elapsed: Duration::ZERO,
            focus_filter: false,
            volume: None,
        }
    }

    fn stop_producer(&mut self) {
        self.cancellation.cancel();
        // A terminal send ignores cancellation, so disconnect before retiring the producer.
        self.receiver.take();
        if let Some(producer) = self.producer.take() {
            self.pending_producers.push(producer);
        }
    }

    fn request_close(&mut self) {
        self.closing = true;
        self.pending_scan = None;
        self.stop_producer();
    }

    fn request_scan(&mut self, request: ScanRequest) {
        if self.closing {
            return;
        }
        self.stop_producer();
        // Coalesce repeated requests instead of starting more threads behind blocked I/O.
        self.pending_scan = Some(request);
    }

    /// Whether a producer, a retired producer or the preparation task still owns a thread.
    fn work_pending(&self) -> bool {
        self.producer.is_some()
            || !self.pending_producers.is_empty()
            || self.preparation.is_some()
            || self.pending_scan.is_some()
    }

    fn poll_pending(&mut self) {
        let mut position = 0;
        while position < self.pending_producers.len() {
            if self.pending_producers[position].is_finished() {
                if self.pending_producers.swap_remove(position).join().is_err() {
                    self.error = Some(text::PRODUCER_PANIC.into());
                }
            } else {
                position += 1;
            }
        }
    }

    fn advance_scan(&mut self) {
        if self
            .preparation
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let result = self.preparation.take().unwrap().join();
            self.chooser.preparing = false;
            if !self.cancellation.is_cancelled() && !self.closing {
                match result {
                    Ok(Ok((root, volume))) => self.launch_real_scan(root, volume),
                    Ok(Err(error)) => {
                        self.chooser.open = true;
                        self.chooser.error = Some(error);
                    }
                    Err(_) => self.error = Some(text::PRODUCER_PANIC.into()),
                }
            } else if result.is_err() {
                self.error = Some(text::PRODUCER_PANIC.into());
            }
        }
        if !self.pending_producers.is_empty() || self.preparation.is_some() || self.closing {
            return;
        }
        match self.pending_scan.take() {
            Some(ScanRequest::Real(root)) => {
                self.cancellation = ScanCancellation::new();
                match thread::Builder::new()
                    .name("prepare-scan".into())
                    .spawn(move || {
                        dialog::validate_root(&root)?;
                        // Query drives here instead of reusing the chooser's cache: on the first
                        // Start its listing may still be running, and a rescan must not reuse
                        // capacity captured when the chooser last refreshed.
                        let volume_root = volume_root_for(&root);
                        let volume = dialog::volume_choice(
                            &root,
                            volume_root.as_deref(),
                            volume::list_drives().unwrap_or_default(),
                        );
                        Ok((root, volume))
                    }) {
                    Ok(worker) => {
                        self.preparation = Some(worker);
                        self.chooser.preparing = true;
                    }
                    Err(error) => self.error = Some(format!("{}: {error}", text::PRODUCER_ERROR)),
                }
            }
            Some(ScanRequest::Sample) => {
                if let Err(error) = self.launch_sample() {
                    self.error = Some(format!("{}: {error}", text::PRODUCER_ERROR));
                }
            }
            None => {}
        }
    }

    fn collect_producer(&mut self) {
        if !self.producer.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        if let Some(producer) = self.producer.take() {
            match producer.join() {
                Ok(report) => self.scan_report = report,
                Err(_) => self.error = Some(text::PRODUCER_PANIC.into()),
            }
        }
    }

    fn start_real_scan(&mut self, root: PathBuf) {
        self.request_scan(ScanRequest::Real(root));
    }

    fn launch_real_scan(&mut self, root: PathBuf, volume: dialog::DriveChoice) {
        let previous_failed = self.error.as_deref() == Some(text::PRODUCER_PANIC);
        let generation = ScanGeneration::new(self.reducer.generation().value().wrapping_add(1));
        let (sender, receiver) = event_channel(DEFAULT_CHANNEL_CAPACITY);
        let context = self.context.clone();
        sender.set_waker(move || context.request_repaint());
        let cancellation = ScanCancellation::new();
        let flag = cancellation.clone();
        let scan_root = root.clone();
        let producer = thread::Builder::new()
            .name("windows-scan".into())
            .spawn(move || {
                let (_, report) = run_win_scan_with_options(
                    generation,
                    scan_root,
                    sender,
                    &flag,
                    ScanOptions::default(),
                );
                Some(report)
            });
        let producer = match producer {
            Ok(producer) => producer,
            Err(error) => {
                let message = format!("{}: {error}", text::PRODUCER_ERROR);
                self.chooser.error = Some(message.clone());
                self.error = Some(message);
                return;
            }
        };
        self.volume = Some(volume);
        self.scan_root = Some(root);
        self.reset_scan(
            generation,
            receiver,
            producer,
            cancellation,
            previous_failed,
        );
    }

    fn rescan(&mut self) {
        if let Some(root) = self.scan_root.clone() {
            self.start_real_scan(root);
        } else {
            self.request_scan(ScanRequest::Sample);
        }
    }

    fn launch_sample(&mut self) -> std::io::Result<()> {
        let previous_failed = self.error.as_deref() == Some(text::PRODUCER_PANIC);
        let generation = ScanGeneration::new(self.reducer.generation().value().wrapping_add(1));
        let (sender, receiver) = event_channel(DEFAULT_CHANNEL_CAPACITY);
        let context = self.context.clone();
        sender.set_waker(move || context.request_repaint());
        let cancellation = ScanCancellation::new();
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
                None
            })?;
        self.volume = None;
        self.scan_root = None;
        self.reset_scan(
            generation,
            receiver,
            producer,
            cancellation,
            previous_failed,
        );
        Ok(())
    }

    fn reset_scan(
        &mut self,
        generation: ScanGeneration,
        receiver: ScanReceiver,
        producer: JoinHandle<Option<ScanReport>>,
        cancellation: ScanCancellation,
        previous_failed: bool,
    ) {
        self.reducer = ScanReducer::new(generation);
        self.cancellation = cancellation;
        self.receiver = Some(receiver);
        self.producer = Some(producer);
        self.cancelled = false;
        self.scan_started = Some(Instant::now());
        self.scan_elapsed = Duration::ZERO;
        self.chooser.open = false;
        self.chooser.error = None;
        self.scan_report = None;
        self.history = History::default();
        self.selected = None;
        self.cache = None;
        self.layout_failure = None;
        self.last_layout = None;
        // A panic in the replaced producer is this scan's only report; do not hide it.
        if !previous_failed {
            self.error = None;
        }
        self.version += 1;
    }

    fn cancel(&mut self) {
        if !(self.receiver.is_some() || self.preparation.is_some() || self.pending_scan.is_some()) {
            return;
        }
        self.cancellation.cancel();
        self.cancelled = true;
        if self.preparation.is_some() || self.pending_scan.is_some() {
            self.pending_scan = None;
            self.chooser.preparing = false;
        }
    }

    fn drain(&mut self, context: &egui::Context) {
        if let Some(receiver) = &self.receiver {
            let report = self
                .reducer
                .drain_budgeted(receiver, usize::MAX, DRAIN_BUDGET);
            if receiver.has_pending() {
                context.request_repaint();
            }
            if report.events > 0 || report.disconnected {
                self.version += 1;
            }
            if let Some(started) = self.scan_started {
                self.scan_elapsed = started.elapsed();
            }
            if let Some(Terminal::Failed(code)) = report.terminal {
                self.error = Some(text::scan_failure(code));
            } else if report.disconnected && report.terminal.is_none() {
                self.error = Some(text::MISSING_TERMINAL.into());
            }
            if report.terminal.is_some() || report.disconnected {
                self.receiver.take();
            }
            context.request_repaint_after(FRAME_INTERVAL);
        }
        // The terminal may arrive just before the thread returns its counts-only report.
        // `collect_producer` joins only an already finished thread.
        self.collect_producer();
        self.poll_pending();
        self.chooser.poll_drives(context);
        self.advance_scan();
        if self.work_pending() {
            context.request_repaint_after(FRAME_INTERVAL);
        } else if self.closing {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// A layout failure of the selected metric. One left by the other metric is not reported while
    /// the map is being rebuilt after a switch.
    fn current_layout_failure(&self) -> Option<&LayoutFailure> {
        self.layout_failure
            .as_ref()
            .filter(|failure| failure.key.metric == self.metric)
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
        let location = match node {
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
                let Some(cache) = self.cache.as_ref() else {
                    return;
                };
                let Some(cell) = cache.layout.placed.iter().find(|cell| cell.node == node) else {
                    return;
                };
                let mut location = Location::directory(index, parent);
                location.members = Some(Members {
                    names: cell
                        .members
                        .iter()
                        .filter_map(|id| index.get(*id).map(|entry| entry.name.to_os_string()))
                        .collect(),
                    grouped_by: cache.key.metric,
                });
                location
            }
        };
        self.navigate(location);
    }

    fn shortcuts(&mut self, context: &egui::Context) {
        if self.chooser.open {
            return;
        }
        if context.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::N)) {
            self.chooser.reopen();
            return;
        }
        if context.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::F)) {
            self.focus_filter = true;
        }
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
        if context.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F5))
            && self.scan_started.is_some()
        {
            self.rescan();
        }
        if context.input_mut(|input| input.consume_key(Modifiers::ALT, Key::Home)) {
            self.navigate(Location::default());
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
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button(text::FILE, |ui| {
                    if ui.button(text::NEW_SCAN).clicked() {
                        self.chooser.reopen();
                        ui.close();
                    }
                    if ui.button(text::SAMPLE_ACTION).clicked() {
                        self.request_scan(ScanRequest::Sample);
                        ui.close();
                    }
                    if ui.button(text::EXIT).clicked() {
                        self.request_close();
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
                    // The map pass later in this frame rebuilds for the new metric before it draws.
                    let mut metric = self.metric;
                    ui.selectable_value(&mut metric, SizeMetric::Logical, text::METRIC_LOGICAL);
                    ui.selectable_value(&mut metric, SizeMetric::Allocated, text::METRIC_ALLOCATED);
                    if metric != self.metric {
                        self.metric = metric;
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
            ui.separator();
            ui.horizontal(|ui| {
                use icons::{Icon, button};
                if button(ui, Icon::NewScan, true, false, text::NEW_SCAN_TIP).clicked() {
                    self.chooser.reopen();
                }
                ui.separator();
                if button(
                    ui,
                    Icon::Back,
                    self.history.can_back(),
                    false,
                    text::BACK_TIP,
                )
                .clicked()
                {
                    self.history.back();
                    self.selected = None;
                }
                if button(
                    ui,
                    Icon::Forward,
                    self.history.can_forward(),
                    false,
                    text::FORWARD_TIP,
                )
                .clicked()
                {
                    self.history.forward();
                    self.selected = None;
                }
                if button(
                    ui,
                    Icon::Up,
                    self.history.current() != Location::default(),
                    false,
                    text::UP_TIP,
                )
                .clicked()
                {
                    self.navigate(self.history.current().up());
                }
                if button(
                    ui,
                    Icon::Home,
                    self.scan_started.is_some(),
                    false,
                    text::HOME_TIP,
                )
                .clicked()
                {
                    self.navigate(Location::default());
                }
                ui.separator();
                if button(
                    ui,
                    Icon::Rescan,
                    self.scan_started.is_some(),
                    false,
                    text::RESCAN_TIP,
                )
                .clicked()
                {
                    self.rescan();
                }
                let can_stop = !self.cancelled
                    && (self.receiver.is_some()
                        || self.preparation.is_some()
                        || self.pending_scan.is_some());
                if button(ui, Icon::Stop, can_stop, false, text::STOP_TIP).clicked() {
                    self.cancel();
                }
                button(ui, Icon::Export, false, false, text::EXPORT_TIP);
                ui.separator();
                button(ui, Icon::Nested, false, false, text::VIEW_MODE_TIP);
                button(ui, Icon::Group, false, false, text::GROUP_BY_TIP);
                if button(
                    ui,
                    Icon::Highlight,
                    true,
                    self.filter_focused,
                    text::HIGHLIGHT_TIP,
                )
                .clicked()
                {
                    self.focus_filter = true;
                }
                button(ui, Icon::Ghosts, false, false, text::GHOSTS_TIP);
                ui.separator();
                button(ui, Icon::Options, false, false, text::OPTIONS_TIP);
            });
            ui.separator();
            ui.horizontal(|ui| {
                let label = ui.label(text::FILTER);
                let (matches, unknown) = self
                    .cache
                    .as_ref()
                    .map_or((0, 0), |cache| cache.match_counts);
                let count = text::match_count(matches, unknown);
                let response = ui
                    .add_sized(
                        egui::vec2((ui.available_width() - 176.0).max(80.0), 24.0),
                        egui::TextEdit::singleline(&mut self.filter_text)
                            .hint_text(text::FILTER_HINT)
                            .char_limit(4097),
                    )
                    .labelled_by(label.id);
                if self.focus_filter {
                    response.request_focus();
                    self.focus_filter = false;
                }
                self.filter_focused = response.has_focus();
                if response.changed() && self.filter.set(&self.filter_text) {
                    self.filter_changed = true;
                }
                if logic::show_match_count(&self.filter_text) {
                    ui.label(
                        egui::RichText::new(count)
                            .small()
                            .color(Theme::current(ui.ctx()).palette().secondary_text),
                    );
                }
            });
            let error_color = ui.visuals().error_fg_color;
            // Reserve one message line so valid/invalid filter input never shifts the map.
            ui.allocate_ui(egui::vec2(ui.available_width(), 16.0), |ui| {
                if let Some(error) = self.filter.error {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(text::filter_error(error))
                                .small()
                                .color(error_color),
                        )
                        .truncate(),
                    )
                    .on_hover_text(text::filter_error(error));
                } else if let Some(error) = &self.error {
                    ui.add(
                        egui::Label::new(egui::RichText::new(error).small().color(error_color))
                            .truncate(),
                    )
                    .on_hover_text(error);
                } else if let Some(failure) = self.current_layout_failure() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&failure.message)
                                .small()
                                .color(error_color),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&failure.message);
                } else {
                    ui.label(
                        egui::RichText::new(text::map_metric(self.metric))
                            .small()
                            .color(Theme::current(ui.ctx()).palette().secondary_text),
                    );
                }
            });
        });
    }

    fn status(&self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .exact_size(56.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let state = logic::scan_state(
                    self.receiver.is_some(),
                    self.cancelled,
                    self.reducer.terminal(),
                );
                let stopping = !self.pending_producers.is_empty()
                    || self.closing
                    || (self.cancelled && self.producer.is_some())
                    || (self.preparation.is_some() && self.cancellation.is_cancelled());
                let lifecycle = if stopping {
                    text::STOPPING
                } else if self.preparation.is_some() {
                    text::PREPARING_SCAN
                } else if self.scan_started.is_none() {
                    text::READY
                } else {
                    text::scan_state(state)
                };
                // Ready describes an idle producer, never the completeness of its metadata.
                let master = match state {
                    logic::ScanState::Finished if !stopping && self.preparation.is_none() => {
                        text::READY
                    }
                    _ => lifecycle,
                };
                if let Some(index) = self.reducer.index() {
                    let totals = logic::metric_totals(index, self.metric);
                    let location = match &self.cache {
                        Some(cache) if cache.key.metric == self.metric => {
                            cache.location_label.clone()
                        }
                        // A metric chosen in this frame is laid out after the status is drawn;
                        // caption the members for the new metric instead of repeating the old one.
                        Some(_) => {
                            let current = self.history.current();
                            let members = current.resolve(index).and_then(|(_, members)| members);
                            text::location(
                                &entry_name(index, index.root()),
                                &current.path,
                                subset_caption(&current, members.as_deref(), self.metric),
                            )
                        }
                        None => text::location(
                            &entry_name(index, index.root()),
                            &self.history.current().path,
                            None,
                        ),
                    };
                    let mut detail =
                        text::status(lifecycle, &location, &totals, self.reducer.diagnostics());
                    if let Some(report) = &self.scan_report {
                        detail.push('\n');
                        detail.push_str(&text::scan_report(report));
                    }
                    if let Some(error) = &self.error {
                        detail.push('\n');
                        detail.push_str(error);
                    }
                    if let Some(failure) = self.current_layout_failure() {
                        detail.push('\n');
                        detail.push_str(&failure.message);
                    }
                    let master = text::master_status(
                        master,
                        self.reducer.progress().records as usize,
                        &totals,
                        self.scan_elapsed,
                    );
                    let accessible = format!("{master}\n{detail}");
                    let status = ui.add(egui::Label::new(master).truncate());
                    status.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &accessible)
                    });
                    status.on_hover_text(&detail);
                    ui.horizontal(|ui| {
                        ui.strong(text::completeness(&totals))
                            .on_hover_text(&detail);
                        ui.separator();
                        ui.add(egui::Label::new(location).truncate())
                            .on_hover_text(detail);
                    });
                } else {
                    let detail = self
                        .scan_report
                        .as_ref()
                        .map(text::scan_report)
                        .unwrap_or_else(|| master.to_owned());
                    let accessible = format!("{master}\n{detail}");
                    let status = ui.label(master);
                    status.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &accessible)
                    });
                    status.on_hover_text(&detail);
                    ui.label(
                        egui::RichText::new(if let Some(error) = &self.error {
                            error.as_str()
                        } else if self.scan_started.is_some() {
                            text::WAITING
                        } else {
                            text::NO_SCAN
                        })
                        .small(),
                    );
                }
            });
    }

    fn volume_header(&self, ui: &mut egui::Ui) {
        let palette = Theme::current(ui.ctx()).palette();
        egui::Panel::top("volume-header")
            .exact_size(32.0)
            .frame(
                egui::Frame::new()
                    .fill(palette.volume_header)
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ui, |ui| {
                ui.visuals_mut().override_text_color = Some(palette.volume_text);
                ui.horizontal(|ui| {
                    let (icon_rect, _) =
                        ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
                    icons::draw(
                        ui.painter(),
                        icons::Icon::Drive,
                        icon_rect.center(),
                        22.0,
                        palette.accent,
                    );
                    let root = self
                        .volume
                        .as_ref()
                        .map(|volume| {
                            if volume.label.is_empty() {
                                volume.root.display().to_string()
                            } else {
                                format!("{} · {}", volume.root.display(), volume.label)
                            }
                        })
                        .unwrap_or_else(|| {
                            if self.scan_started.is_some() {
                                text::SAMPLE.into()
                            } else {
                                text::NO_SPACE.into()
                            }
                        });
                    let capacity = self.volume.as_ref().map_or_else(
                        || text::drive_capacity(None, None),
                        |volume| text::drive_capacity(volume.free_bytes, volume.total_bytes),
                    );
                    let capacity_width = ui
                        .painter()
                        .layout_no_wrap(
                            capacity.clone(),
                            egui::FontId::proportional(13.0),
                            palette.volume_text,
                        )
                        .size()
                        .x;
                    ui.add_sized(
                        egui::vec2(
                            (ui.available_width() - capacity_width - 16.0).max(64.0),
                            24.0,
                        ),
                        egui::Label::new(egui::RichText::new(&root).strong()).truncate(),
                    )
                    .on_hover_text(&root);
                    let detail = text::capacity_detail(
                        self.volume
                            .as_ref()
                            .and_then(|volume| volume.capacity.as_ref()),
                        std::time::SystemTime::now(),
                    );
                    let response = ui.label(&capacity);
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Label,
                            true,
                            format!("{capacity}. {detail}"),
                        )
                    });
                    response.on_hover_text(detail);
                });
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
                if self.scan_started.is_some() {
                    text::WAITING
                } else {
                    text::CHOOSE_HINT
                },
                egui::FontId::proportional(13.0),
                palette.message,
            );
            return;
        };
        if rect.width <= 1.0 || rect.height <= 1.0 {
            return;
        }
        let key = ViewKey {
            version: self.version,
            location: self.history.current(),
            rect,
            metric: self.metric,
        };
        let shown = self
            .cache
            .as_ref()
            .map(|cache| &cache.key)
            .or(self.layout_failure.as_ref().map(|failure| &failure.key));
        let dirty = shown != Some(&key);
        // The scan throttle limits growth updates; it never keeps the previous metric's cells on
        // screen once the other metric is chosen, so an explicit switch rebuilds at once.
        let switched = shown.is_some_and(|shown| shown.metric != key.metric);
        let due = self
            .last_layout
            .is_none_or(|last| last.elapsed() >= LAYOUT_INTERVAL);
        if dirty && (self.receiver.is_none() || due || switched) {
            let location = &key.location;
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
            let caption = subset_caption(location, subset, self.metric);
            let location_label =
                text::location(&entry_name(index, index.root()), &location.path, caption);
            let result = match subset {
                Some(members) => treemap::layout_members_with_metric(
                    index,
                    root,
                    members,
                    rect,
                    params,
                    self.metric,
                ),
                None => treemap::layout_with_metric(index, root, rect, params, self.metric),
            };
            // A single map is cached: the previous one is dropped once this one replaces it, and
            // only its highlight results can carry over.
            let previous = self.cache.take();
            self.layout_failure = None;
            self.last_layout = Some(Instant::now());
            if switched {
                self.selected = logic::retained_selection(
                    self.selected,
                    previous.as_ref().map(|cache| &cache.layout),
                    result.as_ref().ok(),
                );
            }
            match result {
                Ok(layout) => {
                    let cells = paint::prepare(ui, index, &layout, caption, self.metric);
                    // Highlight decisions depend on the snapshot and the filter, not on the
                    // metric or the location, so they are reused while both are unchanged.
                    let (matches, match_counts) = match previous {
                        Some(previous)
                            if previous.key.version == key.version && !self.filter_changed =>
                        {
                            (previous.matches, previous.match_counts)
                        }
                        _ => {
                            let matches = self.filter.matches(index);
                            let counts = logic::match_counts(index, &matches);
                            (matches, counts)
                        }
                    };
                    self.cache = Some(Cache {
                        key,
                        layout,
                        cells,
                        matches,
                        match_counts,
                        location_label,
                    });
                    self.filter_changed = false;
                }
                Err(error) => {
                    // The other metric is offered only when its own totals did not overflow.
                    let other_available = error == LayoutError::SizeOverflow
                        && index.summary_for(logic::other_metric(self.metric)).is_ok();
                    self.layout_failure = Some(LayoutFailure {
                        message: text::layout_error(self.metric, error, other_available),
                        key,
                    });
                }
            }
        }
        if self.filter_changed
            && let Some(cache) = &mut self.cache
        {
            cache.matches = self.filter.matches(index);
            cache.match_counts = logic::match_counts(index, &cache.matches);
            self.filter_changed = false;
        }
        let Some(cache) = &self.cache else {
            if let Some(failure) = &self.layout_failure {
                ui.painter().text(
                    area.center(),
                    egui::Align2::CENTER_CENTER,
                    &failure.message,
                    egui::FontId::proportional(13.0),
                    palette.message,
                );
            }
            return;
        };
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
                text::empty_map(cache.key.metric),
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
        if context.input(|input| input.viewport().close_requested()) {
            self.request_close();
            if self.work_pending() {
                // Hold the first close so workers that return soon finish on their own.
                self.close_hold.get_or_insert_with(Instant::now);
                context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        if let Some(started) = self.close_hold {
            if !self.work_pending() || started.elapsed() >= CLOSE_GRACE {
                // The grace period is over: close and let Drop detach any blocked thread.
                self.close_hold = None;
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                context.request_repaint_after(FRAME_INTERVAL);
            }
        }
        if !self.closing {
            self.shortcuts(&context);
        }
        self.controls(ui);
        self.status(ui);
        self.volume_header(ui);
        let map_background = Theme::current(&context).palette().map_background;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(ui.style())
                    .fill(map_background)
                    .inner_margin(2.0),
            )
            .show(ui, |ui| self.map(ui));
        if let Some(action) = self.chooser.show(&context, self.scan_started.is_some()) {
            match action {
                dialog::Action::Start(root) => self.start_real_scan(root),
                dialog::Action::Sample => self.request_scan(ScanRequest::Sample),
                dialog::Action::Close => {
                    self.chooser.open = false;
                    if self.preparation.is_some() || self.pending_scan.is_some() {
                        self.cancel();
                    }
                }
            }
        }
        if self.about {
            let about = text::about(self.metric);
            egui::Window::new(text::ABOUT)
                .open(&mut self.about)
                .resizable(false)
                .show(&context, |ui| {
                    ui.label(about);
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
        self.poll_pending();
        // Drop cannot wait for synchronous Win32 I/O. Normal close polls above; forced teardown
        // detaches unfinished handles, which end with the process, rather than freezing the UI.
        if self
            .preparation
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let _ = self.preparation.take().unwrap().join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hogmap::scan::ScanEvent;
    use std::sync::mpsc::channel;

    #[test]
    fn replacing_a_blocked_producer_coalesces_requests_without_joining_it() {
        let context = egui::Context::default();
        let mut app = HogMap::with_context(context.clone());
        let (release, wait) = channel();
        let (returned, completion) = channel();
        app.producer = Some(thread::spawn(move || {
            wait.recv().unwrap();
            returned.send(()).unwrap();
            None
        }));
        let flag = app.cancellation.clone();
        let (sender, receiver) = event_channel(1);
        app.receiver = Some(receiver);
        app.request_scan(ScanRequest::Sample);
        app.request_scan(ScanRequest::Real(PathBuf::from("replacement")));
        app.advance_scan();
        assert!(flag.is_cancelled());
        assert!(
            sender
                .send(ScanEvent {
                    generation: ScanGeneration::new(0),
                    event: hogmap::scan::ScanEventKind::Terminal(Terminal::Cancelled)
                })
                .is_err()
        );
        assert!(app.producer.is_none());
        assert_eq!(app.pending_producers.len(), 1);
        assert!(matches!(app.pending_scan, Some(ScanRequest::Real(_))));
        // No real scan is launched: the latest request is replaced with the sample.
        app.request_scan(ScanRequest::Sample);
        release.send(()).unwrap();
        completion.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.reducer.generation().value() == 0 {
            app.drain(&context);
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(app.pending_producers.is_empty());
        assert!(app.scan_root.is_none());
    }

    fn finished_sample(context: &egui::Context) -> HogMap {
        let mut app = HogMap::with_context(context.clone());
        app.request_scan(ScanRequest::Sample);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.drain(context);
            if app.reducer.terminal().is_some() && app.producer.is_none() {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        app
    }

    /// One headless pass of the map; no native window or GL context is created.
    fn draw_map(context: &egui::Context, app: &mut HogMap) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = context.run_ui(input, |ui| app.map(ui));
    }

    #[test]
    fn a_nested_folder_label_starts_at_the_dataset_root_in_both_metrics() {
        let context = egui::Context::default();
        let mut app = finished_sample(&context);
        let index = app.reducer.index().unwrap();
        let root = index.root();
        // A folder two levels down, so the current folder differs from the dataset root.
        let (nested, parent) = index
            .entries()
            .find_map(|(id, entry)| {
                let parent = entry.parent.filter(|parent| *parent != root)?;
                let grandparent = index.get(parent)?.parent;
                (entry.kind == hogmap::index::Kind::Directory && grandparent == Some(root))
                    .then_some((id, parent))
            })
            .expect("the sample nests folders");
        let expected = format!(
            "{} / {} / {}",
            entry_name(index, root),
            entry_name(index, parent),
            entry_name(index, nested)
        );
        let location = Location::directory(index, nested);
        app.navigate(location.clone());
        for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
            app.metric = metric;
            draw_map(&context, &mut app);
            let cache = app.cache.as_ref().unwrap();
            assert_eq!((&cache.key.location, cache.key.metric), (&location, metric));
            assert_eq!(cache.layout.placed[0].node, NodeRef::Entry(nested));
            assert_eq!(cache.location_label, expected);
        }
    }

    #[test]
    fn switching_metric_relays_out_at_once_while_scan_updates_stay_throttled() {
        let context = egui::Context::default();
        let mut app = finished_sample(&context);
        draw_map(&context, &mut app);
        let logical = app.cache.as_ref().unwrap();
        assert_eq!(logical.key.metric, SizeMetric::Logical);
        let logical_layout = logical.layout.clone();
        let built = logical.key.version;

        // Events still arriving: growth waits for the layout interval.
        let (_sender, receiver) = event_channel(1);
        app.receiver = Some(receiver);
        app.last_layout = Some(Instant::now());
        app.version += 1;
        draw_map(&context, &mut app);
        assert_eq!(app.cache.as_ref().unwrap().key.version, built);

        app.metric = SizeMetric::Allocated;
        draw_map(&context, &mut app);
        let cache = app.cache.as_ref().unwrap();
        assert_eq!(cache.key.metric, SizeMetric::Allocated);
        assert_eq!(cache.key.version, app.version);
        let index = app.reducer.index().unwrap();
        let expected = treemap::layout_with_metric(
            index,
            index.root(),
            cache.key.rect,
            LayoutParams {
                min_pixel_area: 400.0,
                header_height: 20.0,
            },
            SizeMetric::Allocated,
        )
        .unwrap();
        assert_eq!(cache.layout, expected);
        assert_ne!(cache.layout, logical_layout);
        assert!(
            cache
                .cells
                .iter()
                .all(|cell| cell.tooltip.contains("observed allocated")),
            "no cell keeps a logical label"
        );
    }

    #[test]
    fn opened_members_survive_a_metric_switch_with_a_neutral_caption() {
        let context = egui::Context::default();
        let mut app = finished_sample(&context);
        draw_map(&context, &mut app);
        let placed = &app.cache.as_ref().unwrap().layout.placed;
        let aggregate = placed
            .iter()
            .find(|cell| matches!(cell.node, NodeRef::Aggregate { .. }))
            .cloned()
            .expect("the sample groups small items");
        let NodeRef::Aggregate { parent } = aggregate.node else {
            unreachable!()
        };
        let count = aggregate.members.len();
        app.zoom(aggregate.node);
        let opened = app.history.current();
        draw_map(&context, &mut app);
        let small = format!("· {count} small item");
        let label = &app.cache.as_ref().unwrap().location_label;
        assert!(label.contains(&small));

        app.metric = SizeMetric::Allocated;
        draw_map(&context, &mut app);
        assert_eq!(app.history.current(), opened);
        let cache = app.cache.as_ref().unwrap();
        assert!(
            cache
                .location_label
                .contains(&format!("· {count} grouped item"))
        );
        assert_eq!(cache.layout.placed[0].node, NodeRef::Entry(parent));
        let index = app.reducer.index().unwrap();
        let expected = treemap::layout_members_with_metric(
            index,
            parent,
            &aggregate.members,
            cache.key.rect,
            LayoutParams {
                min_pixel_area: 1.0,
                header_height: 20.0,
            },
            SizeMetric::Allocated,
        )
        .unwrap();
        assert_eq!(cache.layout.known_sum, expected.known_sum);
        assert_ne!(cache.layout.known_sum, aggregate.known_sum);

        app.metric = SizeMetric::Logical;
        draw_map(&context, &mut app);
        let label = &app.cache.as_ref().unwrap().location_label;
        assert!(label.contains(&small), "the grouping metric is shown again");
    }

    #[test]
    fn the_metric_choice_is_not_saved() {
        #[derive(Default)]
        struct MemoryStorage(std::collections::HashMap<String, String>);

        impl eframe::Storage for MemoryStorage {
            fn get_string(&self, key: &str) -> Option<String> {
                self.0.get(key).cloned()
            }

            fn set_string(&mut self, key: &str, value: String) {
                self.0.insert(key.to_owned(), value);
            }

            fn remove_string(&mut self, key: &str) {
                self.0.remove(key);
            }

            fn flush(&mut self) {}
        }

        let mut app = HogMap::with_context(egui::Context::default());
        app.metric = SizeMetric::Allocated;
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        assert_eq!(storage.0.keys().collect::<Vec<_>>(), ["theme"]);
    }

    #[test]
    fn rescan_replaces_snapshot_navigation_selection_header_and_report() {
        let context = egui::Context::default();
        let mut app = finished_sample(&context);
        assert_eq!(app.reducer.diagnostics().directory_failures, 2);
        assert!(
            app.error.is_none(),
            "ordinary directory failures are status details, not fatal errors"
        );
        let generation = app.reducer.generation();
        let root = app.reducer.index().unwrap().root();
        app.selected = Some(NodeRef::Entry(root));
        app.history.visit(Location {
            path: vec!["old".into()],
            members: None,
        });
        app.scan_report = Some(ScanReport::default());
        app.volume = Some(dialog::DriveChoice {
            root: "old".into(),
            label: "old".into(),
            free_bytes: Some(1),
            total_bytes: Some(2),
            capacity: None,
        });
        app.metric = SizeMetric::Allocated;
        app.rescan();
        app.drain(&context);
        assert_ne!(app.reducer.generation(), generation);
        assert!(app.reducer.index().is_none());
        assert_eq!(app.history.current(), Location::default());
        assert!(!app.history.can_back());
        assert!(app.selected.is_none());
        assert!(app.cache.is_none());
        assert!(app.scan_report.is_none());
        assert!(app.volume.is_none());
        assert!(app.error.is_none());
        assert_eq!(
            app.metric,
            SizeMetric::Allocated,
            "the view choice outlives the snapshot"
        );
    }
}
