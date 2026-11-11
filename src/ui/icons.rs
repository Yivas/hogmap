use eframe::egui::{self, Color32, Pos2, Response, Sense, Shape, Stroke, StrokeKind};

use super::{paint, text, theme::Theme};

#[derive(Clone, Copy)]
pub enum Icon {
    NewScan,
    Back,
    Forward,
    Up,
    Home,
    Rescan,
    Stop,
    Highlight,
    MoreDetail,
    LessDetail,
    Drive,
}

impl Icon {
    fn name(self) -> &'static str {
        match self {
            Self::NewScan => text::NEW_SCAN,
            Self::Back => text::BACK,
            Self::Forward => text::FORWARD,
            Self::Up => text::UP,
            Self::Home => text::HOME,
            Self::Rescan => text::RESCAN,
            Self::Stop => text::STOP,
            Self::Highlight => text::FILTER,
            Self::MoreDetail => text::MORE_DETAIL,
            Self::LessDetail => text::LESS_DETAIL,
            Self::Drive => text::DRIVE,
        }
    }
}

pub fn button(
    ui: &mut egui::Ui,
    icon: Icon,
    enabled: bool,
    selected: bool,
    tooltip: &str,
) -> Response {
    let response = ui
        .add_enabled_ui(enabled, |ui| {
            let palette = Theme::current(ui.ctx()).palette();
            let named = matches!(icon, Icon::NewScan | Icon::Rescan | Icon::Stop);
            let caption = named.then(|| {
                ui.painter().layout_no_wrap(
                    icon.name().into(),
                    egui::FontId::proportional(13.0),
                    if enabled {
                        palette.icon
                    } else {
                        palette.disabled_icon
                    },
                )
            });
            let width = caption.as_ref().map_or(32.0, |text| 40.0 + text.size().x);
            let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 32.0), Sense::click());
            let hover =
                ui.ctx()
                    .animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.1);
            let selection =
                ui.ctx()
                    .animate_bool_with_time(response.id.with("selected"), selected, 0.1);
            let primary = matches!(icon, Icon::NewScan);
            let mut fill = if primary {
                palette.tool_selected
            } else {
                palette.chrome
            };
            fill = fill.lerp_to_gamma(palette.tool_hover, hover);
            fill = fill.lerp_to_gamma(palette.tool_selected, selection);
            if response.is_pointer_button_down_on() {
                fill = palette.tool_pressed;
            }
            ui.painter().rect_filled(rect, 3, fill);
            if selected || primary {
                ui.painter().rect_stroke(
                    rect.shrink(0.5),
                    3,
                    Stroke::new(1.0, palette.accent),
                    StrokeKind::Inside,
                );
            }
            draw(
                ui.painter(),
                icon,
                egui::pos2(rect.left() + 16.0, rect.center().y),
                22.0,
                if enabled {
                    palette.icon
                } else {
                    palette.disabled_icon
                },
            );
            if let Some(caption) = caption {
                ui.painter().galley(
                    egui::pos2(rect.left() + 32.0, rect.center().y - caption.size().y / 2.0),
                    caption,
                    palette.icon,
                );
            }
            if response.has_focus() {
                paint::focus_ring(ui.painter(), rect.shrink(1.0), palette);
            }
            response.widget_info(|| {
                egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, icon.name())
            });
            response
        })
        .inner;
    response
        .on_hover_text(tooltip)
        .on_disabled_hover_text(tooltip)
}

/// A small, original line-icon vocabulary on a shared 22 px grid; no external artwork or font glyphs.
pub fn draw(painter: &egui::Painter, icon: Icon, center: Pos2, size: f32, color: Color32) {
    let p = |x: f32, y: f32| center + egui::vec2(x - 11.0, y - 11.0) * (size / 22.0);
    let stroke = Stroke::new(1.6, color);
    let line = |points: &[(f32, f32)]| {
        painter.add(Shape::line(
            points.iter().map(|&(x, y)| p(x, y)).collect(),
            stroke,
        ));
    };
    let box_at = |left, top, right, bottom| {
        painter.rect_stroke(
            egui::Rect::from_min_max(p(left, top), p(right, bottom)),
            1,
            stroke,
            StrokeKind::Inside,
        );
    };
    match icon {
        Icon::NewScan => {
            box_at(2.0, 4.0, 16.0, 18.0);
            line(&[(2.0, 8.0), (16.0, 8.0)]);
            line(&[(17.0, 11.0), (17.0, 21.0)]);
            line(&[(12.0, 16.0), (22.0, 16.0)]);
        }
        Icon::Back | Icon::Forward | Icon::Up => {
            let transform = |x: f32, y: f32| match icon {
                Icon::Forward => (22.0 - x, y),
                Icon::Up => (y, x),
                _ => (x, y),
            };
            for points in [
                &[(10.0, 4.0), (3.0, 11.0), (10.0, 18.0)][..],
                &[(3.0, 11.0), (20.0, 11.0)][..],
            ] {
                line(
                    &points
                        .iter()
                        .map(|&(x, y)| transform(x, y))
                        .collect::<Vec<_>>(),
                );
            }
        }
        Icon::Home => {
            line(&[(1.0, 10.0), (11.0, 2.0), (21.0, 10.0)]);
            line(&[
                (4.0, 9.0),
                (4.0, 20.0),
                (9.0, 20.0),
                (9.0, 13.0),
                (13.0, 13.0),
                (13.0, 20.0),
                (18.0, 20.0),
                (18.0, 9.0),
            ]);
        }
        Icon::Rescan => {
            line(&[
                (19.0, 9.0),
                (17.0, 5.0),
                (12.0, 3.0),
                (7.0, 4.0),
                (3.0, 8.0),
                (3.0, 14.0),
                (7.0, 18.0),
                (13.0, 19.0),
                (18.0, 16.0),
            ]);
            line(&[(19.0, 2.0), (19.0, 9.0), (12.0, 9.0)]);
        }
        Icon::Stop => {
            painter.rect_filled(
                egui::Rect::from_min_max(p(4.0, 4.0), p(18.0, 18.0)),
                1,
                color,
            );
        }
        Icon::Highlight => {
            let points = (0..10)
                .map(|i| {
                    let angle =
                        std::f32::consts::TAU * i as f32 / 10.0 - std::f32::consts::FRAC_PI_2;
                    let radius = if i % 2 == 0 { 10.0 } else { 4.5 };
                    center + egui::vec2(angle.cos(), angle.sin()) * radius * size / 22.0
                })
                .collect();
            painter.add(Shape::closed_line(points, stroke));
        }
        Icon::MoreDetail | Icon::LessDetail => {
            box_at(2.0, 2.0, 20.0, 20.0);
            line(&[(6.0, 11.0), (16.0, 11.0)]);
            if matches!(icon, Icon::MoreDetail) {
                line(&[(11.0, 6.0), (11.0, 16.0)]);
            }
        }
        Icon::Drive => {
            box_at(2.0, 4.0, 20.0, 18.0);
            line(&[(2.0, 13.0), (20.0, 13.0)]);
            painter.circle_filled(p(16.0, 15.5), 1.0, color);
        }
    }
}
