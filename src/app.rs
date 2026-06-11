// app.rs — Main application state and egui rendering logic.
//
// Layout (top-to-bottom):
//   ┌──────────────────────────────────┐
//   │  TOP PANE — full file viewer     │  CentralPanel (fills remaining space)
//   │  (virtual-scroll, all lines)     │
//   ├──────────────────────────────────┤
//   │  🔍 [regex input     ] [Filter]  │  Fixed-height middle strip
//   │  N matches found                 │
//   ├──────────────────────────────────┤
//   │  BOTTOM PANE — filtered matches  │  BottomPanel (resizable)
//   │  (click a line → top scrolls)   │
//   └──────────────────────────────────┘
//
// SCROLL DESIGN NOTE
// ==================
// Virtual scrolling (show_rows) only renders the rows that are currently
// visible. This means scroll_to_me() cannot work for off-screen rows — the
// widget simply never gets painted, so there is nothing to scroll to.
//
// The correct approach is to drive the scroll offset directly via
// ScrollArea::vertical_scroll_offset(), computed from the row height and the
// real viewport height. The viewport height is read from ScrollArea::show()'s
// output (output.inner_rect.height()), which is accurate after the first frame.
//
// To avoid a one-frame lag where the viewport size is not yet known, we use a
// two-field deferred mechanism:
//
//   top_pane_scroll_target   — set on click; holds the target line number.
//   top_pane_viewport_height — updated every frame from output.inner_rect;
//                              valid from frame 2 onward (f32::NAN on frame 1).
//
// On the click frame (frame N):
//   - top_pane_scroll_target is set.
//   - top_pane_viewport_height is already accurate from frame N-1's paint.
//   - The offset is computed and applied immediately.
//   - egui re-paints with the new offset the same frame.
//
// This means the scroll is correct in a single frame as long as the app has
// rendered at least once before the click — which is always true in practice.

use std::{
    path::PathBuf,
    sync::{mpsc, Arc},
};

use egui::{Context, ScrollArea, Ui};

// ---------------------------------------------------------------------------
// Types that mirror large-text-core's API.
// Replace these thin wrappers with the real crate types when integrating.
// ---------------------------------------------------------------------------

/// Represents an open, memory-mapped file.
/// In the real app this is `large_text_core::FileReader`.
pub struct FileReader {
    lines: Vec<String>,
}

impl FileReader {
    pub fn open(path: &PathBuf) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let lines = content.lines().map(str::to_owned).collect();
        Ok(Self { lines })
    }

    /// Return the text of a single line by 0-based index.
    /// In the real app, `large_text_core::LineIndexer::get_line()` converts a
    /// line number to a byte offset and decodes just those bytes from the mmap.
    pub fn get_line(&self, line_no: usize) -> Option<&str> {
        self.lines.get(line_no).map(String::as_str)
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

// ---------------------------------------------------------------------------
// Background search
// ---------------------------------------------------------------------------

pub enum SearchMessage {
    Match(usize),
    Done,
}

/// Compile `pattern` and scan `reader` on a background thread, streaming
/// matched line numbers back via `tx`.
///
/// In the real app replace the body with `large_text_core::SearchEngine`
/// which uses rayon par_chunks over the mmap for near-linear CPU scaling.
fn start_search(reader: Arc<FileReader>, pattern: String, tx: mpsc::Sender<SearchMessage>) {
    std::thread::spawn(move || {
        let Ok(re) = regex::Regex::new(&pattern) else {
            let _ = tx.send(SearchMessage::Done);
            return;
        };
        for line_no in 0..reader.line_count() {
            if let Some(line) = reader.get_line(line_no) {
                if re.is_match(line) {
                    if tx.send(SearchMessage::Match(line_no)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = tx.send(SearchMessage::Done);
    });
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct FilterApp {
    // --- File ----------------------------------------------------------------
    file: Option<Arc<FileReader>>,
    file_path: Option<PathBuf>,

    // --- Search --------------------------------------------------------------
    regex_input: String,
    regex_error: Option<String>,
    search_rx: Option<mpsc::Receiver<SearchMessage>>,
    search_running: bool,

    // --- Results -------------------------------------------------------------
    /// 0-based line numbers of matched lines, in file order.
    /// Only offsets are stored — no line text is copied.
    match_line_numbers: Vec<usize>,
    /// Index into match_line_numbers for the currently selected result.
    selected_match: Option<usize>,

    // --- Scroll state --------------------------------------------------------
    /// Line number to center in the top pane. Set on click, consumed each frame.
    top_pane_scroll_target: Option<usize>,
    /// Real inner height of the top pane's scroll viewport in pixels.
    /// Captured from ScrollArea output every frame. NAN until first paint.
    top_pane_viewport_height: f32,
}

impl Default for FilterApp {
    fn default() -> Self {
        Self {
            file: None,
            file_path: None,
            regex_input: String::new(),
            regex_error: None,
            search_rx: None,
            search_running: false,
            match_line_numbers: Vec::new(),
            selected_match: None,
            top_pane_scroll_target: None,
            top_pane_viewport_height: f32::NAN,
        }
    }
}

impl FilterApp {
    // -----------------------------------------------------------------------
    // File
    // -----------------------------------------------------------------------

    fn open_file(&mut self, path: PathBuf) {
        match FileReader::open(&path) {
            Ok(reader) => {
                self.file = Some(Arc::new(reader));
                self.file_path = Some(path);
                self.clear_search();
            }
            Err(e) => eprintln!("Failed to open file: {e}"),
        }
    }

    fn clear_search(&mut self) {
        self.match_line_numbers.clear();
        self.selected_match = None;
        self.search_rx = None;
        self.search_running = false;
        self.regex_error = None;
        self.top_pane_scroll_target = None;
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    fn run_search(&mut self) {
        let Some(reader) = &self.file else { return };

        if let Err(e) = regex::Regex::new(&self.regex_input) {
            self.regex_error = Some(format!("Invalid regex: {e}"));
            return;
        }

        self.regex_error = None;
        self.match_line_numbers.clear();
        self.selected_match = None;
        self.search_running = true;

        let (tx, rx) = mpsc::channel();
        self.search_rx = Some(rx);
        start_search(Arc::clone(reader), self.regex_input.clone(), tx);
    }

    /// Drain available search results without blocking. Called every frame.
    fn poll_search_results(&mut self) {
        let Some(rx) = &self.search_rx else { return };
        for _ in 0..10_000 {
            match rx.try_recv() {
                Ok(SearchMessage::Match(line_no)) => self.match_line_numbers.push(line_no),
                Ok(SearchMessage::Done) => {
                    self.search_running = false;
                    self.search_rx = None;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.search_running = false;
                    self.search_rx = None;
                    break;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // UI sections
    // -----------------------------------------------------------------------

    fn show_search_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("🔍");
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.regex_input)
                    .hint_text("Enter regex pattern…")
                    .desired_width(f32::INFINITY),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.run_search();
            }
            if ui
                .add_enabled(!self.search_running, egui::Button::new("Filter"))
                .clicked()
            {
                self.run_search();
            }
            if ui.button("✕ Clear").clicked() {
                self.clear_search();
            }
        });

        ui.horizontal(|ui| {
            if self.search_running {
                ui.spinner();
                ui.label(format!(
                    "Searching… {} matches so far",
                    self.match_line_numbers.len()
                ));
            } else if let Some(err) = &self.regex_error {
                ui.colored_label(egui::Color32::RED, err);
            } else if !self.match_line_numbers.is_empty() {
                ui.label(format!("{} matches found", self.match_line_numbers.len()));
            } else if self.regex_input.is_empty() {
                ui.label("Type a regex above and press Enter or click Filter.");
            } else {
                ui.label("No matches.");
            }
        });
    }

    fn show_top_pane(&mut self, ui: &mut Ui) {
        let Some(file) = &self.file else {
            ui.centered_and_justified(|ui| {
                ui.label("No file open. Use File → Open… to load a file.");
            });
            return;
        };

        let total_lines = file.line_count();
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let total_height = total_lines as f32 * row_height;

        // Resolve the selected line for highlighting (independent of scroll).
        let selected_line = self
            .selected_match
            .and_then(|m| self.match_line_numbers.get(m))
            .copied();

        // ── Scroll offset computation ──────────────────────────────────────
        //
        // We use ScrollArea::show() (not show_rows) so that vertical_scroll_offset
        // operates in a simple coordinate space where pixel Y = line * row_height
        // with no virtualisation spacers interfering.
        //
        // show_rows() internally inserts invisible spacer widgets above and below
        // the visible rows to simulate the full content height. Those spacers shift
        // the coordinate origin, so vertical_scroll_offset ends up pointing at the
        // wrong place. By doing the virtualisation ourselves inside show(), we
        // control the coordinate space directly.
        //
        //   offset = (target_line * row_height) - (viewport_height / 2)
        //
        // This puts the top of the target row at the centre of the viewport.
        // egui clamps the offset to [0, total_height - viewport_height].

        let mut scroll_area = ScrollArea::vertical()
            .id_salt("top_pane")
            .auto_shrink([false; 2]);

        if let Some(target_line) = self.top_pane_scroll_target {
            if self.top_pane_viewport_height.is_finite() {
                let target_top_px = target_line as f32 * row_height;
                let offset = (target_top_px - self.top_pane_viewport_height / 2.0).max(0.0);
                scroll_area = scroll_area.vertical_scroll_offset(offset);
                self.top_pane_scroll_target = None;
            }
            // If NAN (very first frame before any paint), leave target set;
            // it will be applied next frame when the height is known.
        }

        // ── Render with manual virtualisation ─────────────────────────────
        //
        // We allocate the full virtual content height in one shot, then derive
        // which rows are currently visible from the scroll offset. Only those
        // rows get widgets — identical to what show_rows does internally, but
        // without the spacer widgets that corrupt the coordinate space.

        let output = scroll_area.show(ui, |ui| {
            // Reserve the full virtual height so the scrollbar is sized correctly.
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), total_height),
                egui::Sense::hover(),
            );

            // Current scroll offset within this scroll area.
            let scroll_top = ui.clip_rect().min.y - rect.min.y;
            let scroll_top = scroll_top.max(0.0);

            // Which rows are visible?
            let first_visible = (scroll_top / row_height).floor() as usize;
            let last_visible = ((scroll_top + ui.clip_rect().height()) / row_height).ceil()
                as usize;
            let last_visible = last_visible.min(total_lines.saturating_sub(1));

            let painter = ui.painter();

            for line_no in first_visible..=last_visible {
                let y_top = rect.min.y + line_no as f32 * row_height;
                let row_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.min.x, y_top),
                    egui::vec2(rect.width(), row_height),
                );

                // Amber highlight for the selected line.
                if selected_line == Some(line_no) {
                    painter.rect_filled(
                        row_rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(255, 200, 0, 40),
                    );
                }

                let text = file.get_line(line_no).unwrap_or("");
                let line_label = format!("{:>6}  {}", line_no + 1, text);

                painter.text(
                    egui::pos2(row_rect.min.x + 4.0, y_top),
                    egui::Align2::LEFT_TOP,
                    line_label,
                    egui::TextStyle::Monospace.resolve(ui.style()),
                    ui.visuals().text_color(),
                );
            }
        });

        // Capture real viewport height for the next frame's scroll computation.
        self.top_pane_viewport_height = output.inner_rect.height();
    }

    fn show_bottom_pane(&mut self, ui: &mut Ui) {
        let match_count = self.match_line_numbers.len();

        if match_count == 0 {
            ui.centered_and_justified(|ui| {
                if self.regex_input.is_empty() {
                    ui.label("Filter results will appear here.");
                } else {
                    ui.label("No matches for the current pattern.");
                }
            });
            return;
        }

        let Some(file) = &self.file else { return };
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);

        ScrollArea::vertical()
            .id_salt("bottom_pane")
            .auto_shrink([false; 2])
            .show_rows(ui, row_height, match_count, |ui, visible_range| {
                for match_idx in visible_range {
                    let line_no = self.match_line_numbers[match_idx];
                    let text = file.get_line(line_no).unwrap_or("");
                    let line_label = format!("{:>6}  {}", line_no + 1, text);

                    let is_selected = self.selected_match == Some(match_idx);

                    let response = ui.add(egui::SelectableLabel::new(
                        is_selected,
                        egui::RichText::new(line_label).monospace(),
                    ));

                    if response.clicked() {
                        self.selected_match = Some(match_idx);
                        // Set the scroll target. The offset will be computed in
                        // show_top_pane on this same frame using the viewport
                        // height captured last frame — which is accurate.
                        self.top_pane_scroll_target = Some(line_no);
                    }
                }
            });
    }
}

// ---------------------------------------------------------------------------
// egui App trait
// ---------------------------------------------------------------------------

impl eframe::App for FilterApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_search_results();

        if self.search_running {
            ctx.request_repaint();
        }

        // Menu bar
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_file() {
                            self.open_file(path);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });

        // Bottom pane — declared before CentralPanel so egui allocates its
        // space first and the top pane fills the remainder.
        egui::TopBottomPanel::bottom("bottom_pane_container")
            .resizable(true)
            .min_height(80.0)
            .default_height(200.0)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                self.show_bottom_pane(ui);
            });

        // Search bar — also bottom-anchored, sits just above the bottom pane.
        egui::TopBottomPanel::bottom("search_bar")
            .resizable(false)
            .exact_height(56.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                self.show_search_bar(ui);
            });

        // Top pane — fills whatever space remains.
        egui::CentralPanel::default().show(ctx, |ui| {
            self.show_top_pane(ui);
        });
    }
}
