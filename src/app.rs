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
        }

        // Clone what we need before the closure so the borrow checker is happy.
        let reader = Arc::clone(reader);

        let output = scroll_area.show(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), total_height),
                egui::Sense::hover(),
            );

            let scroll_top = (ui.clip_rect().min.y - rect.min.y).max(0.0);
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

                // Fetch line text via indexer + reader.
                // get_line_with_reader returns (start_byte, end_byte).
                let line_text = if let Some((start, end)) =
                    indexer.get_line_with_reader(line_no, &reader)
                {
                    let raw = reader.get_chunk(start, end);
                    raw.trim_end_matches('\n').to_string()
                } else {
                    String::new()
                };

                let line_label = format!("{:>6}  {}", line_no + 1, line_text);

                painter.text(
                    egui::pos2(row_rect.min.x + 4.0, y_top),
                    egui::Align2::LEFT_TOP,
                    line_label,
                    egui::TextStyle::Monospace.resolve(ui.style()),
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

        // Snapshot selection state for rendering (avoids borrow conflict inside
        // the closure with the mutable self fields we update on click).
        let selected_matches_snap = self.selected_matches.clone();
        let match_line_numbers_snap = self.match_line_numbers.clone();

        // Collect click events: (match_idx, modifiers)
        let mut clicked: Option<(usize, egui::Modifiers)> = None;

        ScrollArea::vertical()
            .id_salt("bottom_pane")
            .auto_shrink([false; 2])
            .show_rows(ui, row_height, match_count, |ui, visible_range| {
                for match_idx in visible_range {
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
                    let is_selected = selected_matches_snap.contains(&match_idx);

                    let response = ui.add(egui::SelectableLabel::new(
                        is_selected,
                        egui::RichText::new(line_label).monospace(),
                    ));

                    if response.clicked() {
                        let modifiers = ui.input(|i| i.modifiers);
                        clicked = Some((match_idx, modifiers));
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
