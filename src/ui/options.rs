use super::{HogMap, SAFETY_DEPTH, text};
use eframe::egui;
use hogmap::treemap::LayoutParams;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Options {
    pub minimum_area: f32,
    pub header_height: f32,
    pub group_small: bool,
    /// Levels drawn below the current root. Depth is relative to the zoom, so opening a folder
    /// shows that folder's own levels instead of the remaining ones of the whole tree.
    pub depth_levels: usize,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            // Grouping cannot be removed: one million separate cells measured about a second per
            // frame. The threshold is adjustable in Options for anyone who wants more detail.
            minimum_area: 400.0,
            // The band is what keeps a folder clickable: without it its children cover the
            // whole cell. The reference also names folders along the top of their block.
            header_height: 20.0,
            group_small: true,
            // Three levels keep a folder readable at a glance; deeper content stays reachable by
            // opening the folder instead of tiling every descendant into unreadable slivers.
            depth_levels: 3,
        }
    }
}
impl Options {
    pub fn valid(self) -> bool {
        self.minimum_area.is_finite()
            && (1.0..=1_000_000.0).contains(&self.minimum_area)
            && self.header_height.is_finite()
            && (0.0..=64.0).contains(&self.header_height)
            && (1..=SAFETY_DEPTH).contains(&self.depth_levels)
    }
    pub fn params(self, flat: bool) -> LayoutParams {
        LayoutParams {
            min_pixel_area: if self.group_small {
                self.minimum_area
            } else {
                // Keep the positive-area contract; this threshold groups no observable cell.
                f32::MIN_POSITIVE
            },
            header_height: self.header_height,
            max_depth: if flat { 1 } else { self.depth_levels },
        }
    }
}
impl HogMap {
    pub(super) fn set_options(&mut self, options: Options) -> bool {
        if !options.valid() {
            return false;
        }
        if options != self.options {
            self.options = options;
            self.last_layout = None;
            self.context.request_repaint();
        }
        true
    }
    pub(super) fn show_options(&mut self, context: &egui::Context) {
        if !self.options_open {
            return;
        }
        let mut options = self.options;
        let response = egui::Modal::new(egui::Id::new("view-options")).show(context, |ui| {
            ui.heading(text::OPTIONS);
            ui.label(text::OPTIONS_SESSION);
            ui.horizontal(|ui| {
                let label = ui.label(text::MINIMUM_AREA);
                ui.add(
                    egui::DragValue::new(&mut options.minimum_area)
                        .range(1.0..=1_000_000.0)
                        .suffix(" px²"),
                )
                .labelled_by(label.id);
            });
            ui.horizontal(|ui| {
                let label = ui.label(text::HEADER_HEIGHT);
                ui.add(
                    egui::DragValue::new(&mut options.header_height)
                        .range(0.0..=64.0)
                        .suffix(" px"),
                )
                .labelled_by(label.id);
            });
            ui.horizontal(|ui| {
                let label = ui.label(text::DEPTH_LEVELS);
                ui.add(
                    egui::DragValue::new(&mut options.depth_levels)
                        .range(1..=SAFETY_DEPTH)
                        .suffix(" levels"),
                )
                .labelled_by(label.id);
            });
            ui.checkbox(&mut options.group_small, text::GROUP_SMALL);
            if ui.button(text::CLOSE_OPTIONS).clicked() {
                self.options_open = false;
            }
        });
        self.set_options(options);
        if response.should_close() {
            self.options_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hogmap::index::{Index, Kind, Sizes, State};
    #[test]
    fn validated_session_options_rebuild_the_map_immediately() {
        let context = egui::Context::default();
        let mut index = Index::new("Fixture".into()).unwrap();
        index.set_state(index.root(), State::Complete).unwrap();
        for (name, bytes) in [("large.bin", 90), ("small.bin", 10)] {
            index
                .insert(
                    index.root(),
                    name.into(),
                    Kind::File,
                    Sizes {
                        logical: Some(bytes),
                        allocated: Some(bytes),
                    },
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
        let mut app = HogMap::with_reducer(context.clone(), reducer);
        let draw = |app: &mut HogMap| {
            let _ = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ui| app.map(ui),
            );
        };
        draw(&mut app);
        assert_eq!(app.cache.as_ref().unwrap().layout.placed.len(), 3);
        let original = app.options;
        for invalid in [
            Options {
                minimum_area: 0.0,
                ..original
            },
            Options {
                minimum_area: f32::NAN,
                ..original
            },
            Options {
                minimum_area: 1_000_001.0,
                ..original
            },
            Options {
                header_height: -1.0,
                ..original
            },
            Options {
                header_height: 65.0,
                ..original
            },
        ] {
            assert!(!app.set_options(invalid));
            assert_eq!(app.options, original);
        }
        assert!(app.set_options(Options {
            minimum_area: 1_000_000.0,
            header_height: 0.0,
            group_small: true,
            depth_levels: 3,
        }));
        draw(&mut app);
        assert_eq!(app.cache.as_ref().unwrap().layout.placed.len(), 2);
        assert!(
            app.cache.as_ref().unwrap().layout.placed[0]
                .header
                .is_none()
        );
        assert!(app.set_options(Options {
            group_small: false,
            ..app.options
        }));
        draw(&mut app);
        assert_eq!(app.cache.as_ref().unwrap().layout.placed.len(), 3);
    }
}
