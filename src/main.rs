// main.rs — entry point
// Initializes the egui/eframe application window and launches the app.

use egui::{FontData, FontDefinitions, FontFamily};

/// Names used to refer to each bundled font family throughout the app.
/// These are also used as the egui::FontFamily::Name() keys.
pub const FONT_EGUI_DEFAULT: &str = "egui-default"; // built-in, no bytes needed
pub const FONT_JETBRAINS_MONO: &str = "JetBrains Mono";
pub const FONT_FIRA_CODE: &str = "Fira Code";
pub const FONT_SOURCE_CODE_PRO: &str = "Source Code Pro";

/// All selectable font names, in the order they should appear in the
/// Settings dropdown. "egui-default" first since it's the zero-setup option
/// that always works even if a user removes the bundled font files.
pub const AVAILABLE_FONTS: &[&str] = &[
    FONT_EGUI_DEFAULT,
    FONT_JETBRAINS_MONO,
    FONT_FIRA_CODE,
    FONT_SOURCE_CODE_PRO,
];

/// Register all bundled monospace fonts as named font families.
///
/// egui ships its own default monospace font built in, so FONT_EGUI_DEFAULT
/// needs no extra registration — it already exists as FontFamily::Monospace.
/// The other three are loaded from files you place in `assets/fonts/` and
/// embedded into the binary at compile time via include_bytes!.
///
/// IMPORTANT: download the following open-license .ttf files yourself and
/// place them at these exact paths before building:
///   assets/fonts/JetBrainsMono-Regular.ttf   (SIL Open Font License)
///   assets/fonts/FiraCode-Regular.ttf        (SIL Open Font License)
///   assets/fonts/SourceCodePro-Regular.ttf   (SIL Open Font License)
///
/// JetBrains Mono: https://github.com/JetBrains/JetBrainsMono/releases
/// Fira Code:      https://github.com/tonsky/FiraCode/releases
/// Source Code Pro: https://github.com/adobe-fonts/source-code-pro/releases
fn register_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    // Each call: load the file's bytes, give them a font_data key, then map
    // that key into a *new* named FontFamily so the rest of the app can
    // select it independently of egui's built-in Monospace/Proportional.
    let bundled: &[(&str, &[u8])] = &[
        (
            FONT_JETBRAINS_MONO,
            include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
        ),
        (
            FONT_FIRA_CODE,
            include_bytes!("../assets/fonts/FiraCode-Regular.ttf"),
        ),
        (
            FONT_SOURCE_CODE_PRO,
            include_bytes!("../assets/fonts/SourceCodePro-Regular.ttf"),
        ),
    ];

    for (name, bytes) in bundled {
        fonts
            .font_data
            .insert((*name).to_owned(), FontData::from_static(bytes).into());

        fonts.families.insert(
            FontFamily::Name((*name).into()),
            vec![(*name).to_owned()],
        );
    }

    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rlogg")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };

    eframe::run_native(
        "rlogg",
        native_options,
        Box::new(|cc| {
            register_fonts(&cc.egui_ctx);
            Ok(Box::new(app::FilterApp::default()))
        }),
    )
}

mod app;
mod line_index;
