use eframe::egui;
use hogmap::filter::{ClassColor, ClassTable};

use super::{logic::HighlightFilter, text};

// These helpers live with the desktop editor; navigation tests also include logic.rs without UI.
impl HighlightFilter {
    pub fn active_expression(&self) -> &str {
        &self.active_expression
    }

    pub fn reparse(&mut self, classes: &ClassTable) {
        let expression = self.active_expression.clone();
        if !self.set_with_classes(&expression, classes) {
            self.missing_class = expression.split(';').find_map(|condition| {
                let name = condition
                    .trim()
                    .trim_start_matches(['+', '-'])
                    .strip_prefix(":class:")?;
                (!classes
                    .entries()
                    .any(|(candidate, _)| candidate.eq_ignore_ascii_case(name)))
                .then(|| name.to_owned())
            });
        }
    }

    pub fn uses_class(&self, name: &str) -> bool {
        self.active_expression.split(';').any(|condition| {
            condition
                .trim()
                .trim_start_matches(['+', '-'])
                .strip_prefix(":class:")
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        })
    }
}

pub const COLORS: [ClassColor; 5] = [
    ClassColor::Blue,
    ClassColor::Green,
    ClassColor::Purple,
    ClassColor::Amber,
    ClassColor::Rose,
];

#[derive(Default)]
pub struct ClassEditor {
    pub open: bool,
    selected: Option<String>,
    name: String,
    extensions: String,
    color: usize,
    error: Option<&'static str>,
    confirm_delete: bool,
    focus_name: bool,
}

impl ClassEditor {
    pub fn reopen(&mut self) {
        self.open = true;
        self.focus_name = true;
    }

    fn load(&mut self, name: &str, table: &ClassTable) {
        if let Some((name, class)) = table.entries().find(|(candidate, _)| *candidate == name) {
            self.selected = Some(name.into());
            self.name = name.into();
            self.extensions = class.extensions.join(" ");
            self.color = COLORS
                .iter()
                .position(|color| *color == class.color)
                .unwrap_or(0);
            self.error = None;
            self.confirm_delete = false;
        }
    }

    fn apply(&mut self, table: &mut ClassTable) -> bool {
        let name = self.name.trim().to_ascii_lowercase();
        let extensions: Vec<_> = self.extensions.split_whitespace().collect();
        if self.selected.as_deref() != Some(name.as_str())
            && table.entries().any(|(candidate, _)| candidate == name)
        {
            self.error = Some("A class with this name already exists. Choose another name.");
            return false;
        }
        if table.set(&name, &extensions).is_err() {
            self.error = Some(
                "Use a non-empty name and at least one extension, without dots. Only ASCII letters, digits, _ and - are allowed.",
            );
            return false;
        }
        table
            .set_color(&name, COLORS[self.color])
            .expect("the class was just installed");
        self.selected = Some(name.clone());
        self.name = name;
        self.error = None;
        true
    }

    pub fn show(
        &mut self,
        context: &egui::Context,
        table: &mut ClassTable,
        filter: &HighlightFilter,
    ) -> bool {
        if !self.open {
            return false;
        }
        let mut changed = false;
        let modal = egui::Modal::new(egui::Id::new("file-classes")).show(context, |ui| {
            ui.set_width((context.content_rect().width() - 64.0).clamp(300.0, 600.0));
            ui.heading(text::FILE_CLASSES);
            ui.label(text::CLASS_SESSION);
            egui::ScrollArea::vertical()
                .max_height((context.content_rect().height() - 340.0).clamp(60.0, 220.0))
                .show(ui, |ui| {
                    let mut chosen = None;
                    for (name, class) in table.entries() {
                        let label = format!(
                            "{name} · {} · {}",
                            class.extensions.join(", "),
                            text::CLASS_COLORS[COLORS
                                .iter()
                                .position(|color| *color == class.color)
                                .unwrap_or(0)]
                        );
                        if ui
                            .selectable_label(self.selected.as_deref() == Some(name), label)
                            .clicked()
                        {
                            chosen = Some(name.to_owned());
                        }
                    }
                    if let Some(name) = chosen {
                        self.load(&name, table);
                    }
                });
            ui.separator();
            if ui.button(text::CLASS_NEW).clicked() {
                self.selected = None;
                self.name.clear();
                self.extensions.clear();
                self.error = None;
                self.confirm_delete = false;
                self.focus_name = true;
            }
            let name_label = ui.label(text::CLASS_NAME);
            let name = ui
                .add_enabled(
                    self.selected.is_none(),
                    egui::TextEdit::singleline(&mut self.name).char_limit(64),
                )
                .labelled_by(name_label.id);
            if self.focus_name {
                if self.selected.is_none() {
                    name.request_focus();
                }
                self.focus_name = false;
            }
            let extensions_label = ui.label(text::CLASS_EXTENSIONS);
            ui.add(egui::TextEdit::singleline(&mut self.extensions).char_limit(4096))
                .labelled_by(extensions_label.id);
            let color = egui::ComboBox::from_id_salt("class-color")
                .selected_text(text::CLASS_COLORS[self.color])
                .show_ui(ui, |ui| {
                    for (slot, name) in text::CLASS_COLORS.iter().enumerate() {
                        ui.selectable_value(&mut self.color, slot, *name);
                    }
                })
                .response;
            color.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    format!("{}: {}", text::CLASS_COLOR, text::CLASS_COLORS[self.color]),
                )
            });
            if let Some(error) = self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            if self.confirm_delete {
                if let Some(name) = self.selected.clone() {
                    ui.label(text::confirm_class_delete(
                        &name,
                        filter.active_expression(),
                    ));
                    ui.horizontal(|ui| {
                        if ui.button(text::CLASS_DELETE).clicked() {
                            changed = table.remove(&name);
                            self.selected = None;
                            self.name.clear();
                            self.extensions.clear();
                            self.confirm_delete = false;
                        }
                        if ui.button(text::CLASS_CANCEL).clicked() {
                            self.confirm_delete = false;
                        }
                    });
                }
            } else {
                ui.horizontal(|ui| {
                    if ui.button(text::CLASS_SAVE).clicked() {
                        changed = self.apply(table);
                    }
                    if ui
                        .add_enabled(
                            self.selected.is_some(),
                            egui::Button::new(text::CLASS_DELETE),
                        )
                        .clicked()
                        && let Some(name) = &self.selected
                    {
                        if filter.uses_class(name) {
                            self.confirm_delete = true;
                        } else {
                            changed = table.remove(name);
                            self.selected = None;
                            self.name.clear();
                            self.extensions.clear();
                        }
                    }
                });
            }
            if ui.button(text::CLOSE).clicked() {
                self.open = false;
            }
        });
        modal.response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Window, true, text::FILE_CLASSES)
        });
        if modal.should_close() {
            self.open = false;
            self.confirm_delete = false;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hogmap::{
        filter::Match,
        index::{Index, Kind, Sizes, State},
    };

    #[test]
    fn class_editor_validates_and_reparses_the_active_expression_not_an_invalid_draft() {
        let mut table = ClassTable::default();
        let mut editor = ClassEditor::default();
        for (name, extensions) in [
            ("", "bin"),
            ("work", ""),
            ("work", ".bin"),
            ("IMAGES", "bin"),
        ] {
            editor.name = name.into();
            editor.extensions = extensions.into();
            assert!(!editor.apply(&mut table), "{name:?}/{extensions:?}");
        }
        editor.name = "work".into();
        editor.extensions = "bin".into();
        assert!(editor.apply(&mut table));
        let mut index = Index::new("Fixture".into()).unwrap();
        let id = index
            .insert(
                index.root(),
                "file.bin".into(),
                Kind::File,
                Sizes {
                    logical: Some(10),
                    allocated: Some(10),
                },
                None,
                State::Complete,
            )
            .unwrap();
        let mut filter = HighlightFilter::default();
        assert!(filter.set_with_classes(":class:work", &table));
        assert_eq!(filter.matches(&index)[id.position()], Match::Yes);
        assert!(!filter.set_with_classes("m<bad", &table));
        editor.extensions = "txt".into();
        assert!(editor.apply(&mut table));
        filter.reparse(&table);
        assert_eq!(filter.active_expression(), ":class:work");
        assert_eq!(filter.matches(&index)[id.position()], Match::No);
        assert!(table.remove("work"));
        filter.reparse(&table);
        assert_eq!(filter.missing_class.as_deref(), Some("work"));
        assert_eq!(filter.matches(&index)[id.position()], Match::No);
        table.set("work", &["bin"]).unwrap();
        filter.reparse(&table);
        assert_eq!(filter.missing_class, None);
        assert_eq!(filter.matches(&index)[id.position()], Match::Yes);
    }
}
