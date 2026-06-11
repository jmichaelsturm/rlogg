// main.rs — entry point
// Initializes the egui/eframe application window and launches the app.

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Regex Filter Viewer")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Regex Filter Viewer",
        native_options,
        Box::new(|_cc| Ok(Box::new(app::FilterApp::default()))),
    )
}

mod app;
