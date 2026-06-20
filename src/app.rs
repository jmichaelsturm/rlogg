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

use std::{
    collections::{BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
};

use egui::{Context, ScrollArea, Ui};

use crate::{AVAILABLE_FONTS, FONT_EGUI_DEFAULT};

use large_text_core::{
    file_reader::{detect_encoding, FileReader},
    line_indexer::LineIndexer,
    search_engine::{SearchEngine, SearchMessage, SearchType},
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_HISTORY: usize = 20;

/// Maximum matches we ask fetch_matches to return. A large cap keeps memory
/// flat for huge files while still covering typical log-file use cases.
const MAX_SEARCH_RESULTS: usize = 1_000_000;

/// Default monospace font size in points, applied to both panes.
const DEFAULT_FONT_SIZE: f32 = 14.0;
/// Drag-value range for the font size control in Settings.
const MIN_FONT_SIZE: f32 = 8.0;
const MAX_FONT_SIZE: f32 = 32.0;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct FilterApp {
    // --- File ----------------------------------------------------------------
    /// The memory-mapped file reader. Wrapped in Arc so the search thread can
    /// hold a clone without borrowing from self.
    file_reader: Option<Arc<FileReader>>,
    /// Line indexer built from the file reader. Gives us line count and the
    /// ability to map line numbers ↔ byte offsets.
    line_indexer: Option<LineIndexer>,
    file_path: Option<PathBuf>,

    // --- Search --------------------------------------------------------------
    regex_input: String,
    regex_error: Option<String>,
    search_engine: SearchEngine,
    /// Receiver end of the background search channel.
    search_rx: Option<mpsc::Receiver<SearchMessage>>,
    /// Signals the background search thread to stop early (e.g. when the user
    /// starts a new search before the previous one finishes).
    cancel_token: Arc<AtomicBool>,
    search_running: bool,

    // --- History -------------------------------------------------------------
    search_history: VecDeque<String>,

    // --- Results -------------------------------------------------------------
    /// 0-based line numbers of matched lines, in file order.
    /// Only line numbers are stored — no line text is copied.
    match_line_numbers: Vec<usize>,
    /// Indices into match_line_numbers for every currently-selected result row.
    selected_matches: BTreeSet<usize>,
    /// Anchor for shift-click / shift-arrow range selection.
    selection_anchor: Option<usize>,
    /// Moving end of the selection range (updated by shift-click and arrows).
    selection_cursor: Option<usize>,

    // --- Scroll state --------------------------------------------------------
    /// File line number to center in the top pane. Set on click/arrow, consumed
    /// each frame after the scroll offset is applied.
    top_pane_scroll_target: Option<usize>,
    /// Real inner height of the top pane viewport in pixels. Captured from
    /// ScrollArea output every frame. NAN until the first paint.
    top_pane_viewport_height: f32,

    // --- Font settings ---------------------------------------------------------
    /// Currently selected font family name. Must be one of AVAILABLE_FONTS
    /// (defined in main.rs) — either FONT_EGUI_DEFAULT or one of the bundled
    /// font names registered via register_fonts().
    selected_font: String,
    /// Currently selected font size in points, applied to TextStyle::Monospace
    /// (and therefore to both the top and bottom panes, which both derive
    /// row_height from that text style).
    font_size: f32,
    /// Whether the Settings window is currently open.
    show_font_settings: bool,
    /// Set once on the very first frame to apply the default font/size to
    /// egui's style before anything is painted. Without this, the first
    /// frame would render with egui's untouched default style.
    font_settings_applied: bool,

    // --- Theme settings ----------------------------------------------------
    /// Currently selected color theme. Chosen directly by the user via the
    /// Edit menu — no OS/system-theme auto-detection, which proved unreliable
    /// on WSL/GNOME (the XDG portal query times out there).
    theme: egui::Theme,
    /// Set once on the first frame to apply the default theme before
    /// anything is painted, mirroring font_settings_applied.
    theme_applied: bool,
}

impl Default for FilterApp {
    fn default() -> Self {
        Self {
            file_reader: None,
            line_indexer: None,
            file_path: None,
            regex_input: String::new(),
            regex_error: None,
            search_engine: SearchEngine::new(),
            search_rx: None,
            cancel_token: Arc::new(AtomicBool::new(false)),
            search_running: false,
            search_history: VecDeque::new(),
            match_line_numbers: Vec::new(),
            selected_matches: BTreeSet::new(),
            selection_anchor: None,
            selection_cursor: None,
            top_pane_scroll_target: None,
            top_pane_viewport_height: f32::NAN,
            selected_font: FONT_EGUI_DEFAULT.to_owned(),
            font_size: DEFAULT_FONT_SIZE,
            show_font_settings: false,
            font_settings_applied: false,
            theme: egui::Theme::Light,
            theme_applied: false,
        }
    }
}

impl FilterApp {
    // -----------------------------------------------------------------------
    // File opening
    // -----------------------------------------------------------------------

    fn open_file(&mut self, path: PathBuf) {
        // Detect encoding from the first 4KB of the file (BOM / heuristic).
        let encoding = std::fs::read(&path)
            .map(|bytes| detect_encoding(&bytes[..bytes.len().min(4096)]))
            .unwrap_or(encoding_rs::UTF_8);

        match FileReader::new(path.clone(), encoding) {
            Ok(reader) => {
                let reader = Arc::new(reader);

                // Build the line index synchronously. For files < 10 MB this
                // is a full scan; for larger files it falls back to sparse
                // sampling. Either way it completes fast enough for startup.
                let mut indexer = LineIndexer::new();
                indexer.index_file(&reader);

                self.file_reader = Some(reader);
                self.line_indexer = Some(indexer);
                self.file_path = Some(path);
                self.clear_search();
            }
            Err(e) => eprintln!("Failed to open file: {e}"),
        }
    }

    // -----------------------------------------------------------------------
    // Search management
    // -----------------------------------------------------------------------

    fn clear_search(&mut self) {
        // Cancel any in-flight search.
        self.cancel_token.store(true, Ordering::Relaxed);
        self.cancel_token = Arc::new(AtomicBool::new(false));

        self.search_engine.clear();
        self.search_rx = None;
        self.search_running = false;
        self.regex_error = None;
        self.match_line_numbers.clear();
        self.selected_matches.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        self.top_pane_scroll_target = None;
    }

    fn add_to_history(&mut self, pattern: String) {
        if pattern.is_empty() {
            return;
        }
        self.search_history.retain(|p| p != &pattern);
        self.search_history.push_front(pattern);
        if self.search_history.len() > MAX_HISTORY {
            self.search_history.pop_back();
        }
    }

    fn run_search(&mut self) {
        // Clone Arc up front so we own it before any &mut self calls below.
        let Some(reader) = self.file_reader.clone() else {
            return;
        };

        // Cancel any previous in-flight search.
        self.cancel_token.store(true, Ordering::Relaxed);
        self.cancel_token = Arc::new(AtomicBool::new(false));

        // Configure SearchEngine. set_query compiles the regex internally.
        // We always use regex mode (use_regex = true) since our UI is a regex
        // filter. case_sensitive = false for friendlier default behaviour.
        self.search_engine.set_query(
            self.regex_input.clone(),
            true,  // use_regex
            false, // case_sensitive
        );

        // Check if the regex compiled successfully by trying it ourselves.
        if let Err(e) = regex::Regex::new(&self.regex_input) {
            self.regex_error = Some(format!("Invalid regex: {e}"));
            return;
        }

        self.regex_error = None;
        self.add_to_history(self.regex_input.clone());
        self.match_line_numbers.clear();
        self.selected_matches.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        self.search_running = true;

        // SyncSender with a buffer of 256 chunks. The background thread parks
        // when the buffer is full, providing natural back-pressure so we never
        // queue more than ~2.5 GB of results in memory.
        let (tx, rx) = mpsc::sync_channel(256);
        self.search_rx = Some(rx);

        self.search_engine.fetch_matches(
            reader,
            tx,
            0, // start_offset — scan the whole file
            MAX_SEARCH_RESULTS,
            Arc::clone(&self.cancel_token),
        );
    }

    /// Drain available search messages without blocking. Called every frame.
    /// Converts byte-offset SearchResults into line numbers via the indexer.
    fn poll_search_results(&mut self) {
        let Some(rx) = &self.search_rx else { return };
        let Some(indexer) = &self.line_indexer else { return };

        // Drain up to 10 000 messages per frame to keep the UI responsive.
        for _ in 0..10_000 {
            match rx.try_recv() {
                Ok(SearchMessage::ChunkResult(chunk)) => {
                    for result in chunk.matches {
                        // Convert byte offset → 0-based line number.
                        let line_no = indexer.find_line_at_offset(result.byte_offset);
                        // Deduplicate: skip if the last inserted line is the
                        // same (multiple matches on one line → one result row).
                        if self.match_line_numbers.last() != Some(&line_no) {
                            self.match_line_numbers.push(line_no);
                        }
                    }
                }
                Ok(SearchMessage::Done(SearchType::Fetch)) => {
                    self.search_running = false;
                    self.search_rx = None;
                    break;
                }
                Ok(SearchMessage::Error(e)) => {
                    self.regex_error = Some(e);
                    self.search_running = false;
                    self.search_rx = None;
                    break;
                }
                // CountResult messages are not expected here (we use
                // fetch_matches, not count_matches) but handle gracefully.
                Ok(_) => {}
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
    // Keyboard shortcuts
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Theme settings
    // -----------------------------------------------------------------------

    /// Apply `self.theme` to egui's visuals.
    ///
    /// We use set_visuals() rather than set_theme(). set_theme() participates
    /// in egui's ThemePreference::System machinery, which re-evaluates the OS
    /// theme every frame and can silently override a manually chosen theme —
    /// this bit us earlier when trying to auto-detect WSL/GNOME's theme.
    /// set_visuals() sets the rendered visuals directly and unconditionally,
    /// so the user's explicit choice always sticks.
    fn apply_theme(&self, ctx: &Context) {
        let visuals = match self.theme {
            egui::Theme::Dark => egui::Visuals::dark(),
            egui::Theme::Light => egui::Visuals::light(),
        };
        ctx.set_visuals(visuals);
    }

    // -----------------------------------------------------------------------
    // Font settings
    // -----------------------------------------------------------------------

    /// Apply `self.selected_font` and `self.font_size` to egui's style.
    ///
    /// This overrides TextStyle::Monospace's FontId to point at the chosen
    /// family/size. Both panes derive row_height from
    /// `ui.text_style_height(&TextStyle::Monospace)` and render with
    /// `TextStyle::Monospace.resolve(ui.style())`, so changing this one style
    /// entry affects both panes identically with no other code changes needed.
    ///
    /// Cheap to call: this only mutates the Style struct, it doesn't touch
    /// the underlying font atlas/glyph cache, so there's no flicker or
    /// re-registration cost when switching.
    fn apply_font_settings(&self, ctx: &Context) {
        let family = if self.selected_font == FONT_EGUI_DEFAULT {
            egui::FontFamily::Monospace
        } else {
            egui::FontFamily::Name(self.selected_font.clone().into())
        };

        ctx.style_mut(|style| {
            style.text_styles.insert(
                egui::TextStyle::Monospace,
                egui::FontId::new(self.font_size, family),
            );
        });
    }

    /// Renders the Font Settings popup window (opened from Edit menu).
    /// Returns true if any setting changed this frame, so the caller can
    /// re-apply the style immediately rather than waiting a frame.
    fn show_font_settings_window(&mut self, ctx: &Context) {
        if !self.show_font_settings {
            return;
        }

        let mut changed = false;
        let mut still_open = true;

        egui::Window::new("Font Settings")
            .open(&mut still_open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Font:");
                    egui::ComboBox::from_id_salt("font_family_combo")
                        .selected_text(&self.selected_font)
                        .show_ui(ui, |ui| {
                            for &name in AVAILABLE_FONTS {
                                let is_selected = self.selected_font == name;
                                if ui
                                    .selectable_label(is_selected, name)
                                    .clicked()
                                    && !is_selected
                                {
                                    self.selected_font = name.to_owned();
                                    changed = true;
                                }
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("Size:");
                    let response = ui.add(
                        egui::DragValue::new(&mut self.font_size)
                            .range(MIN_FONT_SIZE..=MAX_FONT_SIZE)
                            .suffix(" pt"),
                    );
                    if response.changed() {
                        changed = true;
                    }
                });

                ui.separator();

                // Live preview, rendered in the chosen font/size so the user
                // can see the effect before closing the window.
                let family = if self.selected_font == FONT_EGUI_DEFAULT {
                    egui::FontFamily::Monospace
                } else {
                    egui::FontFamily::Name(self.selected_font.clone().into())
                };
                ui.label(
                    egui::RichText::new("The quick brown fox jumps 0123456789")
                        .font(egui::FontId::new(self.font_size, family)),
                );
            });

        if !still_open {
            self.show_font_settings = false;
        }
        if changed {
            self.apply_font_settings(ctx);
        }
    }

    // -----------------------------------------------------------------------
    // Clipboard
    // -----------------------------------------------------------------------

    fn copy_selected_to_clipboard(&self, ctx: &Context) {
        let (Some(reader), Some(indexer)) = (&self.file_reader, &self.line_indexer) else {
            return;
        };
        if self.selected_matches.is_empty() {
            return;
        }

        let mut text = String::new();
        for (i, &match_idx) in self.selected_matches.iter().enumerate() {
            if let Some(&line_no) = self.match_line_numbers.get(match_idx) {
                if i > 0 {
                    text.push('\n');
                }
                // get_line_with_reader returns (start_byte, end_byte).
                if let Some((start, end)) = indexer.get_line_with_reader(line_no, reader) {
                    let line = reader.get_chunk(start, end);
                    // Strip trailing newline so lines paste cleanly.
                    text.push_str(line.trim_end_matches('\n'));
                }
            }
        }

        ctx.copy_text(text);
    }

    // -----------------------------------------------------------------------
    // UI sections
    // -----------------------------------------------------------------------

    fn show_search_bar(&mut self, ui: &mut Ui) {
        let history_snapshot: Vec<String> = self.search_history.iter().cloned().collect();
        let mut history_selection: Option<String> = None;

        ui.horizontal(|ui| {
            ui.label("🔍");

            if ui
                .add_enabled(!self.search_running, egui::Button::new("Filter"))
                .clicked()
            {
                self.run_search();
            }

            ui.menu_button("History ▾", |ui| {
                if history_snapshot.is_empty() {
                    ui.label("No search history yet");
                } else {
                    for pattern in &history_snapshot {
                        if ui.button(pattern).clicked() {
                            history_selection = Some(pattern.clone());
                            ui.close_menu();
                        }
                    }
                }
            });

            if ui.button("✕ Clear").clicked() {
                self.clear_search();
            }

            // TextEdit last so it claims all remaining horizontal space.
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.regex_input)
                    .hint_text("Enter regex pattern…")
                    .desired_width(f32::INFINITY),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.run_search();
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

        // Apply history selection after the closures so `self` is free.
        if let Some(pattern) = history_selection {
            self.regex_input = pattern;
            self.run_search();
        }
    }

    fn show_top_pane(&mut self, ui: &mut Ui) {
        let (Some(reader), Some(indexer)) = (&self.file_reader, &self.line_indexer) else {
            ui.centered_and_justified(|ui| {
                ui.label("No file open. Use File → Open… to load a file.");
            });
            return;
        };

        let total_lines = indexer.total_lines();
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let total_height = total_lines as f32 * row_height;

        // both() enables a horizontal scrollbar in addition to the existing
        // vertical one. Content that's wider than the viewport now scrolls
        // sideways instead of wrapping or being clipped.
        let mut scroll_area = ScrollArea::both()
            .id_salt("top_pane")
            .auto_shrink([false; 2]);

        if let Some(target_line) = self.top_pane_scroll_target {
            if self.top_pane_viewport_height.is_finite() {
                let target_top_px = target_line as f32 * row_height;
                let offset = (target_top_px - self.top_pane_viewport_height / 2.0).max(0.0);
                scroll_area = scroll_area.vertical_scroll_offset(offset);
                self.top_pane_scroll_target = None;
            }
        }

        // Clone what we need before the closure so the borrow checker is happy.
        let reader = Arc::clone(reader);

        let output = scroll_area.show(ui, |ui| {
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());

            // ui.cursor().min is where content starts in this ScrollArea —
            // a stable reference point available BEFORE any allocation, which
            // is what we need since we want to measure visible lines before
            // deciding how wide to allocate (for the horizontal scrollbar).
            // This mirrors what allocate_exact_size's returned rect.min would
            // have been, since nothing is drawn before it in this closure.
            let content_origin_y = ui.cursor().min.y;
            let scroll_top = (ui.clip_rect().min.y - content_origin_y).max(0.0);
            let first_visible = (scroll_top / row_height).floor() as usize;
            let visible_row_count = (ui.clip_rect().height() / row_height).ceil() as usize + 1;
            let last_visible =
                (first_visible + visible_row_count).min(total_lines.saturating_sub(1));

            // Pre-fetch visible line text once; reused for both width
            // measurement and painting so we don't hit the reader twice.
            //
            // We only measure the WIDTH of currently-visible lines, not every
            // line in the file — measuring all lines up front would be too
            // slow for multi-gigabyte files. This means the horizontal
            // scrollbar's range "follows" whichever lines are in view: as the
            // user scrolls down to a wider line, the scrollable width grows
            // to include it. The scrollbar can't know about a long line far
            // below the current viewport until it's been scrolled into view
            // at least once.
            let mut visible_lines: Vec<(usize, String)> = Vec::new();
            let mut max_width = ui.available_width();

            if total_lines > 0 && first_visible <= last_visible {
                for line_no in first_visible..=last_visible {
                    let line_text = if let Some((start, end)) =
                        indexer.get_line_with_reader(line_no, &reader)
                    {
                        let raw = reader.get_chunk(start, end);
                        raw.trim_end_matches('\n').to_string()
                    } else {
                        String::new()
                    };
                    let line_label = format!("{:>6}  {}", line_no + 1, line_text);

                    let galley_width = ui
                        .fonts(|f| {
                            f.layout_no_wrap(
                                line_label.clone(),
                                font_id.clone(),
                                ui.visuals().text_color(),
                            )
                        })
                        .size()
                        .x;
                    max_width = max_width.max(galley_width + 8.0); // small right margin

                    visible_lines.push((line_no, line_label));
                }
            }

            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(max_width, total_height),
                egui::Sense::hover(),
            );

            let painter = ui.painter();

            for (line_no, line_label) in &visible_lines {
                let y_top = rect.min.y + *line_no as f32 * row_height;

                painter.text(
                    egui::pos2(rect.min.x + 4.0, y_top),
                    egui::Align2::LEFT_TOP,
                    line_label,
                    font_id.clone(),
                    ui.visuals().text_color(),
                );
            }
        });

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

        let (Some(reader), Some(indexer)) = (&self.file_reader, &self.line_indexer) else {
            return;
        };
        // Clone Arcs so the closure can capture them without borrowing self.
        let reader = Arc::clone(reader);

        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let total_height = match_count as f32 * row_height;

        // Snapshot selection state for rendering (avoids borrow conflict inside
        // the closure with the mutable self fields we update on click).
        let selected_matches_snap = self.selected_matches.clone();
        let match_line_numbers_snap = self.match_line_numbers.clone();

        // Collect click events: (match_idx, modifiers)
        let mut clicked: Option<(usize, egui::Modifiers)> = None;

        // both() adds a horizontal scrollbar alongside the existing vertical
        // one, matching the top pane. show_rows() (used previously) only
        // supports vertical scrolling, so we switch to manual virtualization
        // here too — same pattern as show_top_pane.
        let scroll_area = ScrollArea::both()
            .id_salt("bottom_pane")
            .auto_shrink([false; 2]);

        scroll_area.show(ui, |ui| {
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());

            // Stable reference point available before allocation — see the
            // matching comment in show_top_pane for why this is needed.
            let content_origin_y = ui.cursor().min.y;
            let scroll_top = (ui.clip_rect().min.y - content_origin_y).max(0.0);
            let first_visible = (scroll_top / row_height).floor() as usize;
            let visible_row_count = (ui.clip_rect().height() / row_height).ceil() as usize + 1;
            let last_visible =
                (first_visible + visible_row_count).min(match_count.saturating_sub(1));

            // Only measure/render currently-visible rows — see the matching
            // comment in show_top_pane for why this doesn't scan every match.
            let mut visible_rows: Vec<(usize, usize, String)> = Vec::new(); // (match_idx, line_no, label)
            let mut max_width = ui.available_width();

            if first_visible <= last_visible {
                for match_idx in first_visible..=last_visible {
                    let line_no = match_line_numbers_snap[match_idx];
                    let line_text = if let Some((start, end)) =
                        indexer.get_line_with_reader(line_no, &reader)
                    {
                        let raw = reader.get_chunk(start, end);
                        raw.trim_end_matches('\n').to_string()
                    } else {
                        String::new()
                    };
                    let line_label = format!("{:>6}  {}", line_no + 1, line_text);

                    let galley_width = ui
                        .fonts(|f| {
                            f.layout_no_wrap(
                                line_label.clone(),
                                font_id.clone(),
                                ui.visuals().text_color(),
                            )
                        })
                        .size()
                        .x;
                    max_width = max_width.max(galley_width + 8.0);

                    visible_rows.push((match_idx, line_no, line_label));
                }
            }

            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(max_width, total_height),
                egui::Sense::hover(),
            );

            for (match_idx, _line_no, line_label) in &visible_rows {
                let y_top = rect.min.y + *match_idx as f32 * row_height;
                let row_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.min.x, y_top),
                    egui::vec2(max_width, row_height),
                );

                let is_selected = selected_matches_snap.contains(match_idx);

                // ui.put() centers its widget within the given rect by
                // default, which is why the text appeared centered instead of
                // left-aligned. Building a child Ui with an explicit
                // left-to-right, top-down layout forces the SelectableLabel
                // to hug the left edge instead.
                let mut row_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(row_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::TOP)),
                );
                let response = row_ui.add(egui::SelectableLabel::new(
                    is_selected,
                    egui::RichText::new(line_label).monospace(),
                ));

                if response.clicked() {
                    let modifiers = ui.input(|i| i.modifiers);
                    clicked = Some((*match_idx, modifiers));
                }
            }
        });

        // Apply click outside the closure so &mut self is available.
        if let Some((match_idx, modifiers)) = clicked {
            let line_no = self.match_line_numbers[match_idx];

            if modifiers.shift {
                let anchor = self.selection_anchor.unwrap_or(match_idx);
                let (lo, hi) = if anchor <= match_idx {
                    (anchor, match_idx)
                } else {
                    (match_idx, anchor)
                };
                self.selected_matches.clear();
                self.selected_matches.extend(lo..=hi);
                self.selection_cursor = Some(match_idx);
            } else if modifiers.command {
                if !self.selected_matches.remove(&match_idx) {
                    self.selected_matches.insert(match_idx);
                }
                self.selection_anchor = Some(match_idx);
                self.selection_cursor = Some(match_idx);
            } else {
                self.selected_matches.clear();
                self.selected_matches.insert(match_idx);
                self.selection_anchor = Some(match_idx);
                self.selection_cursor = Some(match_idx);
            }

            self.top_pane_scroll_target = Some(line_no);
        }
    }

    fn handle_arrow_keys_impl(&mut self, ctx: &Context) {
        let no_text_focused = ctx.memory(|m| m.focused().is_none());
        if no_text_focused && !self.match_line_numbers.is_empty() {
            let count = self.match_line_numbers.len();
            let up = ctx.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::ArrowUp));
            let down = ctx.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::ArrowDown));

            if up || down {
                let cursor = self.selection_cursor.unwrap_or_else(|| {
                    if up { count - 1 } else { 0 }
                });
                let anchor = self.selection_anchor.unwrap_or(cursor);
                let new_cursor = if up {
                    cursor.saturating_sub(1)
                } else {
                    (cursor + 1).min(count - 1)
                };

                let (lo, hi) = if anchor <= new_cursor {
                    (anchor, new_cursor)
                } else {
                    (new_cursor, anchor)
                };
                self.selected_matches.clear();
                self.selected_matches.extend(lo..=hi);
                self.selection_cursor = Some(new_cursor);
                if self.selection_anchor.is_none() {
                    self.selection_anchor = Some(anchor);
                }

                let line_no = self.match_line_numbers[new_cursor];
                self.top_pane_scroll_target = Some(line_no);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// egui App trait
// ---------------------------------------------------------------------------

impl eframe::App for FilterApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Apply the default font/size once, on the very first frame. Without
        // this, the first frame renders with egui's untouched built-in style
        // before our DEFAULT_FONT_SIZE override takes effect.
        if !self.font_settings_applied {
            self.apply_font_settings(ctx);
            self.font_settings_applied = true;
        }

        // Same idea for the default theme.
        if !self.theme_applied {
            self.apply_theme(ctx);
            self.theme_applied = true;
        }

        self.poll_search_results();

        if self.search_running {
            ctx.request_repaint();
        }

        // Global Ctrl+C — copy selected result lines to clipboard.
        let no_text_focused = ctx.memory(|m| m.focused().is_none());
        if no_text_focused
            && !self.selected_matches.is_empty()
            && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C))
        {
            self.copy_selected_to_clipboard(ctx);
        }

        // Shift+Up / Shift+Down extend the selection in the bottom pane.
        self.handle_arrow_keys_impl(ctx);

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

                ui.menu_button("Edit", |ui| {
                    let has_selection = !self.selected_matches.is_empty();
                    if ui
                        .add_enabled(
                            has_selection,
                            egui::Button::new("Copy Selected Lines    Ctrl+C"),
                        )
                        .clicked()
                    {
                        self.copy_selected_to_clipboard(ctx);
                        ui.close_menu();
                    }

                    ui.separator();

                    if ui.button("Font Settings…").clicked() {
                        self.show_font_settings = true;
                        ui.close_menu();
                    }

                    ui.menu_button("Theme", |ui| {
                        // selectable_label as a radio-style entry: highlighted
                        // when it matches the current theme, clicking applies
                        // it immediately and closes the submenu.
                        if ui
                            .selectable_label(self.theme == egui::Theme::Light, "Light")
                            .clicked()
                        {
                            self.theme = egui::Theme::Light;
                            self.apply_theme(ctx);
                            ui.close_menu();
                        }
                        if ui
                            .selectable_label(self.theme == egui::Theme::Dark, "Dark")
                            .clicked()
                        {
                            self.theme = egui::Theme::Dark;
                            self.apply_theme(ctx);
                            ui.close_menu();
                        }
                    });
                });
            });
        });

        self.show_font_settings_window(ctx);

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

        // Search bar — bottom-anchored, sits just above the bottom pane.
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