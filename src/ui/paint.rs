use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui::{self, Color32, FontId, Pos2, Shape, Stroke, StrokeKind};
use hogmap::filter::{ClassColor, ClassTable, Match};
use hogmap::index::{EntryId, Index, Kind, SizeMetric, State};
use hogmap::treemap::{Layout, NodeRef, Placed, Rect};

use super::theme::{DEPTH_TONES, MapStyle, Palette, Theme, Tone};
use super::{logic, text};

/// Cells smaller than this on either side keep their fill and tooltip but skip state marks, which
/// would not be legible there and would only add paint cost. Hover and selection outlines move
/// outside such cells instead of covering them.
const MARK_MIN_SIDE: f32 = 8.0;
const HATCH_SPACING: f32 = 6.0;
/// Fewer visible characters than this read as punctuation rather than a name.
const MIN_LABEL_CHARS: usize = 3;

/// What a cell represents, independent of the theme; colours are resolved when drawing, so a theme
/// change never invalidates the cached cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// `depth` counts levels below the drawn root and selects the header tone.
    Directory {
        depth: usize,
    },
    File,
    ClassFile(ClassColor),
    Aggregate,
    /// Partial, inaccessible or stale. Inaccessible entries are hatched; the others get a dashed
    /// inner border.
    Incomplete {
        inaccessible: bool,
    },
}

pub struct Cell {
    pub tooltip: String,
    pub label: Option<(Arc<egui::Galley>, Pos2)>,
    /// Name and size centred in a directory whose children are not drawn, so its interior
    /// carries the same reading as a file instead of staying blank under the header.
    pub center: Option<(Arc<egui::Galley>, Pos2)>,
    pub surface: Surface,
    pub tags: u8,
    /// Directory borders are drawn after every cell, above the children that tile them.
    pub directory: bool,
}

pub fn rect(rect: Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        Pos2::new(rect.x, rect.y),
        egui::vec2(rect.width, rect.height),
    )
}

/// `subset` describes the members being drawn when the map shows an aggregate's members only.
/// `metric` is the one `layout` was weighed by; tooltips and accessible labels name it.
pub fn prepare(
    ui: &egui::Ui,
    index: &Index,
    layout: &Layout,
    subset: Option<text::Subset>,
    metric: SizeMetric,
    classes: &ClassTable,
    style: MapStyle,
) -> Vec<Cell> {
    // The layout places parents before their descendants, so a parent's depth is always known.
    let mut depths: HashMap<EntryId, usize> = HashMap::new();
    // Small cells get a smaller face before they lose their name: the reference fits a name and
    // a size into cells this size, and a bare number says less than a small label does. Large
    // cells get a larger one, so a big block reads from across the screen instead of carrying the
    // same small print as a sliver. Header bands keep the three small faces: they are one line
    // tall, and a larger face would only make the band wider than its siblings.
    let mut all_fitters = [
        LabelFitter::new(ui, 16.0),
        LabelFitter::new(ui, 13.0),
        LabelFitter::new(ui, 11.0),
        LabelFitter::new(ui, 9.0),
        LabelFitter::new(ui, 7.5),
    ];
    layout
        .placed
        .iter()
        .enumerate()
        .map(|(position, cell)| {
            let (name, state, kind, depth) = match cell.node {
                NodeRef::Entry(id) => {
                    let entry = index
                        .get(id)
                        .expect("layout IDs belong to the current snapshot");
                    let depth = entry
                        .parent
                        .and_then(|parent| depths.get(&parent))
                        .map_or(0, |depth| depth + 1);
                    depths.insert(id, depth);
                    let name = match subset {
                        // The root cell stays the parent directory; the suffix names the subset.
                        Some(subset) if position == 0 => {
                            text::subset_name(&entry.name.to_string_lossy(), subset)
                        }
                        _ => entry.name.to_string_lossy().into_owned(),
                    };
                    (name, entry.state, entry.kind, depth)
                }
                NodeRef::Group { .. } => (
                    cell.group_name.clone().expect("group has a name"),
                    State::Complete,
                    Kind::File,
                    0,
                ),
                // The parent's depth, so the aggregate takes its parent's interior fill.
                NodeRef::Aggregate { parent } => (
                    text::aggregate_name(cell.members.len()),
                    State::Complete,
                    Kind::File,
                    depths.get(&parent).copied().unwrap_or(0),
                ),
            };
            let size = text::size(cell.known_sum, cell.has_unknown);
            let state_label = text::state(state);
            let mut tooltip = if matches!(cell.node, NodeRef::Aggregate { .. }) {
                text::aggregate_tooltip(cell.members.len(), &size, metric)
            } else {
                text::tooltip(&name, &size, state_label, metric, kind == Kind::Directory)
            };
            if matches!(cell.node, NodeRef::Group { .. }) {
                tooltip.push_str(&format!(
                    "\n{} members; open to view them.",
                    cell.members.len()
                ));
            }
            let tags = match cell.node {
                NodeRef::Entry(id) => index.tags(id).unwrap_or(0),
                _ => 0,
            };
            if tags != 0 {
                tooltip.push_str(&format!("\n{}", text::tag_names(tags)));
            }
            let class = match cell.node {
                NodeRef::Entry(id) if kind == Kind::File && style == MapStyle::FileClasses => index
                    .get(id)
                    .and_then(|entry| classes.color_for(entry.name)),
                _ => None,
            };
            let surface = if matches!(state, State::Partial | State::Inaccessible | State::Stale) {
                Surface::Incomplete {
                    inaccessible: state == State::Inaccessible,
                }
            } else if matches!(cell.node, NodeRef::Aggregate { .. }) {
                // Items too small to draw read as the folder's own background, as in the
                // reference: no fill of their own and no "small items" label. They stay one
                // cell so the map never has to draw a million slivers, and the tooltip and
                // double-click still reach them.
                Surface::Directory { depth }
            } else if matches!(cell.node, NodeRef::Group { .. }) {
                Surface::Aggregate
            } else if kind == Kind::Directory {
                Surface::Directory { depth }
            } else {
                class.map_or(Surface::File, Surface::ClassFile)
            };
            // Header bands are short by design, so they get almost no vertical inset; a uniform
            // 3 px on every edge would eat the text line of a 13 px band.
            let mut label_rect = match cell.header {
                Some(header) => {
                    let band = rect(header);
                    egui::Rect::from_min_max(
                        band.min + egui::vec2(5.0, 1.0),
                        band.max - egui::vec2(3.0, 1.0),
                    )
                }
                None => rect(cell.rect).shrink(3.0),
            };
            if let Some(badge) = tag_area(cell, tags) {
                label_rect.max.x = label_rect.max.x.min(badge.left() - 2.0);
            }
            // Every whole attempt runs across all font sizes before any truncated one: a name
            // that fits complete at 7.5 px beats the same name cut at 11 px, which is what a
            // single pass per size used to produce. Only when nothing fits whole does the name
            // get shortened. A size is never shortened: where the name and the size compete, the
            // name wins, because the number is also in the tooltip and the status bar.
            let header = text::header(&name, &size);
            let label = if matches!(cell.node, NodeRef::Aggregate { .. }) {
                None
            } else if cell.header.is_some() {
                let fitters = &mut all_fitters[HEADER_FACES];
                // A folder is identified by its name, so the name survives the size: a narrow
                // band shows the name alone, and only a band too narrow for the name falls back
                // to the number. A cut size is never shown.
                fitters
                    .iter_mut()
                    .find_map(|fitter| fitter.fit_whole(ui, &header, label_rect, false))
                    .or_else(|| {
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit_whole(ui, &name, label_rect, false))
                    })
                    .or_else(|| {
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit(ui, &name, label_rect, false))
                    })
                    .or_else(|| {
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit_whole(ui, &size, label_rect, false))
                    })
            } else {
                let fitters = &mut all_fitters;
                let two_lines = text::label(&name, &size);
                fitters
                    .iter_mut()
                    .find_map(|fitter| fitter.fit_whole(ui, &two_lines, label_rect, true))
                    .or_else(|| {
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit_whole(ui, &header, label_rect, true))
                    })
                    .or_else(|| {
                        // A whole name beats a bare number: the size is also in the tooltip and
                        // the status bar, while the name appears nowhere else.
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit_whole(ui, &name, label_rect, true))
                    })
                    .or_else(|| {
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit(ui, &two_lines, label_rect, true))
                    })
                    .or_else(|| {
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit_whole(ui, &size, label_rect, true))
                    })
            };
            // A folder drawn without its children gets its name and size centred in the empty
            // interior, as a file would. Never the drawn root (position 0): its band always names
            // it, a grouped view fills its interior with groups, and an empty map shows its own
            // message there.
            let center = if position != 0 && cell.header.is_some() && !cell.expanded {
                let interior = rect(cell.content).shrink(3.0);
                let two_lines = text::label(&name, &size);
                let fitters = &mut all_fitters;
                fitters
                    .iter_mut()
                    .find_map(|fitter| fitter.fit_whole(ui, &two_lines, interior, true))
                    .or_else(|| {
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit_whole(ui, &header, interior, true))
                    })
                    .or_else(|| {
                        fitters
                            .iter_mut()
                            .find_map(|fitter| fitter.fit(ui, &two_lines, interior, true))
                    })
            } else {
                None
            };
            // The reference names a closed folder once, in its middle; the band repeats it only
            // when the middle is too small for the label.
            let label = if center.is_some() { None } else { label };
            Cell {
                tooltip,
                label,
                center,
                surface,
                tags,
                directory: kind == Kind::Directory,
            }
        })
        .collect()
}

/// The faces a one-line header band may use: the three small ones of [`prepare`]'s list.
const HEADER_FACES: std::ops::RangeFrom<usize> = 2..;

pub(super) fn fit(
    ui: &egui::Ui,
    label: &str,
    rect: egui::Rect,
    centered: bool,
) -> Option<(Arc<egui::Galley>, Pos2)> {
    LabelFitter::new(ui, 11.0).fit(ui, label, rect, centered)
}

struct LabelFitter {
    font: FontId,
    line_height: f32,
    ellipsis: f32,
    glyphs: HashMap<char, f32>,
    chars: Vec<char>,
    widths: Vec<f32>,
}
impl LabelFitter {
    fn new(ui: &egui::Ui, size: f32) -> Self {
        let font = FontId::proportional(size);
        let painter = ui.painter();
        Self {
            line_height: painter
                .layout_no_wrap("M".into(), font.clone(), Color32::BLACK)
                .size()
                .y,
            ellipsis: painter
                .layout_no_wrap(text::ELLIPSIS.into(), font.clone(), Color32::BLACK)
                .size()
                .x,
            font,
            glyphs: HashMap::new(),
            chars: Vec::new(),
            widths: Vec::new(),
        }
    }
    fn fit(
        &mut self,
        ui: &egui::Ui,
        label: &str,
        rect: egui::Rect,
        centered: bool,
    ) -> Option<(Arc<egui::Galley>, Pos2)> {
        self.fit_inner(ui, label, rect, centered, true)
    }

    /// Only returns a label that fits whole, for the shapes where a cut would remove the number.
    fn fit_whole(
        &mut self,
        ui: &egui::Ui,
        label: &str,
        rect: egui::Rect,
        centered: bool,
    ) -> Option<(Arc<egui::Galley>, Pos2)> {
        self.fit_inner(ui, label, rect, centered, false)
    }

    fn fit_inner(
        &mut self,
        ui: &egui::Ui,
        label: &str,
        rect: egui::Rect,
        centered: bool,
        allow_truncation: bool,
    ) -> Option<(Arc<egui::Galley>, Pos2)> {
        if rect.height() < self.line_height * label.lines().count() as f32 {
            return None;
        }
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        for (line_number, line) in label.lines().enumerate() {
            let numeric = line_number > 0;
            // The number shares the name's face: in a second face it read as a separate label.
            let font = self.font.clone();
            let ellipsis = self.ellipsis;
            self.chars.clear();
            self.widths.clear();
            self.chars.extend(line.chars());
            // Widths are scoped to this font/preparation, so font or DPI changes cannot reuse
            // stale measurements. Zero advance can mean a missing glyph or a genuinely zero-width
            // character: ask layout once and cache its actual result, including replacement glyphs.
            ui.fonts_mut(|fonts| {
                for character in &self.chars {
                    let width = *self.glyphs.entry(*character).or_insert_with(|| {
                        let width = fonts.glyph_width(&font, *character);
                        if width == 0.0 {
                            fonts
                                .layout_no_wrap(character.to_string(), font.clone(), Color32::BLACK)
                                .size()
                                .x
                        } else {
                            width
                        }
                    });
                    self.widths.push(width);
                }
            });
            let (count, truncated) = logic::fitting_prefix(&self.widths, rect.width(), ellipsis)?;
            // The name may be cut short; the size may not. A cell that says "Docume…" and the
            // number is useful, one that says "Docume…" alone is not.
            let size_line = numeric;
            if count < MIN_LABEL_CHARS || (truncated && (!allow_truncation || size_line)) {
                return None;
            }
            let mut fitted = String::with_capacity(line.len());
            if line_number > 0 {
                fitted.push('\n');
            }
            fitted.extend(self.chars[..count].iter());
            if truncated {
                fitted.push_str(text::ELLIPSIS);
            }
            job.append(
                &fitted,
                0.0,
                egui::TextFormat {
                    font_id: font,
                    color: Color32::PLACEHOLDER,
                    ..Default::default()
                },
            );
        }
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        // Shaping and rounding can differ from summed advances; the final renderer is authoritative.
        if galley.size().x > rect.width() || galley.size().y > rect.height() {
            return None;
        }
        let position = if centered {
            rect.center() - galley.size() / 2.0
        } else {
            rect.left_center() - egui::vec2(0.0, galley.size().y / 2.0)
        };
        Some((galley, position))
    }
}

fn base_tone(palette: &Palette, surface: Surface) -> Tone {
    match surface {
        Surface::Directory { depth } => palette.directories[depth.min(DEPTH_TONES - 1)],
        Surface::File => palette.file,
        Surface::ClassFile(color) => palette.class(color),
        Surface::Aggregate => palette.aggregate,
        Surface::Incomplete { .. } => palette.incomplete,
    }
}

/// The tone a cell is painted with: its surface, or the dimmed or undecided fill the highlight
/// chooses. Marks are read from this same tone, so a dimmed cell cannot keep a state mark.
fn painted_tone(palette: &Palette, surface: Surface, matches: &[Match], placed: &Placed) -> Tone {
    match logic::highlight(placed.node, &placed.members, matches) {
        Match::Yes => base_tone(palette, surface),
        Match::No => palette.dimmed,
        Match::Unknown => palette.undecided,
    }
}

/// Numbers, not hue, identify effective tags. Tiny cells retain their full accessible tooltip.
fn tag_area(placed: &Placed, tags: u8) -> Option<egui::Rect> {
    let area = rect(placed.header.unwrap_or(placed.rect));
    let width = tags.count_ones() as f32 * 12.0;
    (tags != 0 && area.height() >= 15.0 && area.width() >= width + 6.0).then(|| {
        egui::Rect::from_min_size(
            egui::pos2(area.right() - width - 3.0, area.top() + 1.0),
            egui::vec2(width, 13.0),
        )
    })
}

fn at_least(area: egui::Rect, side: f32) -> bool {
    area.width() >= side && area.height() >= side
}

fn markable(area: egui::Rect) -> bool {
    at_least(area, MARK_MIN_SIDE)
}

/// Draws the cells in layout order, then directory structure above the children, then every
/// dashed state mark in one batch, then hover and selection. Hover and selection are outlines
/// only, so the state fill and label colours underneath stay as measured.
pub fn draw(
    painter: &egui::Painter,
    palette: &Palette,
    layout: &Layout,
    cells: &[Cell],
    matches: &[Match],
    interaction: (Option<NodeRef>, Option<usize>),
    scratch: &mut Vec<Shape>,
) {
    let clip = painter.clip_rect();
    // Retain the frame scratch allocation; egui receives the shapes, not this Vec's capacity.
    let mut dashes = std::mem::take(scratch);
    dashes.clear();
    for (placed, cell) in layout.placed.iter().zip(cells) {
        let area = rect(placed.rect);
        let painter = painter.with_clip_rect(area.intersect(clip));
        let tone = painted_tone(palette, cell.surface, matches, placed);
        let inaccessible = cell.surface == Surface::Incomplete { inaccessible: true };
        painter.rect_filled(area, 0, tone.fill);
        if inaccessible
            && markable(area)
            && let Some(mark) = tone.mark
        {
            hatch(&painter, area, mark);
        }
        if !cell.directory
            && let Some(seam) = tone.seam
        {
            seams(&painter, area, seam);
        }
        if cell.surface
            == (Surface::Incomplete {
                inaccessible: false,
            })
            && markable(area)
            && let Some(mark) = tone.mark
        {
            dashed_outline(
                area.shrink(2.5),
                Stroke::new(1.0, mark),
                4.0,
                3.0,
                &mut dashes,
            );
        }
        if let Some(badge) = tag_area(placed, cell.tags) {
            for (offset, slot) in (0..4)
                .filter(|slot| cell.tags & (1 << slot) != 0)
                .enumerate()
            {
                let mark = egui::Rect::from_min_size(
                    badge.min + egui::vec2(offset as f32 * 12.0, 0.0),
                    egui::vec2(11.0, 13.0),
                );
                painter.rect_filled(mark, 0, super::theme::TAG_COLORS[slot]);
                painter.rect_stroke(
                    mark,
                    0,
                    Stroke::new(1.0, Color32::BLACK),
                    StrokeKind::Inside,
                );
                painter.text(
                    mark.center(),
                    egui::Align2::CENTER_CENTER,
                    (slot + 1).to_string(),
                    FontId::monospace(10.0),
                    Color32::BLACK,
                );
            }
        }
        for (galley, position) in cell.label.iter().chain(cell.center.iter()) {
            if inaccessible {
                // Keep the label on its measured fill instead of on the hatching.
                let plate = egui::Rect::from_min_size(*position, galley.size()).expand(1.0);
                painter.rect_filled(plate, 0, tone.fill);
            }
            painter.galley(*position, galley.clone(), tone.text);
        }
    }
    for (placed, cell) in layout.placed.iter().zip(cells) {
        let area = rect(placed.rect);
        let painter = painter.with_clip_rect(area.intersect(clip));
        if cell.directory {
            painter.rect_stroke(
                area,
                0,
                Stroke::new(1.0, palette.directory_border),
                StrokeKind::Inside,
            );
            if let Some(header) = placed.header {
                header_rule(&painter, rect(header), palette.directory_border);
            }
        }
    }
    painter.extend(dashes.drain(..));
    *scratch = dashes;
    draw_interaction(painter, palette, layout, interaction);
}

pub fn draw_interaction(
    painter: &egui::Painter,
    palette: &Palette,
    layout: &Layout,
    interaction: (Option<NodeRef>, Option<usize>),
) {
    let (selected, hovered) = interaction;
    if let Some(position) = hovered
        && let Some(cell) = layout.placed.get(position)
        && Some(cell.node) != selected
    {
        outline(painter, rect(cell.rect), 1.0, 1.0, palette.hover);
    }
    if let Some(selected) = selected
        && let Some(cell) = layout.placed.iter().find(|cell| cell.node == selected)
    {
        outline(painter, rect(cell.rect), 2.0, 0.0, palette.selection);
    }
}

/// A memoized galley also detects atlas resets: egui clears this cache when it recreates fonts.
pub fn font_probe(ui: &egui::Ui) -> Arc<egui::Galley> {
    ui.painter()
        .layout_no_wrap("M".into(), FontId::proportional(11.0), Color32::BLACK)
}

pub struct CachedMap {
    theme: Theme,
    pixels_per_point: f32,
    font_image_size: [usize; 2],
    probe: Arc<egui::Galley>,
    shapes: Vec<egui::epaint::ClippedShape>,
    dirty: bool,
}
impl CachedMap {
    pub fn font_matches(&self, ui: &egui::Ui, probe: &Arc<egui::Galley>) -> bool {
        self.pixels_per_point == ui.ctx().pixels_per_point() && Arc::ptr_eq(&self.probe, probe)
    }
    pub fn valid(&self, ui: &egui::Ui, probe: &Arc<egui::Galley>) -> bool {
        !self.dirty
            && self.font_matches(ui, probe)
            && self.theme == Theme::current(ui.ctx())
            && self.font_image_size == ui.fonts(|fonts| fonts.font_image_size())
    }
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Record through the real painter and tessellator, then detach the scratch layer. Per-cell
    /// clips, texture coordinates and paint order survive exactly; only overlays remain dynamic.
    /// Build after widget/tooltip text has populated the atlas, never before new glyphs are laid out.
    pub fn build(
        ui: &egui::Ui,
        layout: &Layout,
        cells: &[Cell],
        matches: &[Match],
        scratch: &mut Vec<Shape>,
        probe: Arc<egui::Galley>,
    ) -> Self {
        let theme = Theme::current(ui.ctx());
        let layer = egui::LayerId::new(egui::Order::Background, ui.id().with("map-mesh-scratch"));
        let painter = ui.painter().clone().with_layer_id(layer);
        draw(
            &painter,
            theme.palette(),
            layout,
            cells,
            matches,
            (None, None),
            scratch,
        );
        let recorded = ui
            .ctx()
            .graphics_mut(|layers| std::mem::take(layers.entry(layer)));
        let shapes = recorded.all_entries().cloned().collect();
        let pixels_per_point = ui.ctx().pixels_per_point();
        let shapes = ui
            .ctx()
            .tessellate(shapes, pixels_per_point)
            .into_iter()
            .map(|primitive| {
                let shape = match primitive.primitive {
                    egui::epaint::Primitive::Mesh(mesh) => Shape::Mesh(Arc::new(mesh)),
                    egui::epaint::Primitive::Callback(callback) => Shape::Callback(callback),
                };
                egui::epaint::ClippedShape {
                    clip_rect: primitive.clip_rect,
                    shape,
                }
            })
            .collect();
        Self {
            theme,
            pixels_per_point,
            font_image_size: ui.fonts(|fonts| fonts.font_image_size()),
            probe,
            shapes,
            dirty: false,
        }
    }
    pub fn draw(&self, ui: &egui::Ui) {
        ui.ctx().graphics_mut(|layers| {
            let list = layers.entry(ui.layer_id());
            for shape in &self.shapes {
                list.add(
                    shape.clip_rect.intersect(ui.clip_rect()),
                    shape.shape.clone(),
                );
            }
        });
    }
}

/// Hover sits one pixel in and selection on the edge, so the inset is explicit instead of being
/// guessed from the width. Cells too small for marks get the outline outside, so a sliver keeps
/// its fill visible.
fn outline(painter: &egui::Painter, area: egui::Rect, width: f32, inset: f32, color: Color32) {
    let stroke = Stroke::new(width, color);
    if !markable(area) {
        painter.rect_stroke(area, 0, stroke, StrokeKind::Outside);
    } else {
        painter.rect_stroke(area.shrink(inset), 0, stroke, StrokeKind::Inside);
    }
}

/// Keyboard focus is a two-tone dashed ring on the control's own edge: no state mark or outline
/// uses that pattern, and the backing keeps the dashes visible on any fill.
pub fn focus_ring(painter: &egui::Painter, area: egui::Rect, palette: &Palette) {
    // Drawn inside the cell: a stroke outside it paints pixels the widget's rectangle does not
    // cover, so after a resize the old ones are not marked dirty and fade away on their own.
    // The width shrinks with the control so a one-point cell still holds its ring, and the clip
    // keeps the guarantee even where a centred dash stroke would round past the edge.
    let painter = painter.with_clip_rect(area);
    let mut shapes = Vec::new();
    focus_ring_shapes(area, palette, &mut shapes);
    painter.extend(shapes);
}

/// The ring's geometry, kept apart so a test can measure it without a painter.
fn focus_ring_shapes(area: egui::Rect, palette: &Palette, shapes: &mut Vec<Shape>) {
    let width = 2.0_f32.min(area.width()).min(area.height());
    shapes.push(Shape::rect_stroke(
        area,
        0,
        Stroke::new(width, palette.focus_backing),
        StrokeKind::Inside,
    ));
    dashed_outline(
        area.shrink(width / 2.0),
        Stroke::new(width, palette.focus),
        4.0,
        3.0,
        shapes,
    );
}

/// One-pixel seams on the right and bottom edges only, so siblings share a single seam; the left
/// and top edges belong to the neighbour, the header rule or the directory border.
fn seams(painter: &egui::Painter, area: egui::Rect, color: Color32) {
    let right = egui::Rect::from_min_max(egui::pos2(area.right() - 1.0, area.top()), area.max);
    let bottom = egui::Rect::from_min_max(egui::pos2(area.left(), area.bottom() - 1.0), area.max);
    painter.rect_filled(right, 0, color);
    painter.rect_filled(bottom, 0, color);
}

/// Every header closes with the same solid rule. An incomplete total used to get a light dotted
/// rule instead, and during a scan almost every folder passes through that state, so the dots
/// flickered on and off across the whole map. The header text already says "+ unknown".
fn header_rule(painter: &egui::Painter, header: egui::Rect, directory_border: Color32) {
    let rule =
        egui::Rect::from_min_max(egui::pos2(header.left(), header.bottom() - 1.0), header.max);
    painter.rect_filled(rule, 0, directory_border);
}

fn hatch(painter: &egui::Painter, area: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.0, color);
    let mut offset = -area.height();
    while offset < area.width() {
        painter.line_segment(
            [
                egui::pos2(area.left() + offset, area.bottom()),
                egui::pos2(area.left() + offset + area.height(), area.top()),
            ],
            stroke,
        );
        offset += HATCH_SPACING;
    }
}

fn dashed_outline(area: egui::Rect, stroke: Stroke, dash: f32, gap: f32, shapes: &mut Vec<Shape>) {
    let path = [
        area.left_top(),
        area.right_top(),
        area.right_bottom(),
        area.left_bottom(),
        area.left_top(),
    ];
    Shape::dashed_line_many(&path, stroke, dash, gap, shapes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_focus_ring_stays_inside_the_control_at_every_size() {
        let palette = crate::ui::theme::Theme::Dark.palette();
        for (width, height) in [
            (1.0, 20.0),
            (20.0, 1.0),
            (1.0, 1.0),
            (2.0, 20.0),
            (40.0, 40.0),
        ] {
            let area = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(width, height));
            let mut shapes = Vec::new();
            focus_ring_shapes(area, palette, &mut shapes);
            assert!(!shapes.is_empty(), "{width}x{height}: no ring");
            for shape in &shapes {
                let painted = match shape {
                    Shape::Rect(rect) => rect.rect,
                    Shape::LineSegment { points, stroke } => {
                        egui::Rect::from_two_pos(points[0], points[1]).expand(stroke.width / 2.0)
                    }
                    other => panic!("{width}x{height}: unexpected shape {other:?}"),
                };
                assert!(
                    area.expand(0.01).contains_rect(painted),
                    "{width}x{height}: {painted:?} escapes {area:?}"
                );
            }
        }
    }

    #[test]
    fn folder_headers_keep_the_name_when_the_size_cannot_fit() {
        let context = egui::Context::default();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            let mut index = Index::new("SystemApplicationData".into()).unwrap();
            let root = index.root();
            index.set_state(root, State::Complete).unwrap();
            index
                .insert(
                    root,
                    "file.bin".into(),
                    Kind::File,
                    hogmap::index::Sizes {
                        logical: Some(4096),
                        allocated: Some(4096),
                    },
                    None,
                    State::Complete,
                )
                .unwrap();
            let layout = hogmap::treemap::layout(
                &index,
                root,
                Rect::new(0.0, 0.0, 65.0, 100.0),
                hogmap::treemap::LayoutParams {
                    min_pixel_area: 1.0,
                    header_height: 20.0,
                    max_depth: 1,
                },
            )
            .unwrap();
            let cells = prepare(
                ui,
                &index,
                &layout,
                None,
                SizeMetric::Logical,
                &ClassTable::default(),
                MapStyle::FlatColors,
            );
            let (label, _) = cells[0]
                .label
                .as_ref()
                .expect("the name fits in the header");
            // A folder is identified by its name: in a band too narrow for both, the name stays
            // and the size goes, because the number is still in the tooltip and the status bar
            // while the name appears nowhere else.
            assert!(
                label.job.text.starts_with("System"),
                "header lost its name: {:?}",
                label.job.text
            );
            assert!(
                !label.job.text.contains("KiB"),
                "a cut band must drop the size, not show it: {:?}",
                label.job.text
            );
        });
    }

    #[test]
    fn folder_names_use_the_smallest_face_before_losing_characters() {
        let context = egui::Context::default();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            let name = "SystemApplicationData";
            let mut index = Index::new(name.into()).unwrap();
            let root = index.root();
            index.set_state(root, State::Complete).unwrap();
            index
                .insert(
                    root,
                    "file.bin".into(),
                    Kind::File,
                    hogmap::index::Sizes {
                        logical: Some(4096),
                        allocated: Some(4096),
                    },
                    None,
                    State::Complete,
                )
                .unwrap();
            let full = text::header(name, "4.0 KiB");
            let width = ui
                .painter()
                .layout_no_wrap(full.clone(), FontId::proportional(7.5), Color32::BLACK)
                .size()
                .x
                + 9.0;
            let layout = hogmap::treemap::layout(
                &index,
                root,
                Rect::new(0.0, 0.0, width, 100.0),
                hogmap::treemap::LayoutParams {
                    min_pixel_area: 1.0,
                    header_height: 20.0,
                    max_depth: 1,
                },
            )
            .unwrap();
            let cells = prepare(
                ui,
                &index,
                &layout,
                None,
                SizeMetric::Logical,
                &ClassTable::default(),
                MapStyle::FlatColors,
            );
            let (label, _) = cells[0]
                .label
                .as_ref()
                .expect("the complete header fits at 7.5 points");
            assert_eq!(label.job.text, full);
            assert_eq!(label.job.sections[0].format.font_id.size, 7.5);
        });
    }

    #[test]
    fn a_narrow_folder_band_keeps_its_name_instead_of_only_the_size() {
        let context = egui::Context::default();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            let name = "SystemApplicationData";
            let mut index = Index::new(name.into()).unwrap();
            let root = index.root();
            index.set_state(root, State::Complete).unwrap();
            index
                .insert(
                    root,
                    "file.bin".into(),
                    Kind::File,
                    hogmap::index::Sizes {
                        logical: Some(4096),
                        allocated: Some(4096),
                    },
                    None,
                    State::Complete,
                )
                .unwrap();
            // Wide enough for the name at the smallest face, too narrow for the name plus the
            // size at that same face, so the size is what has to go.
            let width = ui
                .painter()
                .layout_no_wrap(name.into(), FontId::proportional(7.5), Color32::BLACK)
                .size()
                .x
                + 10.0;
            let layout = hogmap::treemap::layout(
                &index,
                root,
                Rect::new(0.0, 0.0, width, 100.0),
                hogmap::treemap::LayoutParams {
                    min_pixel_area: 1.0,
                    header_height: 20.0,
                    max_depth: 1,
                },
            )
            .unwrap();
            let cells = prepare(
                ui,
                &index,
                &layout,
                None,
                SizeMetric::Logical,
                &ClassTable::default(),
                MapStyle::FlatColors,
            );
            let (label, _) = cells[0]
                .label
                .as_ref()
                .expect("the name fits once the size is dropped");
            assert_eq!(label.job.text, name, "the folder keeps its name");
        });
    }

    #[test]
    fn labels_fit_bounds_headers_and_the_three_character_minimum() {
        let context = egui::Context::default();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            let font = FontId::proportional(11.0);
            assert_eq!(
                ui.fonts_mut(|fonts| fonts.glyph_width(&font, '\u{f0000}')),
                0.0
            );
            let narrow = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(20.0, 24.0));
            let roomy = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(48.0, 24.0));
            for (long, three, two) in [
                (
                    "\u{f0000}\u{f0000}\u{f0000}\u{f0000}\u{f0000}\u{f0000}",
                    "\u{f0000}\u{f0000}\u{f0000}",
                    "\u{f0000}\u{f0000}",
                ),
                ("🫨🫨🫨🫨🫨🫨", "🫨🫨🫨", "🫨🫨"),
                ("漢漢漢漢漢漢", "漢漢漢", "漢漢"),
            ] {
                let raw =
                    ui.fonts_mut(|fonts| fonts.glyph_width(&font, long.chars().next().unwrap()));
                let fitted = fit(ui, long, narrow, true);
                println!(
                    "unit66_glyph_fit raw_width={raw} returned_width={:?}",
                    fitted.as_ref().map(|(galley, _)| galley.size().x)
                );
                if let Some((galley, position)) = fitted {
                    assert!(
                        galley.size().x <= 20.0,
                        "overflow for {long:?}: {:?}",
                        galley.size()
                    );
                    assert!(
                        narrow.contains_rect(egui::Rect::from_min_size(position, galley.size()))
                    );
                    assert!(
                        galley
                            .job
                            .text
                            .trim_end_matches(text::ELLIPSIS)
                            .chars()
                            .count()
                            >= 3
                    );
                }
                let (truncated, _) = fit(ui, long, roomy, false).expect("a readable prefix fits");
                assert!(truncated.size().x <= 48.0);
                assert!(truncated.job.text.ends_with(text::ELLIPSIS));
                assert!(
                    truncated
                        .job
                        .text
                        .trim_end_matches(text::ELLIPSIS)
                        .chars()
                        .count()
                        >= 3
                );
                let (galley, _) =
                    fit(ui, three, roomy, false).expect("three replacement glyphs fit");
                assert_eq!(galley.job.text, three);
                assert!(galley.size().x <= 48.0);
                assert!(
                    fit(ui, two, roomy, false).is_none(),
                    "two characters stay hidden"
                );
            }
            let mut index = Index::new("Root header".into()).unwrap();
            let root = index.root();
            let first = index.insert(root, "First header".into(), Kind::Directory, Default::default(), None, State::Complete).unwrap();
            let second = index.insert(first, "Second header".into(), Kind::Directory, Default::default(), None, State::Complete).unwrap();
            index.insert(second, "leaf.bin".into(), Kind::File, hogmap::index::Sizes { logical: Some(4096), allocated: Some(4096) }, None, State::Complete).unwrap();
            index.set_state(root, State::Complete).unwrap();
            let layout = hogmap::treemap::layout(&index, root, Rect::new(0.0, 0.0, 800.0, 600.0), hogmap::treemap::LayoutParams { min_pixel_area: 1.0, header_height: 20.0, max_depth: 2 }).unwrap();
            let cells = prepare(ui, &index, &layout, None, SizeMetric::Logical, &ClassTable::default(), MapStyle::FlatColors);
            assert_eq!(cells.len(), 3);
            for ((placed, cell), height) in layout.placed.iter().zip(&cells).zip([20.0, 16.0, 15.0]) {
                let header = placed.header.unwrap();
                assert_eq!(header.height, height);
                if placed.expanded {
                    let (galley, _) = cell.label.as_ref().expect("every open folder's header fits its label");
                    assert!(galley.size().y + 2.0 <= header.height);
                    assert!(galley.size().x + 8.0 <= header.width);
                    assert!(cell.center.is_none(), "an open folder has no centred label");
                } else {
                    // The folder at the drawn depth is closed: named once, in its middle.
                    let (galley, position) = cell.center.as_ref().expect("a closed folder is named in its middle");
                    assert!(cell.label.is_none(), "and not again in its band");
                    let interior = rect(placed.content);
                    assert!(interior.contains_rect(egui::Rect::from_min_size(*position, galley.size())));
                    assert_eq!(galley.job.text, "Second header\n4.0 KiB");
                }
            }
            let (size_label, _) = fit(ui, "Example.bin\n4.0 KiB", egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(128.0, 40.0)), true).unwrap();
            assert_eq!(size_label.job.text, "Example.bin\n4.0 KiB");
            // Name and number share one face, so the job may merge them into a single section.
            assert!(!size_label.job.sections.is_empty());
            for section in &size_label.job.sections {
                assert_eq!(section.format.font_id, FontId::proportional(11.0));
            }
            println!("unit67_labels header_heights=20,16,15 header_labels=3 numeric_font_proportional=1 minimum_chars=3");
        });
    }
}
