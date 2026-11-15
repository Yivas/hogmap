mod ui;

fn main() -> std::process::ExitCode {
    let startup = match hogmap::cli::parse(std::env::args_os().skip(1)) {
        hogmap::cli::Parsed::Start(startup) => startup,
        hogmap::cli::Parsed::Exit { code, message } => {
            if code == 0 {
                println!("{message}");
            } else {
                eprintln!("{message}");
            }
            return std::process::ExitCode::from(code);
        }
    };
    if let Err(message) = hogmap::cli::validate_scan_path(&startup) {
        eprintln!("{message}");
        return std::process::ExitCode::from(2);
    }
    let snapshot = if let Some(path) = &startup.open {
        match std::fs::File::open(path)
            .map_err(|error| error.to_string())
            .and_then(|file| {
                hogmap::snapshot::decode(std::io::BufReader::new(file))
                    .map_err(|error| error.to_string())
            }) {
            Ok(snapshot) => Some(snapshot),
            Err(message) => {
                eprintln!("Cannot open snapshot: {message}\n\n{}", hogmap::cli::USAGE);
                return std::process::ExitCode::from(2);
            }
        }
    } else {
        None
    };
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
        Box::new(move |context| Ok(Box::new(ui::HogMap::new(context, startup, snapshot)?))),
    ) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", ui::text::START_ERROR);
            std::process::ExitCode::FAILURE
        }
    }
}
