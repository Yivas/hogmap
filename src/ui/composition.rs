//! Non-file volume cells. They never enter the index, hit testing or file navigation.
use super::{dialog::DriveChoice, paint, text, theme::Palette};
use eframe::egui::{self, Stroke, StrokeKind};
use hogmap::composition::{CompositionView, VolumeComponents, VolumeEvidence};
use hogmap::volume::{ObservedContinuity, ObservedFilesystem, RootScope};
use std::path::{Component, Path, Prefix};
use std::sync::Arc;

pub use hogmap::composition::VolumePart;

pub struct VisibleEvaluation {
    pub components: VolumeComponents,
    pub pending_bytes: Option<u64>,
    pub detail: String,
}

fn disk_root(path: &Path) -> Option<u8> {
    let mut parts = path.components().filter(|part| *part != Component::CurDir);
    let disk = match parts.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => disk.to_ascii_uppercase(),
            _ => return None,
        },
        _ => return None,
    };
    (parts.next() == Some(Component::RootDir) && parts.next().is_none()).then_some(disk)
}

/// Visible Free is independent of D, allocated accounting and reserve availability. A provisional
/// lexical drive-root match is never promoted into identity evidence; final eligibility replaces it.
pub fn visible(
    view: CompositionView,
    running: bool,
    root: Option<&Path>,
    drive: Option<&DriveChoice>,
    evidence: Option<&VolumeEvidence>,
    legacy: VolumeComponents,
    scanned: Option<u64>,
) -> VisibleEvaluation {
    let mut result = VisibleEvaluation {
        components: VolumeComponents {
            free_bytes: None,
            reserve_bytes: legacy.reserve_bytes,
        },
        pending_bytes: None,
        detail: String::new(),
    };
    let unavailable = |mut result: VisibleEvaluation, reason: &str| {
        result.detail = format!(
            "Free space unavailable: {reason}. Reserved by Windows retains its allocated/Finished eligibility."
        );
        result
    };
    if !view.unzoomed {
        return unavailable(result, "this is a zoomed view");
    }
    let (total, free, reserve) = if running {
        let Some(drive) = drive.filter(|drive| {
            disk_root(&drive.root).is_some() && root.and_then(disk_root) == disk_root(&drive.root)
        }) else {
            return unavailable(result, "the scan root is not a whole-volume drive root");
        };
        if drive
            .capacity
            .as_ref()
            .and_then(|sample| sample.issue)
            .is_some()
        {
            return unavailable(result, "the provisional capacity query failed");
        }
        result.detail.push_str("Capacity read when the scan started; confirmed at the end. Provisional whole-volume scope, not identity evidence.\n");
        result.detail.push_str(
            "Any known storage reserve comes from the same capacity reading at scan start.\n",
        );
        (
            drive.total_bytes,
            drive.free_bytes,
            drive.storage_reserve_bytes,
        )
    } else {
        if !view.finished {
            return unavailable(result, "the scan did not finish");
        }
        let Some(evidence) = evidence else {
            return unavailable(result, "final origin evidence has not arrived");
        };
        if evidence.scope != Ok(RootScope::WholeVolume) {
            return unavailable(result, "the final origin is not a proven whole volume");
        }
        if evidence.filesystem != Ok(ObservedFilesystem::Ntfs) {
            return unavailable(result, "the final filesystem is not proven NTFS");
        }
        if !evidence.validation.is_ok_and(|validation| {
            validation.directory_identity == Ok(ObservedContinuity::UnchangedAtEndpoints)
                && validation.normalized_path == Ok(ObservedContinuity::UnchangedAtEndpoints)
        }) {
            return unavailable(
                result,
                "root identity or normalized path continuity is unproven",
            );
        }
        let Ok(sample) = evidence.after else {
            return unavailable(result, "the final capacity observation failed");
        };
        if sample.metadata.issue.is_some() {
            return unavailable(result, "the final capacity observation is invalid");
        }
        result
            .detail
            .push_str("Final origin-bound capacity observation.\n");
        (
            sample.total_bytes,
            sample.free_bytes,
            sample
                .storage_reserve_bytes
                .filter(|_| sample.reserve_issue.is_none()),
        )
    };
    if total.zip(free).is_some_and(|(total, free)| free > total) {
        return unavailable(result, "free capacity exceeds total capacity");
    }
    result.components.free_bytes = free;
    result.detail.push_str(match view.metric {
        hogmap::index::SizeMetric::Logical => "The tree uses logical apparent bytes per name; Free space uses volume-capacity bytes. Comparable only in order of magnitude, not physical attribution.\n",
        hogmap::index::SizeMetric::Allocated => "The tree uses apparent allocated bytes per name, not deduplicated storage. Volume cells use independent capacity bytes.\n",
    });
    if let Some(free) = free {
        result
            .detail
            .push_str(&format!("Free space: {free} B known.\n"));
    } else {
        result.detail.push_str("Free space is unknown, not zero.\n");
    }
    if running {
        if let Some(((total, free), scanned)) = total.zip(free).zip(scanned) {
            let residual = i128::from(total)
                - i128::from(free)
                - i128::from(reserve.unwrap_or(0))
                - i128::from(scanned);
            result.pending_bytes = u64::try_from(residual).ok().filter(|bytes| *bytes > 0);
            if residual <= 0 {
                result.detail.push_str("No positive drawable estimate at this sample: the residual is non-positive, not measured zero or measured inaccessible space.\n");
            }
        }
        result.detail.push_str("Not yet scanned · estimate = total - free - known reserve (if available) - displayed scanned apparent bytes. Not measured inaccessible space, a partition, or an error bound. It may shrink or disappear.\n");
        if reserve.is_none() {
            result.detail.push_str(
                "Storage reserve is unknown and omitted from the estimate, not assumed zero.\n",
            );
        }
        if total.is_none() {
            result
                .detail
                .push_str("Estimate unavailable: total capacity unknown.\n");
        }
        if free.is_none() {
            result
                .detail
                .push_str("Estimate unavailable: free capacity unknown.\n");
        }
        if scanned.is_none() {
            result
                .detail
                .push_str("Estimate unavailable: displayed tree bytes overflowed.\n");
        }
    }
    if scanned.is_none() {
        result.components = VolumeComponents::default();
        result.detail.push_str("Volume cells hidden: the displayed tree total cannot be represented; unknown is not zero.");
    }
    result
}

pub struct VolumeCell {
    /// None is the running estimate, not a capacity observation.
    pub part: Option<VolumePart>,
    pub rect: egui::Rect,
    label: String,
    tooltip: String,
    galley: Option<(Arc<egui::Galley>, egui::Pos2)>,
    probe: Option<Arc<egui::Galley>>,
    pixels_per_point: f32,
}
impl VolumeCell {
    fn new(part: Option<VolumePart>, bytes: u64, rect: egui::Rect, detail: &str) -> Self {
        let (label, tooltip) = if let Some(part) = part {
            (
                text::volume_cell(part, bytes),
                text::volume_cell_detail(part, bytes, detail),
            )
        } else {
            let label = format!("Not yet scanned · estimate\n{}", text::size(bytes, false));
            let tooltip = format!(
                "{label} ({bytes} B)\nProvisional residual, not measured inaccessible space.\nNot a file; no file actions.\n{detail}"
            );
            (label, tooltip)
        };
        Self {
            part,
            rect,
            label,
            tooltip,
            galley: None,
            probe: None,
            pixels_per_point: 0.0,
        }
    }
}

/// Normalize only the displayed byte bases. D never has a cell; the running estimate is distinct.
pub fn arrange(
    area: egui::Rect,
    apparent: u64,
    components: VolumeComponents,
    pending: Option<u64>,
    detail: &str,
) -> (egui::Rect, Vec<VolumeCell>) {
    let parts = [
        (Some(VolumePart::Free), components.free_bytes),
        (Some(VolumePart::Reserve), components.reserve_bytes),
        (None, pending),
    ];
    let total = u128::from(apparent)
        + parts
            .iter()
            .filter_map(|(_, bytes)| *bytes)
            .map(u128::from)
            .sum::<u128>();
    if total == 0 {
        return (area, Vec::new());
    }
    let boundary =
        |bytes: u128| area.left() + (f64::from(area.width()) * bytes as f64 / total as f64) as f32;
    let mut consumed = u128::from(apparent);
    let tree = egui::Rect::from_min_max(area.min, egui::pos2(boundary(consumed), area.bottom()));
    let mut cells = Vec::new();
    for (part, bytes) in parts {
        if let Some(bytes) = bytes.filter(|bytes| *bytes > 0) {
            let left = boundary(consumed);
            consumed += u128::from(bytes);
            let rect = egui::Rect::from_min_max(
                egui::pos2(left, area.top()),
                egui::pos2(boundary(consumed), area.bottom()),
            );
            cells.push(VolumeCell::new(part, bytes, rect, detail));
        }
    }
    (tree, cells)
}

pub fn draw(ui: &egui::Ui, palette: &Palette, cells: &mut [VolumeCell]) {
    let probe = paint::font_probe(ui);
    for cell in cells {
        let tone = match cell.part {
            Some(VolumePart::Free) => palette.free,
            Some(VolumePart::Reserve) => super::theme::Tone {
                fill: palette.chrome,
                text: palette.secondary_text,
                seam: None,
                mark: None,
            },
            None => palette.not_yet_scanned,
        };
        let painter = ui
            .painter()
            .with_clip_rect(cell.rect.intersect(ui.clip_rect()));
        painter.rect_filled(cell.rect, 0, tone.fill);
        painter.rect_stroke(
            cell.rect,
            0,
            Stroke::new(1.0, palette.separator),
            StrokeKind::Inside,
        );
        if cell.pixels_per_point != ui.ctx().pixels_per_point()
            || cell
                .probe
                .as_ref()
                .is_none_or(|previous| !Arc::ptr_eq(previous, &probe))
        {
            cell.galley = paint::fit(ui, &cell.label, cell.rect.shrink(3.0), true);
            cell.probe = Some(probe.clone());
            cell.pixels_per_point = ui.ctx().pixels_per_point();
        }
        if let Some((galley, position)) = &cell.galley {
            painter.galley(*position, galley.clone(), tone.text);
        }
        let id = match cell.part {
            Some(part) => ui.id().with(("volume-part", part)),
            None => ui.id().with("not-yet-scanned"),
        };
        let response = ui.interact(cell.rect, id, egui::Sense::focusable_noninteractive());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &cell.tooltip)
        });
        if response.has_focus() {
            paint::focus_ring(ui.painter(), cell.rect, palette);
        }
        if response.hovered() {
            response.on_hover_text(&cell.tooltip);
        }
    }
}
