use std::path::{Component, Path, PathBuf};
use std::thread::{self, JoinHandle};

use hogmap::scan_win::ScanError;
use hogmap::volume::{self, CapacityMetadata, VolumeInfo};

use eframe::egui::{self, Key, Modifiers, Stroke, StrokeKind};

use super::{
    icons::{self, Icon},
    paint, text,
    theme::Theme,
};

#[derive(Clone)]
pub struct DriveChoice {
    pub root: PathBuf,
    pub label: String,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub capacity: Option<CapacityMetadata>,
}

impl From<VolumeInfo> for DriveChoice {
    fn from(volume: VolumeInfo) -> Self {
        Self {
            root: PathBuf::from(volume.root),
            label: volume.label.unwrap_or_default(),
            free_bytes: volume.free_bytes,
            total_bytes: volume.total_bytes,
            capacity: Some(volume.capacity),
        }
    }
}

pub fn drives() -> Result<Vec<DriveChoice>, ScanError> {
    volume::list_drives().map(|drives| drives.into_iter().map(DriveChoice::from).collect())
}

/// Pair the selected path with a freshly queried drive entry. The chooser's cached list is not
/// used: it may still be loading on the first Start and can be stale when a scan is rescanned.
pub fn volume_choice(
    root: &Path,
    volume_root: Option<&str>,
    drives: Vec<VolumeInfo>,
) -> DriveChoice {
    volume_root
        .and_then(|volume_root| {
            drives
                .into_iter()
                .find(|drive| drive.root.eq_ignore_ascii_case(volume_root))
        })
        .map_or_else(
            || DriveChoice {
                root: root.to_path_buf(),
                label: String::new(),
                free_bytes: None,
                total_bytes: None,
                capacity: None,
            },
            DriveChoice::from,
        )
}

/// Inspect each prefix before the directory check, without canonicalizing through a junction.
/// Like the engine's checks, this cannot rule out concurrent path replacement.
pub fn validate_root(root: &Path) -> Result<(), String> {
    let absolute = std::path::absolute(root).map_err(|error| text::path_error(&error))?;
    let mut prefix = PathBuf::new();
    for component in absolute.components() {
        if matches!(component, Component::ParentDir) {
            return Err(text::PARENT_PATH.into());
        }
        prefix.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata =
            std::fs::symlink_metadata(&prefix).map_err(|error| text::path_error(&error))?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
        };
        #[cfg(not(windows))]
        let reparse = metadata.file_type().is_symlink();
        if reparse {
            return Err(text::scan_code(hogmap::scan_win::SKIP_REPARSE_CODE));
        }
    }
    let metadata = std::fs::metadata(root).map_err(|error| text::path_error(&error))?;
    if !metadata.is_dir() {
        return Err(text::NOT_DIRECTORY.into());
    }
    Ok(())
}

struct DriveColumns {
    name: egui::Rect,
    free: egui::Rect,
    total: egui::Rect,
    bar: Option<egui::Rect>,
}
impl DriveColumns {
    fn new(area: egui::Rect, capacity_width: f32) -> Self {
        let bar = (area.width() >= 640.0).then(|| {
            egui::Rect::from_min_max(
                egui::pos2(area.right() - 96.0, area.top()),
                egui::pos2(area.right() - 8.0, area.bottom()),
            )
        });
        let right = bar.map_or(area.right(), |bar| bar.left()) - 8.0;
        let total = egui::Rect::from_min_max(
            egui::pos2(right - capacity_width, area.top()),
            egui::pos2(right, area.bottom()),
        );
        let free = total.translate(egui::vec2(-capacity_width - 8.0, 0.0));
        let name = egui::Rect::from_min_max(
            egui::pos2(area.left() + 40.0, area.top()),
            egui::pos2(free.left() - 8.0, area.bottom()),
        );
        Self {
            name,
            free,
            total,
            bar,
        }
    }
}

pub enum Action {
    Start(PathBuf),
    Sample,
    Close,
}

pub struct SpaceDialog {
    pub open: bool,
    path: String,
    drives: Vec<DriveChoice>,
    drive_worker: Option<JoinHandle<Result<Vec<DriveChoice>, ScanError>>>,
    pub preparing: bool,
    selected: Option<usize>,
    focus_path: bool,
    drive_error: Option<String>,
    pub error: Option<String>,
}

impl Default for SpaceDialog {
    fn default() -> Self {
        let mut dialog = Self {
            open: true,
            path: String::new(),
            drives: Vec::new(),
            drive_worker: None,
            preparing: false,
            selected: None,
            focus_path: true,
            drive_error: None,
            error: None,
        };
        dialog.refresh_drives();
        dialog
    }
}

impl SpaceDialog {
    pub fn reopen(&mut self) {
        self.open = true;
        self.selected = None;
        self.focus_path = true;
        self.error = None;
    }

    fn refresh_drives(&mut self) {
        if self.drive_worker.is_some() {
            return;
        }
        self.selected = None;
        match thread::Builder::new()
            .name("list-drives".into())
            .spawn(drives)
        {
            Ok(worker) => self.drive_worker = Some(worker),
            Err(error) => self.drive_error = Some(format!("{}: {error}", text::DRIVE_THREAD_ERROR)),
        }
    }

    pub fn poll_drives(&mut self, context: &egui::Context) {
        if self
            .drive_worker
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            match self.drive_worker.take().unwrap().join() {
                Ok(Ok(drives)) => {
                    self.drives = drives;
                    self.drive_error = None;
                }
                Ok(Err(error)) => self.drive_error = Some(text::drive_error(error.code)),
                Err(_) => self.drive_error = Some(text::DRIVE_THREAD_ERROR.into()),
            }
        }
        if self.drive_worker.is_some() {
            context.request_repaint_after(super::FRAME_INTERVAL);
        }
    }

    pub fn show(&mut self, context: &egui::Context, has_scan: bool) -> Option<Action> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let palette = Theme::current(context).palette();
        let width = (context.content_rect().width() - 64.0).clamp(360.0, 720.0);
        let modal = egui::Modal::new(egui::Id::new("choose-space"))
            .frame(egui::Frame::popup(&context.global_style()).inner_margin(16.0))
            .show(context, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                ui.horizontal(|ui| {
                    ui.heading(text::CHOOSE_SPACE);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // The button also cancels a pending preparation before the first scan,
                        // when the modal blocks the toolbar underneath it.
                        if (has_scan || self.preparing) && ui.button(text::CLOSE).clicked() {
                            action = Some(Action::Close);
                        }
                    });
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            self.drive_worker.is_none(),
                            egui::Button::new(text::REFRESH_DRIVES),
                        )
                        .clicked()
                    {
                        self.refresh_drives();
                    }
                });
                if self.drive_worker.is_some() {
                    ui.label(text::LISTING_DRIVES);
                }
                if self.preparing {
                    ui.label(text::PREPARING_SCAN);
                }
                ui.separator();
                {
                    let number_font = egui::FontId::monospace(13.0);
                    let capacity_width = self
                        .drives
                        .iter()
                        .flat_map(|drive| [drive.free_bytes, drive.total_bytes])
                        .map(|value| {
                            ui.painter()
                                .layout_no_wrap(
                                    text::capacity_value(value),
                                    number_font.clone(),
                                    palette.secondary_text,
                                )
                                .size()
                                .x
                        })
                        .fold(96.0_f32, f32::max);
                    let capacity_width = ((capacity_width + 8.0) / 8.0).ceil() * 8.0;
                    let (heading, _) =
                        ui.allocate_exact_size(egui::vec2(width, 24.0), egui::Sense::hover());
                    let visible_rows = self.drives.len().clamp(1, 8).min(
                        ((context.content_rect().height() - 280.0) / 44.0)
                            .floor()
                            .max(1.0) as usize,
                    );
                    let choices = egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .id_salt("drive-choices")
                        .max_height(visible_rows as f32 * 44.0 - 4.0)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            if self.drives.is_empty() {
                                ui.add_space(40.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        self.drive_error
                                            .as_deref()
                                            .unwrap_or(text::DRIVES_UNAVAILABLE),
                                    );
                                });
                            } else {
                                for (position, drive) in self.drives.iter().enumerate() {
                                    let selected = self.selected == Some(position);
                                    let (rect, response) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), 40.0),
                                        egui::Sense::click(),
                                    );
                                    let hover = context.animate_bool_with_time(
                                        response.id.with("hover"),
                                        response.hovered(),
                                        0.1,
                                    );
                                    let selection = context.animate_bool_with_time(
                                        response.id.with("selection"),
                                        selected,
                                        0.1,
                                    );
                                    let fill = palette
                                        .chrome
                                        .lerp_to_gamma(palette.tool_hover, hover)
                                        .lerp_to_gamma(palette.tool_selected, selection);
                                    ui.painter().rect_filled(rect, 0, fill);
                                    ui.painter().hline(
                                        rect.x_range(),
                                        rect.bottom(),
                                        Stroke::new(1.0, palette.separator),
                                    );
                                    if selected {
                                        ui.painter().rect_stroke(
                                            rect,
                                            0,
                                            Stroke::new(1.0, palette.accent),
                                            StrokeKind::Inside,
                                        );
                                    }
                                    icons::draw(
                                        ui.painter(),
                                        Icon::Drive,
                                        rect.left_center() + egui::vec2(16.0, 0.0),
                                        22.0,
                                        palette.icon,
                                    );
                                    let root = drive.root.to_string_lossy();
                                    let caption = if drive.label.is_empty() {
                                        root.to_string()
                                    } else {
                                        format!("{root}  {}", drive.label)
                                    };
                                    let capacity =
                                        text::drive_capacity(drive.free_bytes, drive.total_bytes);
                                    let capacity_detail = text::capacity_detail(
                                        drive.capacity.as_ref(),
                                        std::time::SystemTime::now(),
                                    );
                                    let columns = DriveColumns::new(rect, capacity_width);
                                    let mut job = egui::text::LayoutJob::simple_singleline(
                                        caption.clone(),
                                        egui::FontId::proportional(13.0),
                                        palette.icon,
                                    );
                                    job.wrap.max_width = columns.name.width();
                                    job.wrap.max_rows = 1;
                                    let caption_galley =
                                        ui.fonts_mut(|fonts| fonts.layout_job(job));
                                    ui.painter().galley(
                                        egui::pos2(
                                            columns.name.left(),
                                            rect.center().y - caption_galley.size().y / 2.0,
                                        ),
                                        caption_galley,
                                        palette.icon,
                                    );
                                    for (area, value) in [
                                        (columns.free, drive.free_bytes),
                                        (columns.total, drive.total_bytes),
                                    ] {
                                        ui.painter().text(
                                            area.right_center(),
                                            egui::Align2::RIGHT_CENTER,
                                            text::capacity_value(value),
                                            number_font.clone(),
                                            palette.secondary_text,
                                        );
                                    }
                                    if let (Some(area), Some(free), Some(total)) =
                                        (columns.bar, drive.free_bytes, drive.total_bytes)
                                        && total > 0
                                        && free <= total
                                    {
                                        let bar = egui::Rect::from_center_size(
                                            area.center(),
                                            egui::vec2(area.width(), 4.0),
                                        );
                                        ui.painter().rect_filled(bar, 0, palette.separator);
                                        let used = (1.0 - free as f64 / total as f64) as f32;
                                        ui.painter().rect_filled(
                                            egui::Rect::from_min_size(
                                                bar.min,
                                                egui::vec2(bar.width() * used, 4.0),
                                            ),
                                            0,
                                            palette.secondary_text,
                                        );
                                    }
                                    response.widget_info(|| {
                                        egui::WidgetInfo::selected(
                                            egui::WidgetType::Button,
                                            true,
                                            selected,
                                            format!("{caption}, {capacity}. {capacity_detail}"),
                                        )
                                    });
                                    if response.has_focus() {
                                        paint::focus_ring(ui.painter(), rect.shrink(2.0), palette);
                                    }
                                    if response.clicked() {
                                        self.selected = Some(position);
                                        self.path = root.to_string();
                                    }
                                    if !self.preparing
                                        && (response.double_clicked()
                                            || (response.has_focus()
                                                && context.input_mut(|input| {
                                                    input.consume_key(Modifiers::NONE, Key::Enter)
                                                })))
                                    {
                                        action = Some(Action::Start(drive.root.clone()));
                                    }
                                    response.on_hover_text(format!(
                                        "{caption}\n{capacity}\n{capacity_detail}"
                                    ));
                                }
                            }
                        });
                    let heading = egui::Rect::from_min_size(
                        heading.min,
                        egui::vec2(choices.inner_rect.width(), heading.height()),
                    );
                    let columns = DriveColumns::new(heading, capacity_width);
                    for (area, label, align) in [
                        (columns.name, text::DRIVE, egui::Align2::LEFT_CENTER),
                        (columns.free, text::FREE, egui::Align2::RIGHT_CENTER),
                        (columns.total, text::TOTAL, egui::Align2::RIGHT_CENTER),
                    ] {
                        let position = if align == egui::Align2::LEFT_CENTER {
                            area.left_center()
                        } else {
                            area.right_center()
                        };
                        ui.painter().text(
                            position,
                            align,
                            label,
                            egui::FontId::proportional(12.0),
                            palette.secondary_text,
                        );
                    }
                    if let Some(bar) = columns.bar {
                        ui.painter().text(
                            bar.center(),
                            egui::Align2::CENTER_CENTER,
                            text::USED_FREE,
                            egui::FontId::proportional(12.0),
                            palette.secondary_text,
                        );
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        let label = ui.label(text::PATH);
                        let path = ui
                            .add(
                                egui::TextEdit::singleline(&mut self.path)
                                    .id_salt("scan-path")
                                    .hint_text(text::PATH_HINT)
                                    .desired_width((width - 116.0).max(120.0))
                                    .char_limit(32767),
                            )
                            .labelled_by(label.id);
                        if self.focus_path {
                            path.request_focus();
                            self.focus_path = false;
                        }
                        if path.changed() {
                            self.selected = None;
                            self.error = None;
                        }
                        let enter = path.lost_focus()
                            && context.input(|input| input.key_pressed(Key::Enter));
                        let start = ui
                            .add_enabled(
                                !self.preparing && !self.path.trim().is_empty(),
                                egui::Button::new(text::START)
                                    .fill(palette.tool_selected)
                                    .stroke(Stroke::new(1.0, palette.accent)),
                            )
                            .clicked();
                        if !self.preparing && (enter || start) && !self.path.trim().is_empty() {
                            action = Some(Action::Start(PathBuf::from(self.path.trim())));
                        }
                    });
                    if let Some(error) = &self.error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                }
                ui.separator();
                if ui
                    .add_enabled(!self.preparing, egui::Button::new(text::SAMPLE_ACTION))
                    .clicked()
                {
                    action = Some(Action::Sample);
                }
            });
        modal.response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Window, true, text::CHOOSE_SPACE)
        });
        // Even at startup consume Escape. Without a scan or a pending preparation it must not
        // dismiss the only route into a scan; while preparing, Escape cancels that preparation.
        if modal.should_close() && (has_scan || self.preparing) {
            action = Some(Action::Close);
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capacity_metadata(issue: Option<volume::CapacityIssue>) -> CapacityMetadata {
        CapacityMetadata {
            source: volume::CapacitySource::WindowsActualGlobalFilesystem,
            captured_at: std::time::SystemTime::UNIX_EPOCH,
            issue,
        }
    }

    fn escape(context: &egui::Context, dialog: &mut SpaceDialog, has_scan: bool) -> Option<Action> {
        let mut action = None;
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            ..Default::default()
        };
        let _ = context.run_ui(input, |_| {
            action = dialog.show(context, has_scan);
        });
        action
    }

    #[test]
    fn volume_choices_preserve_independent_measurements_and_full_volumes() {
        let volume = VolumeInfo {
            root: r"X:\".into(),
            label: Some("Fixture".into()),
            filesystem: Some("NTFS".into()),
            total_bytes: Some(8192),
            free_bytes: Some(2048),
            storage_reserve_bytes: Some(0),
            total_reserved_bytes: Some(0),
            reserve_issue: None,
            capacity: capacity_metadata(None),
        };
        let choice = DriveChoice::from(volume.clone());
        assert_eq!(choice.capacity, Some(volume.capacity));
        assert_eq!(choice.root, PathBuf::from(r"X:\"));
        assert_eq!(choice.label, "Fixture");
        assert_eq!(
            (choice.free_bytes, choice.total_bytes),
            (Some(2048), Some(8192))
        );
        let failed = capacity_metadata(Some(volume::CapacityIssue::QueryFailed(-2147467263)));
        let choice = DriveChoice::from(VolumeInfo {
            free_bytes: None,
            total_bytes: None,
            capacity: failed,
            ..volume.clone()
        });
        assert_eq!(choice.capacity, Some(failed));
        assert_eq!(choice.label, "Fixture");
        assert_eq!((choice.free_bytes, choice.total_bytes), (None, None));
        let choice = DriveChoice::from(VolumeInfo {
            label: None,
            filesystem: None,
            free_bytes: Some(0),
            ..volume.clone()
        });
        assert!(choice.label.is_empty());
        assert_eq!(
            (choice.free_bytes, choice.total_bytes),
            (Some(0), Some(8192))
        );
        assert_eq!(
            text::drive_capacity(choice.free_bytes, choice.total_bytes),
            "0 B free / 8.0 KiB total"
        );
        let choice = DriveChoice::from(VolumeInfo {
            total_bytes: None,
            ..volume
        });
        assert_eq!((choice.free_bytes, choice.total_bytes), (Some(2048), None));
    }

    #[test]
    fn a_fresh_drive_query_supplies_capacity_before_the_chooser_has_any() {
        let fresh = vec![
            VolumeInfo {
                root: r"X:\".into(),
                label: Some("Fresh".into()),
                filesystem: Some("NTFS".into()),
                total_bytes: Some(8192),
                free_bytes: Some(4096),
                storage_reserve_bytes: Some(0),
                total_reserved_bytes: Some(0),
                reserve_issue: None,
                capacity: capacity_metadata(None),
            },
            VolumeInfo {
                root: r"Y:\".into(),
                label: Some("Other".into()),
                filesystem: Some("NTFS".into()),
                total_bytes: Some(1024),
                free_bytes: Some(0),
                storage_reserve_bytes: Some(0),
                total_reserved_bytes: Some(0),
                reserve_issue: None,
                capacity: capacity_metadata(None),
            },
        ];
        let choice = volume_choice(Path::new(r"X:\fixture"), Some(r"x:\"), fresh);
        // A matched volume is shown by its root, as before; an unmatched path keeps the selection.
        assert_eq!(choice.root, PathBuf::from(r"X:\"));
        assert_eq!(choice.label, "Fresh");
        assert_eq!(
            (choice.free_bytes, choice.total_bytes),
            (Some(4096), Some(8192))
        );
        assert_eq!(choice.capacity, Some(capacity_metadata(None)));
        let failed = capacity_metadata(Some(volume::CapacityIssue::IncoherentFreeSpace));
        let choice = volume_choice(
            Path::new(r"X:\fixture"),
            Some(r"X:\"),
            vec![VolumeInfo {
                root: r"X:\".into(),
                label: Some("Still labelled".into()),
                filesystem: None,
                total_bytes: None,
                free_bytes: None,
                storage_reserve_bytes: None,
                total_reserved_bytes: None,
                reserve_issue: failed.issue,
                capacity: failed,
            }],
        );
        assert_eq!(choice.capacity, Some(failed));
        assert_eq!((choice.total_bytes, choice.free_bytes), (None, None));
        assert_eq!(choice.label, "Still labelled");
        // An empty listing (first Start still loading) or an unknown volume keeps unknown numbers.
        let choice = volume_choice(
            Path::new(r"\\server\share\folder"),
            Some(r"\\server\share\"),
            Vec::new(),
        );
        assert_eq!(choice.root, PathBuf::from(r"\\server\share\folder"));
        assert_eq!(choice.capacity, None);
        assert!(choice.label.is_empty());
        assert_eq!((choice.free_bytes, choice.total_bytes), (None, None));
        let choice = volume_choice(Path::new(r"X:\fixture"), None, Vec::new());
        assert_eq!(choice.root, PathBuf::from(r"X:\fixture"));
        assert_eq!((choice.free_bytes, choice.total_bytes), (None, None));
    }

    #[test]
    fn path_validation_requires_an_existing_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(validate_root(root).is_ok());
        assert_eq!(
            validate_root(&root.join("Cargo.toml")),
            Err(text::NOT_DIRECTORY.into())
        );
        assert!(validate_root(&root.join("missing-scan-root-fixture")).is_err());
    }

    #[test]
    fn drive_rows_keep_capacity_visible_and_support_keyboard_selection() {
        for theme in [Theme::Dark, Theme::Light] {
            for size in [egui::vec2(1914.0, 1035.0), egui::vec2(1280.0, 720.0)] {
                let context = egui::Context::default();
                context.enable_accesskit();
                super::super::theme::apply(&context, theme);
                let mut dialog = SpaceDialog {
                    open: true,
                    path: String::new(),
                    drives: vec![
                        DriveChoice {
                            root: r"X:\".into(),
                            label: "Fixture".into(),
                            free_bytes: Some(0),
                            total_bytes: Some(8192),
                            capacity: None,
                        },
                        DriveChoice {
                            root: r"Y:\".into(),
                            label: "Data".into(),
                            free_bytes: Some(2048),
                            total_bytes: Some(8192),
                            capacity: None,
                        },
                        DriveChoice {
                            root: r"Z:\".into(),
                            label: "Unknown".into(),
                            free_bytes: None,
                            total_bytes: None,
                            capacity: None,
                        },
                        DriveChoice {
                            root: r"W:\".into(),
                            label: "Capacity ceiling".into(),
                            free_bytes: Some(u64::MAX),
                            total_bytes: Some(u64::MAX),
                            capacity: None,
                        },
                    ],
                    drive_worker: None,
                    preparing: false,
                    selected: None,
                    focus_path: true,
                    drive_error: None,
                    error: None,
                };
                let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                let mut pass = |events: Vec<egui::Event>| {
                    let mut action = None;
                    let output = context.run_ui(
                        egui::RawInput {
                            screen_rect: Some(viewport),
                            events,
                            ..Default::default()
                        },
                        |_| {
                            action = dialog.show(&context, false);
                        },
                    );
                    (output, action)
                };
                pass(Vec::new());
                let (output, _) = pass(Vec::new());
                let nodes = &output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .unwrap()
                    .nodes;
                let mut rows = Vec::new();
                for prefix in [
                    "X:\\  Fixture, 0 B free / 8.0 KiB total",
                    "Y:\\  Data, 2.0 KiB free / 8.0 KiB total",
                    "Z:\\  Unknown, unknown free / unknown total",
                    "W:\\  Capacity ceiling, 17179869184.0 GiB free / 17179869184.0 GiB total",
                ] {
                    let node = nodes
                        .iter()
                        .map(|(_, node)| node)
                        .find(|node| node.label().is_some_and(|label| label.starts_with(prefix)))
                        .expect("drive row has a full accessible name");
                    assert!(node.supports_action(egui::accesskit::Action::Focus));
                    let bounds = node.bounds().unwrap();
                    let rect = egui::Rect::from_min_max(
                        egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
                        egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
                    );
                    assert!(viewport.contains_rect(rect));
                    assert_eq!(rect.height(), 40.0);
                    rows.push(rect);
                }
                for pair in rows.windows(2) {
                    assert_eq!(pair[0].left(), pair[1].left());
                    assert_eq!(pair[1].top() - pair[0].top(), 44.0);
                }
                let labels: Vec<_> = nodes
                    .iter()
                    .filter_map(|(_, node)| node.label().or(node.value()))
                    .collect();
                assert!(!labels.iter().any(|label| label.contains("Snapshots")));
                for expected in [
                    "Start",
                    "Refresh drives",
                    "Explore synthetic sample",
                    "Path",
                ] {
                    assert!(
                        labels.iter().any(|label| label.contains(expected)),
                        "{expected}: {labels:?}"
                    );
                }
                for value in ["0 B", "2.0 KiB", "8.0 KiB", "unknown", "17179869184.0 GiB"] {
                    let painted: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::Shape::Text(text) if text.galley.job.text == value => {
                                Some((shape.clip_rect, text))
                            }
                            _ => None,
                        })
                        .collect();
                    assert!(!painted.is_empty(), "painted capacity {value}");
                    for (clip, text) in painted {
                        let bounds = egui::Rect::from_min_size(text.pos, text.galley.size());
                        assert!(
                            clip.contains_rect(bounds),
                            "clipped {value}: {bounds:?} vs {clip:?}"
                        );
                        assert!(rows.iter().any(|row| row.contains_rect(bounds)));
                    }
                }
                let point = rows[0].center();
                for pressed in [true, false] {
                    pass(vec![
                        egui::Event::PointerMoved(point),
                        egui::Event::PointerButton {
                            pos: point,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: Modifiers::NONE,
                        },
                    ]);
                }
                let (_, action) = pass(vec![egui::Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }]);
                assert!(matches!(action, Some(Action::Start(root)) if root == Path::new(r"X:\")));
                println!(
                    "unit67_chooser width={} height={} rows=4 row_height=40 spacing=4 capacities_unclipped=1 keyboard=1",
                    size.x, size.y
                );
            }
        }
    }

    #[test]
    fn enter_submits_the_path() {
        {
            let context = egui::Context::default();
            let mut dialog = SpaceDialog {
                path: r"X:\synthetic-fixture".into(),
                ..Default::default()
            };
            let _ = context.run_ui(egui::RawInput::default(), |_| {
                dialog.show(&context, false);
            });
            let mut action = None;
            let input = egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            };
            let _ = context.run_ui(input, |_| {
                action = dialog.show(&context, false);
            });
            assert!(
                matches!(action, Some(Action::Start(root)) if root.as_path() == std::path::Path::new(r"X:\synthetic-fixture"))
            );
        }
    }

    #[test]
    fn escape_only_dismisses_the_chooser_with_a_scan_or_pending_preparation() {
        for (has_scan, preparing) in [(false, false), (true, false), (false, true)] {
            let context = egui::Context::default();
            let mut dialog = SpaceDialog {
                preparing,
                ..Default::default()
            };
            let _ = context.run_ui(egui::RawInput::default(), |_| {
                dialog.show(&context, has_scan);
            });
            let action = escape(&context, &mut dialog, has_scan);
            assert_eq!(matches!(action, Some(Action::Close)), has_scan || preparing);
        }
    }
}
