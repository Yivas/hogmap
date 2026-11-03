mod ui;

fn main() -> std::process::ExitCode {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(ui::text::TITLE)
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([640.0, 360.0])
            .with_icon(eframe::egui::IconData::default()),
        renderer: eframe::Renderer::Glow,
        persist_window: true,
        ..Default::default()
    };
    match eframe::run_native(
        ui::text::TITLE,
        options,
        Box::new(|context| Ok(Box::new(ui::HogMap::new(context)?))),
    ) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", ui::text::START_ERROR);
            std::process::ExitCode::FAILURE
        }
    }
}
