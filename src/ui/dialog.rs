use std::path::{Component, Path, PathBuf};
use std::thread::{self, JoinHandle};

use hogmap::scan_win::ScanError;
use hogmap::volume::{self, VolumeInfo};

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
}

impl From<VolumeInfo> for DriveChoice {
    fn from(volume: VolumeInfo) -> Self {
        Self {
            root: PathBuf::from(volume.root),
            label: volume.label.unwrap_or_default(),
            free_bytes: volume.free_bytes,
            total_bytes: volume.total_bytes,
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

#[derive(Default, PartialEq)]
enum Tab {
    #[default]
    Drives,
    Snapshots,
}

pub enum Action {
    Start(PathBuf),
    Sample,
    Close,
}

pub struct SpaceDialog {
    pub open: bool,
    tab: Tab,
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
            tab: Tab::Drives,
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
        let width = (context.content_rect().width() - 48.0).clamp(280.0, 488.0);
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
                    ui.selectable_value(&mut self.tab, Tab::Drives, text::DRIVES_TAB);
                    ui.selectable_value(&mut self.tab, Tab::Snapshots, text::SNAPSHOTS_TAB);
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
                match self.tab {
                    Tab::Drives => {
                        egui::ScrollArea::vertical()
                            .id_salt("drive-choices")
                            .max_height(160.0)
                            .show(ui, |ui| {
                                ui.set_min_height(120.0);
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
                                    ui.horizontal_wrapped(|ui| {
                                        for (position, drive) in self.drives.iter().enumerate() {
                                            let selected = self.selected == Some(position);
                                            let (rect, response) = ui.allocate_exact_size(
                                                egui::vec2(148.0, 100.0),
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
                                            ui.painter().rect_filled(rect, 3, fill);
                                            ui.painter().rect_stroke(
                                                rect,
                                                3,
                                                Stroke::new(
                                                    1.0,
                                                    if selected {
                                                        palette.accent
                                                    } else {
                                                        palette.separator
                                                    },
                                                ),
                                                StrokeKind::Inside,
                                            );
                                            icons::draw(
                                                ui.painter(),
                                                Icon::Drive,
                                                rect.left_top() + egui::vec2(20.0, 24.0),
                                                24.0,
                                                palette.icon,
                                            );
                                            let root = drive.root.to_string_lossy();
                                            let caption = if drive.label.is_empty() {
                                                root.to_string()
                                            } else {
                                                format!("{root}  {}", drive.label)
                                            };
                                            let capacity = text::drive_capacity(
                                                drive.free_bytes,
                                                drive.total_bytes,
                                            );
                                            let painter =
                                                ui.painter().with_clip_rect(rect.shrink(8.0));
                                            painter.text(
                                                rect.left_top() + egui::vec2(8.0, 48.0),
                                                egui::Align2::LEFT_TOP,
                                                &caption,
                                                egui::FontId::proportional(13.0),
                                                palette.icon,
                                            );
                                            painter.text(
                                                rect.left_top() + egui::vec2(8.0, 72.0),
                                                egui::Align2::LEFT_TOP,
                                                &capacity,
                                                egui::FontId::proportional(11.0),
                                                palette.secondary_text,
                                            );
                                            response.widget_info(|| {
                                                egui::WidgetInfo::selected(
                                                    egui::WidgetType::Button,
                                                    true,
                                                    selected,
                                                    format!("{caption}, {capacity}"),
                                                )
                                            });
                                            if response.has_focus() {
                                                paint::focus_ring(
                                                    ui.painter(),
                                                    rect.shrink(2.0),
                                                    palette,
                                                );
                                            }
                                            if response.clicked() {
                                                self.selected = Some(position);
                                                self.path = root.to_string();
                                            }
                                            if !self.preparing
                                                && (response.double_clicked()
                                                    || (response.has_focus()
                                                        && context.input_mut(|input| {
                                                            input.consume_key(
                                                                Modifiers::NONE,
                                                                Key::Enter,
                                                            )
                                                        })))
                                            {
                                                action = Some(Action::Start(drive.root.clone()));
                                            }
                                            response
                                                .on_hover_text(format!("{caption}\n{capacity}"));
                                        }
                                    });
                                }
                            });
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
                                    egui::Button::new(text::START),
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
                    Tab::Snapshots => {
                        ui.allocate_ui(egui::vec2(width, 160.0), |ui| {
                            ui.add_space(40.0);
                            ui.vertical_centered(|ui| {
                                ui.strong(text::NO_SNAPSHOTS);
                                ui.label(text::SNAPSHOTS_UNAVAILABLE);
                            });
                        });
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
        };
        let choice = DriveChoice::from(volume.clone());
        assert_eq!(choice.root, PathBuf::from(r"X:\"));
        assert_eq!(choice.label, "Fixture");
        assert_eq!(
            (choice.free_bytes, choice.total_bytes),
            (Some(2048), Some(8192))
        );
        let choice = DriveChoice::from(VolumeInfo {
            free_bytes: None,
            total_bytes: None,
            ..volume.clone()
        });
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
            },
            VolumeInfo {
                root: r"Y:\".into(),
                label: Some("Other".into()),
                filesystem: Some("NTFS".into()),
                total_bytes: Some(1024),
                free_bytes: Some(0),
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
        // An empty listing (first Start still loading) or an unknown volume keeps unknown numbers.
        let choice = volume_choice(
            Path::new(r"\\server\share\folder"),
            Some(r"\\server\share\"),
            Vec::new(),
        );
        assert_eq!(choice.root, PathBuf::from(r"\\server\share\folder"));
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
    fn enter_submits_the_path_only_from_the_drives_tab() {
        for snapshots in [false, true] {
            let context = egui::Context::default();
            let mut dialog = SpaceDialog {
                path: r"X:\synthetic-fixture".into(),
                ..Default::default()
            };
            let _ = context.run_ui(egui::RawInput::default(), |_| {
                dialog.show(&context, false);
            });
            if snapshots {
                dialog.tab = Tab::Snapshots;
            }
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
            if snapshots {
                assert!(action.is_none());
            } else {
                assert!(
                    matches!(action, Some(Action::Start(root)) if root.as_path() == std::path::Path::new(r"X:\synthetic-fixture"))
                );
            }
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
