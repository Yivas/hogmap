use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui::{self, Color32, FontId, Pos2, Stroke, StrokeKind};
use hogmap::filter::Match;
use hogmap::index::{EntryId, Index, Kind, State};
use hogmap::treemap::{Layout, NodeRef, Rect};

use super::{logic, text};

pub struct Cell {
    pub tooltip: String,
    pub label: Option<(Arc<egui::Galley>, Pos2)>,
    pub color: Color32,
}

pub fn rect(rect: Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        Pos2::new(rect.x, rect.y),
        egui::vec2(rect.width, rect.height),
    )
}

/// `subset` is the member count being drawn when the map shows an aggregate's members only.
pub fn prepare(ui: &egui::Ui, index: &Index, layout: &Layout, subset: Option<usize>) -> Vec<Cell> {
    layout
        .placed
        .iter()
        .enumerate()
        .map(|(position, cell)| {
            let (name, state, kind) = match cell.node {
                NodeRef::Entry(id) => {
                    let entry = index
                        .get(id)
                        .expect("layout IDs belong to the current snapshot");
                    let name = match subset {
                        // The root cell stays the parent directory; the suffix names the subset.
                        Some(members) if position == 0 => {
                            text::subset_name(&entry.name.to_string_lossy(), members)
                        }
                        _ => entry.name.to_string_lossy().into_owned(),
                    };
                    (name, entry.state, entry.kind)
                }
                NodeRef::Aggregate { .. } => (
                    text::aggregate_name(cell.members.len()),
                    State::Complete,
                    Kind::File,
                ),
            };
            let size = text::size(cell.known_sum, cell.has_unknown);
            let state_label = text::state(state);
            let tooltip = if matches!(cell.node, NodeRef::Aggregate { .. }) {
                text::aggregate_tooltip(cell.members.len(), &size)
            } else {
                text::tooltip(&name, &size, state_label)
            };
            let color = if matches!(state, State::Partial | State::Inaccessible | State::Stale) {
                Color32::from_rgb(166, 166, 166)
            } else if matches!(cell.node, NodeRef::Aggregate { .. }) {
                Color32::from_rgb(190, 166, 218)
            } else if kind == Kind::Directory {
                Color32::from_rgb(202, 166, 124)
            } else {
                Color32::from_rgb(157, 200, 229)
            };
            let label_rect = rect(cell.header.unwrap_or(cell.rect)).shrink(3.0);
            let label = if cell.header.is_some() {
                fit(ui, &text::header(&name, &size), label_rect, false)
            } else {
                fit(ui, &text::label(&name, &size), label_rect, true)
            };
            Cell {
                tooltip,
                label,
                color,
            }
        })
        .collect()
}

fn fit(
    ui: &egui::Ui,
    label: &str,
    rect: egui::Rect,
    centered: bool,
) -> Option<(Arc<egui::Galley>, Pos2)> {
    let font = FontId::proportional(11.0);
    let painter = ui.painter();
    let lines: Vec<_> = label.lines().collect();
    let line_height = painter
        .layout_no_wrap("M".into(), font.clone(), Color32::BLACK)
        .size()
        .y;
    if rect.height() < line_height * lines.len() as f32 {
        return None;
    }
    let mut fitted = Vec::new();
    let ellipsis_width = painter
        .layout_no_wrap(text::ELLIPSIS.into(), font.clone(), Color32::BLACK)
        .size()
        .x;
    for line in lines {
        let chars: Vec<_> = line.chars().collect();
        let widths: Vec<_> = chars
            .iter()
            .map(|c| {
                painter
                    .layout_no_wrap(c.to_string(), font.clone(), Color32::BLACK)
                    .size()
                    .x
            })
            .collect();
        let (count, truncated) = logic::fitting_prefix(&widths, rect.width(), ellipsis_width)?;
        let mut line: String = chars[..count].iter().collect();
        if truncated {
            line.push_str(text::ELLIPSIS);
        }
        fitted.push(line);
    }
    let galley = painter.layout_no_wrap(fitted.join("\n"), font, Color32::BLACK);
    let position = if centered {
        rect.center() - galley.size() / 2.0
    } else {
        rect.left_center() - egui::vec2(0.0, galley.size().y / 2.0)
    };
    Some((galley, position))
}

pub fn draw(
    ui: &egui::Ui,
    layout: &Layout,
    cells: &[Cell],
    matches: &HashMap<EntryId, Match>,
    selected: Option<NodeRef>,
) {
    for (placed, visual) in layout.placed.iter().zip(cells) {
        let area = rect(placed.rect);
        let painter = ui.painter().with_clip_rect(area.intersect(ui.clip_rect()));
        let color = match logic::highlight(placed.node, &placed.members, matches) {
            Match::Yes => visual.color,
            Match::No => Color32::from_rgb(222, 222, 222),
            Match::Unknown => Color32::from_rgb(187, 183, 172),
        };
        painter.rect_filled(area, 0, color);
        painter.rect_stroke(
            area,
            0,
            Stroke::new(1.0, Color32::from_rgb(102, 81, 58)),
            StrokeKind::Inside,
        );
        if let Some(header) = placed.header {
            painter.line_segment(
                [rect(header).left_bottom(), rect(header).right_bottom()],
                Stroke::new(1.0, Color32::from_rgb(125, 95, 62)),
            );
        }
        if let Some((galley, position)) = &visual.label {
            painter.galley(*position, galley.clone(), Color32::BLACK);
        }
    }
    if let Some(selected) = selected
        && let Some(cell) = layout.placed.iter().find(|cell| cell.node == selected)
    {
        ui.painter().rect_stroke(
            rect(cell.rect).shrink(1.0),
            0,
            Stroke::new(2.0, Color32::from_rgb(18, 69, 130)),
            StrokeKind::Inside,
        );
    }
}
