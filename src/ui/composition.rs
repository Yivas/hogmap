//! Non-file volume cells. They never enter the index, hit testing or file navigation.
use super::{paint, text, theme::Palette};
use eframe::egui::{self, Stroke, StrokeKind};
use hogmap::composition::VolumeComponents;

pub use hogmap::composition::VolumePart;

#[derive(Debug)]
pub struct VolumeCell {
    pub part: VolumePart,
    pub bytes: u64,
    pub rect: egui::Rect,
}

/// Normalize only the displayed bytes. D has neither a cell nor an implicit blank area.
pub fn arrange(
    area: egui::Rect,
    apparent: u64,
    components: VolumeComponents,
) -> (egui::Rect, Vec<VolumeCell>) {
    let free = components.free_bytes.unwrap_or(0);
    let reserve = components.reserve_bytes.unwrap_or(0);
    let total = u128::from(apparent) + u128::from(free) + u128::from(reserve);
    if total == 0 || (free == 0 && reserve == 0) {
        return (area, Vec::new());
    }
    let boundary =
        |bytes: u128| area.left() + (f64::from(area.width()) * bytes as f64 / total as f64) as f32;
    let tree_end = boundary(u128::from(apparent));
    let free_end = boundary(u128::from(apparent) + u128::from(free));
    let tree = egui::Rect::from_min_max(area.min, egui::pos2(tree_end, area.bottom()));
    let mut cells = Vec::new();
    if free > 0 {
        cells.push(VolumeCell {
            part: VolumePart::Free,
            bytes: free,
            rect: egui::Rect::from_min_max(
                egui::pos2(tree_end, area.top()),
                egui::pos2(free_end, area.bottom()),
            ),
        });
    }
    if reserve > 0 {
        cells.push(VolumeCell {
            part: VolumePart::Reserve,
            bytes: reserve,
            rect: egui::Rect::from_min_max(egui::pos2(free_end, area.top()), area.max),
        });
    }
    (tree, cells)
}

pub fn draw(ui: &egui::Ui, palette: &Palette, cells: &[VolumeCell], detail: &str) {
    for cell in cells {
        let (fill, color) = match cell.part {
            VolumePart::Free => (palette.dimmed.fill, palette.dimmed.text),
            VolumePart::Reserve => (palette.chrome, palette.secondary_text),
        };
        let painter = ui
            .painter()
            .with_clip_rect(cell.rect.intersect(ui.clip_rect()));
        painter.rect_filled(cell.rect, 0, fill);
        painter.rect_stroke(
            cell.rect,
            0,
            Stroke::new(1.0, palette.separator),
            StrokeKind::Inside,
        );
        let label = text::volume_cell(cell.part, cell.bytes);
        if let Some((galley, position)) = paint::fit(ui, &label, cell.rect.shrink(3.0), true) {
            painter.galley(position, galley, color);
        }
        let tooltip = text::volume_cell_detail(cell.part, cell.bytes, detail);
        let response = ui.interact(
            cell.rect,
            ui.id().with(("volume-part", cell.part)),
            egui::Sense::focusable_noninteractive(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &tooltip));
        if response.has_focus() {
            paint::focus_ring(ui.painter(), cell.rect, palette);
        }
        response.on_hover_text(tooltip);
    }
}
