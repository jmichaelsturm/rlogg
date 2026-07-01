// app.rs — Main application state and egui rendering logic.
//
// Both panes use the same widget-based rendering pipeline via render_pane().
// The only differences between them are:
//   - Which rows they show (all file lines vs. match_line_numbers)
//   - Which selection state they read/write (top_selected vs. bot_selected)

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
};

use egui::{Context, ScrollArea, Ui};

use crate::{line_index::FullLineIndex, AVAILABLE_FONTS, FONT_EGUI_DEFAULT};

use large_text_core::{
    file_reader::{detect_encoding, FileReader},
    search_engine::{SearchEngine, SearchMessage, SearchType},
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_HISTORY: usize = 20;
const MAX_SEARCH_RESULTS: usize = 1_000_000;
const DEFAULT_FONT_SIZE: f32 = 14.0;
const MIN_FONT_SIZE: f32 = 8.0;
const MAX_FONT_SIZE: f32 = 32.0;

// ---------------------------------------------------------------------------
// Display-row type
// ---------------------------------------------------------------------------

/// One row of vertical space in a pane. A noted line occupies two rows:
/// a Note row (the annotation text) directly above a Content row (the file line).
#[derive(Clone, Copy)]
enum DisplayRow {
    Note    { line_no: usize },
    Content { line_no: usize },
}

impl DisplayRow {
    fn line_no(self) -> usize {
        match self {
            DisplayRow::Note    { line_no } => line_no,
            DisplayRow::Content { line_no } => line_no,
        }
    }
}

/// Which pane most recently received a click — determines where Ctrl+C,
/// shift+arrow, and double-click events apply.
#[derive(Clone, Copy, PartialEq)]
enum ActivePane { Top, Bottom }

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct FilterApp {
    // --- File ----------------------------------------------------------------
    file_reader:  Option<Arc<FileReader>>,
    line_indexer: Option<FullLineIndex>,
    file_path:    Option<PathBuf>,

    // --- Search --------------------------------------------------------------
    regex_input:    String,
    regex_error:    Option<String>,
    search_engine:  SearchEngine,
    search_rx:      Option<mpsc::Receiver<SearchMessage>>,
    cancel_token:   Arc<AtomicBool>,
    search_running: bool,

    // --- History -------------------------------------------------------------
    search_history: VecDeque<String>,

    // --- Results -------------------------------------------------------------
    /// Matched line numbers, always in ascending file order.
    match_line_numbers: Vec<usize>,

    // --- Selection -----------------------------------------------------------
    // Both panes use the same interaction model: shift-click range,
    // ctrl-click toggle, shift+arrow extension. Selection is keyed by LINE
    // NUMBER in both panes — a stable identifier that doesn't shift when the
    // result list is re-sorted or inserted into.
    active_pane: ActivePane,

    top_selected: BTreeSet<usize>, // line numbers selected in the top pane
    top_anchor:   Option<usize>,   // shift-click anchor (line number)
    top_cursor:   Option<usize>,   // shift-arrow cursor (line number)

    bot_selected: BTreeSet<usize>, // line numbers selected in the bottom pane
    bot_anchor:   Option<usize>,
    bot_cursor:   Option<usize>,

    // --- Scroll state --------------------------------------------------------
    /// When Some, the top pane scrolls to center this line number next frame.
    top_scroll_target:   Option<usize>,
    /// Measured viewport height of the top pane (NAN until first paint).
    top_viewport_height: f32,
    /// Last observed scroll-top pixel position of the top pane, reported
    /// back by render_pane each frame. Used to decide which window of rows
    /// to build on the NEXT frame so the windowed top pane keeps following
    /// the user's manual scrolling (mouse wheel / scrollbar drag), not just
    /// programmatic scroll-to-line-N requests from clicks.
    top_scroll_top: f32,

    // --- Font settings -------------------------------------------------------
    selected_font:         String,
    font_size:             f32,
    show_font_settings:    bool,
    font_settings_applied: bool,

    // --- Theme ---------------------------------------------------------------
    theme:         egui::Theme,
    theme_applied: bool,

    // --- Notes ---------------------------------------------------------------
    notes:          BTreeMap<usize, String>, // line_no → note text
    note_popup_line: Option<usize>,
    note_popup_text: String,
    show_all_notes:  bool,
    /// Line numbers whose note text (not file-line text) matched the regex.
    note_matched_lines: BTreeSet<usize>,
}

impl Default for FilterApp {
    fn default() -> Self {
        Self {
            file_reader:    None,
            line_indexer:   None,
            file_path:      None,
            regex_input:    String::new(),
            regex_error:    None,
            search_engine:  SearchEngine::new(),
            search_rx:      None,
            cancel_token:   Arc::new(AtomicBool::new(false)),
            search_running: false,
            search_history: VecDeque::new(),
            match_line_numbers: Vec::new(),
            active_pane:    ActivePane::Bottom,
            top_selected:   BTreeSet::new(),
            top_anchor:     None,
            top_cursor:     None,
            bot_selected:   BTreeSet::new(),
            bot_anchor:     None,
            bot_cursor:     None,
            top_scroll_target:   None,
            top_viewport_height: f32::NAN,
            top_scroll_top: 0.0,
            selected_font:         FONT_EGUI_DEFAULT.to_owned(),
            font_size:             DEFAULT_FONT_SIZE,
            show_font_settings:    false,
            font_settings_applied: false,
            theme:         egui::Theme::Light,
            theme_applied: false,
            notes:          BTreeMap::new(),
            note_popup_line: None,
            note_popup_text: String::new(),
            show_all_notes:  false,
            note_matched_lines: BTreeSet::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Module-level helpers (usable inside closures without &self borrow)
// ---------------------------------------------------------------------------

/// Number of note rows inserted before `line_no` in the top pane's display.
/// Each noted line strictly before `line_no` adds one extra display row.
fn notes_before(notes: &BTreeMap<usize, String>, line_no: usize) -> usize {
    notes.range(..line_no).count()
}

/// Label for a Content row.
fn content_label(line_no: usize, reader: &FileReader, indexer: &FullLineIndex) -> String {
    let text = indexer
        .get_line_with_reader(line_no, reader)
        .map(|(s, e)| reader.get_chunk(s, e).trim_end_matches('\n').to_owned())
        .unwrap_or_default();
    format!("{:>6}  {}", line_no + 1, text)
}

/// Label for a Note row. The ==== prefix visually distinguishes note rows
/// from file-content rows without relying on emoji or icons.
fn note_label(text: &str, _note_matched: bool) -> String {
    format!("        ==== {}", text)
}

/// Apply a click with modifiers to a selection set. Pure function — no
/// side effects, returns the new (selected, anchor, cursor).
fn apply_click(
    line_no: usize,
    mods: egui::Modifiers,
    current: &BTreeSet<usize>,
    anchor: Option<usize>,
) -> (BTreeSet<usize>, Option<usize>, Option<usize>) {
    if mods.shift {
        let a = anchor.unwrap_or(line_no);
        let (lo, hi) = if a <= line_no { (a, line_no) } else { (line_no, a) };
        let mut s = BTreeSet::new();
        s.extend(lo..=hi);
        (s, Some(a), Some(line_no))
    } else if mods.command {
        let mut s = current.clone();
        if !s.remove(&line_no) { s.insert(line_no); }
        (s, Some(line_no), Some(line_no))
    } else {
        let mut s = BTreeSet::new();
        s.insert(line_no);
        (s, Some(line_no), Some(line_no))
    }
}

// ---------------------------------------------------------------------------
// impl FilterApp
// ---------------------------------------------------------------------------

impl FilterApp {

    // -----------------------------------------------------------------------
    // File
    // -----------------------------------------------------------------------

    fn open_file(&mut self, path: PathBuf) {
        let encoding = std::fs::read(&path)
            .map(|b| detect_encoding(&b[..b.len().min(4096)]))
            .unwrap_or(encoding_rs::UTF_8);

        match FileReader::new(path.clone(), encoding) {
            Ok(reader) => {
                let reader = Arc::new(reader);
                let indexer = FullLineIndex::build(&reader);
                self.file_reader  = Some(reader);
                self.line_indexer = Some(indexer);
                self.file_path    = Some(path);
                self.notes.clear();
                self.note_popup_line = None;
                self.note_popup_text.clear();
                self.show_all_notes = false;
                self.clear_search();
            }
            Err(e) => eprintln!("Failed to open file: {e}"),
        }
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    fn clear_search(&mut self) {
        self.cancel_token.store(true, Ordering::Relaxed);
        self.cancel_token = Arc::new(AtomicBool::new(false));
        self.search_engine.clear();
        self.search_rx      = None;
        self.search_running = false;
        self.regex_error    = None;
        self.match_line_numbers.clear();
        self.note_matched_lines.clear();
        self.bot_selected.clear();
        self.bot_anchor = None;
        self.bot_cursor = None;
        self.top_scroll_target = None;
        // Deliberately NOT clearing notes, top_selected, show_all_notes —
        // those survive a search clear (✕ Clear button). Only open_file
        // resets notes.
    }

    fn add_to_history(&mut self, pattern: String) {
        if pattern.is_empty() { return; }
        self.search_history.retain(|p| p != &pattern);
        self.search_history.push_front(pattern);
        if self.search_history.len() > MAX_HISTORY {
            self.search_history.pop_back();
        }
    }

    fn run_search(&mut self) {
        let Some(reader) = self.file_reader.clone() else { return };

        self.cancel_token.store(true, Ordering::Relaxed);
        self.cancel_token = Arc::new(AtomicBool::new(false));
        self.match_line_numbers.clear();
        self.note_matched_lines.clear();
        self.bot_selected.clear();
        self.bot_anchor = None;
        self.bot_cursor = None;

        // Empty regex + "show notes": just list noted lines, skip file search.
        if self.regex_input.is_empty() {
            self.regex_error    = None;
            self.search_running = false;
            if self.show_all_notes {
                self.match_line_numbers = self.notes.keys().copied().collect();
            }
            return;
        }

        let compiled_regex = match regex::Regex::new(&self.regex_input) {
            Ok(re) => re,
            Err(e) => { self.regex_error = Some(format!("Invalid regex: {e}")); return; }
        };
        self.regex_error = None;
        self.add_to_history(self.regex_input.clone());

        // Match against note text first — SearchEngine only searches mmap bytes.
        for (&line_no, note_text) in &self.notes {
            if compiled_regex.is_match(note_text) {
                self.note_matched_lines.insert(line_no);
                self.match_line_numbers.push(line_no);
            }
        }

        // Union all noted lines if "show notes" checkbox is checked.
        if self.show_all_notes {
            for &line_no in self.notes.keys() {
                if !self.match_line_numbers.contains(&line_no) {
                    self.match_line_numbers.push(line_no);
                    self.note_matched_lines.insert(line_no);
                }
            }
        }

        // Sort so binary-search insertions from streaming file results
        // maintain ascending order throughout.
        self.match_line_numbers.sort_unstable();
        self.match_line_numbers.dedup();

        self.search_engine.set_query(self.regex_input.clone(), true, false);
        self.search_running = true;

        let (tx, rx) = mpsc::sync_channel(256);
        self.search_rx = Some(rx);
        self.search_engine.fetch_matches(
            reader, tx, 0, MAX_SEARCH_RESULTS, Arc::clone(&self.cancel_token),
        );
    }

    fn poll_search_results(&mut self) {
        let Some(rx)      = &self.search_rx    else { return };
        let Some(indexer) = &self.line_indexer else { return };
        let Some(reader)  = &self.file_reader  else { return };

        let mut seen: std::collections::HashSet<usize> =
            self.match_line_numbers.iter().copied().collect();

        for _ in 0..10_000 {
            match rx.try_recv() {
                Ok(SearchMessage::ChunkResult(chunk)) => {
                    for result in chunk.matches {
                        let ln = Self::resolve_line_for_offset(
                            indexer, reader, result.byte_offset,
                        );
                        if seen.insert(ln) {
                            let pos = self.match_line_numbers
                                .binary_search(&ln).unwrap_or_else(|i| i);
                            self.match_line_numbers.insert(pos, ln);
                        }
                    }
                }
                Ok(SearchMessage::Done(SearchType::Fetch)) => {
                    self.search_running = false;
                    self.search_rx = None;
                    break;
                }
                Ok(SearchMessage::Error(e)) => {
                    self.regex_error    = Some(e);
                    self.search_running = false;
                    self.search_rx      = None;
                    break;
                }
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty)        => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.search_running = false;
                    self.search_rx      = None;
                    break;
                }
            }
        }
    }

    fn resolve_line_for_offset(
        indexer: &FullLineIndex,
        reader: &FileReader,
        offset: usize,
    ) -> usize {
        let mut ln = indexer.find_line_at_offset(offset);
        let Some((mut start, mut end)) = indexer.get_line_with_reader(ln, reader) else {
            return ln;
        };
        const MAX: usize = 64;
        let mut steps = 0;
        while offset >= end && steps < MAX {
            ln += 1;
            match indexer.get_line_with_reader(ln, reader) {
                Some((s, e)) => { start = s; end = e; }
                None => break,
            }
            steps += 1;
        }
        steps = 0;
        while offset < start && ln > 0 && steps < MAX {
            ln -= 1;
            if let Some((s, _)) = indexer.get_line_with_reader(ln, reader) {
                start = s;
            }
            steps += 1;
        }
        ln
    }

    // -----------------------------------------------------------------------
    // Keyboard shortcuts (shift+arrow selection extension)
    // -----------------------------------------------------------------------

    fn handle_arrow_keys(&mut self, ctx: &Context) {
        if !ctx.memory(|m| m.focused().is_none()) { return; }
        let up   = ctx.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::ArrowUp));
        let down = ctx.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::ArrowDown));
        if !up && !down { return; }

        match self.active_pane {
            ActivePane::Bottom => {
                let lines = self.match_line_numbers.clone();
                if lines.is_empty() { return; }
                let count = lines.len();
                let cursor_ln = self.bot_cursor
                    .unwrap_or_else(|| if up { *lines.last().unwrap() } else { lines[0] });
                let anchor_ln = self.bot_anchor.unwrap_or(cursor_ln);
                let cursor_idx = lines.partition_point(|&n| n < cursor_ln);
                let new_idx = if up {
                    cursor_idx.saturating_sub(1)
                } else {
                    (cursor_idx + 1).min(count - 1)
                };
                let new_ln  = lines[new_idx];
                let anc_idx = lines.partition_point(|&n| n < anchor_ln);
                let (lo, hi) = if anc_idx <= new_idx { (anc_idx, new_idx) } else { (new_idx, anc_idx) };
                self.bot_selected.clear();
                for &ln in &lines[lo..=hi] { self.bot_selected.insert(ln); }
                self.bot_cursor = Some(new_ln);
                if self.bot_anchor.is_none() { self.bot_anchor = Some(anchor_ln); }
                self.top_scroll_target = Some(new_ln);
            }
            ActivePane::Top => {
                let Some(indexer) = &self.line_indexer else { return };
                let total = indexer.total_lines();
                if total == 0 { return; }
                let cursor_ln = self.top_cursor
                    .unwrap_or(if up { total - 1 } else { 0 });
                let anchor_ln = self.top_anchor.unwrap_or(cursor_ln);
                let new_ln = if up {
                    cursor_ln.saturating_sub(1)
                } else {
                    (cursor_ln + 1).min(total - 1)
                };
                let (lo, hi) = if anchor_ln <= new_ln {
                    (anchor_ln, new_ln)
                } else {
                    (new_ln, anchor_ln)
                };
                self.top_selected.clear();
                self.top_selected.extend(lo..=hi);
                self.top_cursor = Some(new_ln);
                if self.top_anchor.is_none() { self.top_anchor = Some(anchor_ln); }
                self.top_scroll_target = Some(new_ln);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Clipboard
    // -----------------------------------------------------------------------

    fn copy_selected_to_clipboard(&self, ctx: &Context) {
        let (Some(reader), Some(indexer)) = (&self.file_reader, &self.line_indexer) else {
            return;
        };
        let lines: Vec<usize> = match self.active_pane {
            ActivePane::Top    => self.top_selected.iter().copied().collect(),
            ActivePane::Bottom => self.bot_selected.iter().copied().collect(),
        };
        if lines.is_empty() { return; }

        let text = lines.iter().enumerate()
            .map(|(i, &ln)| {
                let prefix = if i > 0 { "\n" } else { "" };
                let content = indexer
                    .get_line_with_reader(ln, reader)
                    .map(|(s, e)| reader.get_chunk(s, e).trim_end_matches('\n').to_owned())
                    .unwrap_or_default();
                // Append the note on its own line above the content,
                // matching the visual layout in the panes.
                if let Some(note) = self.notes.get(&ln) {
                    format!("{}==== {}\n{}", prefix, note, content)
                } else {
                    format!("{}{}", prefix, content)
                }
            })
            .collect::<String>();

        ctx.copy_text(text);
    }


    // -----------------------------------------------------------------------
    // Theme / Font
    // -----------------------------------------------------------------------

    fn apply_theme(&self, ctx: &Context) {
        ctx.set_visuals(match self.theme {
            egui::Theme::Dark  => egui::Visuals::dark(),
            egui::Theme::Light => egui::Visuals::light(),
        });
    }

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

    fn show_font_settings_window(&mut self, ctx: &Context) {
        if !self.show_font_settings { return; }
        let mut changed = false;
        let mut open    = true;
        egui::Window::new("Font Settings")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Font:");
                    egui::ComboBox::from_id_salt("font_family_combo")
                        .selected_text(&self.selected_font)
                        .show_ui(ui, |ui| {
                            for &name in AVAILABLE_FONTS {
                                let sel = self.selected_font == name;
                                if ui.selectable_label(sel, name).clicked() && !sel {
                                    self.selected_font = name.to_owned();
                                    changed = true;
                                }
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Size:");
                    if ui.add(egui::DragValue::new(&mut self.font_size)
                        .range(MIN_FONT_SIZE..=MAX_FONT_SIZE)
                        .suffix(" pt")).changed()
                    {
                        changed = true;
                    }
                });
                ui.separator();
                let family = if self.selected_font == FONT_EGUI_DEFAULT {
                    egui::FontFamily::Monospace
                } else {
                    egui::FontFamily::Name(self.selected_font.clone().into())
                };
                ui.label(egui::RichText::new("The quick brown fox jumps 0123456789")
                    .font(egui::FontId::new(self.font_size, family)));
            });
        if !open { self.show_font_settings = false; }
        if changed { self.apply_font_settings(ctx); }
    }

    // -----------------------------------------------------------------------
    // Notes popup
    // -----------------------------------------------------------------------

    fn open_note_popup(&mut self, line_no: usize) {
        self.note_popup_text = self.notes.get(&line_no).cloned().unwrap_or_default();
        self.note_popup_line = Some(line_no);
    }

    fn show_note_popup_window(&mut self, ctx: &Context) {
        let Some(line_no) = self.note_popup_line else { return };
        let mut open   = true;
        let mut save   = false;
        let mut cancel = false;

        egui::Window::new(format!("Note \u{2014} line {}", line_no + 1))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.note_popup_text)
                        .hint_text("Enter a note for this line\u{2026}")
                        .desired_width(320.0),
                );
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked()   { save   = true; }
                    if ui.button("Cancel").clicked() { cancel = true; }
                    if self.notes.contains_key(&line_no) && ui.button("Delete").clicked() {
                        self.notes.remove(&line_no);
                        cancel = true;
                    }
                });
            });

        if save {
            let t = self.note_popup_text.trim().to_owned();
            if t.is_empty() {
                self.notes.remove(&line_no);
            } else {
                self.notes.insert(line_no, t);
            }
            self.note_popup_line = None;
            self.note_popup_text.clear();
            self.run_search();
        } else if cancel || !open {
            self.note_popup_line = None;
            self.note_popup_text.clear();
        }
    }

    // -----------------------------------------------------------------------
    // Search bar
    // -----------------------------------------------------------------------

    fn show_search_bar(&mut self, ui: &mut Ui) {
        let history_snap: Vec<String> = self.search_history.iter().cloned().collect();
        let mut history_sel: Option<String> = None;

        ui.horizontal(|ui| {
            ui.label("\u{1F50D}");
            if ui.add_enabled(!self.search_running, egui::Button::new("Filter")).clicked() {
                self.run_search();
            }
            ui.menu_button("History \u{25BE}", |ui| {
                if history_snap.is_empty() {
                    ui.label("No search history yet");
                } else {
                    for p in &history_snap {
                        if ui.button(p).clicked() {
                            history_sel = Some(p.clone());
                            ui.close_menu();
                        }
                    }
                }
            });
            if ui.button("\u{2715} Clear").clicked() { self.clear_search(); }
            if ui.checkbox(&mut self.show_all_notes, "Show notes").changed() {
                self.run_search();
            }
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.regex_input)
                    .hint_text("Enter regex pattern\u{2026}")
                    .desired_width(f32::INFINITY),
            );
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.run_search();
            }
        });

        ui.horizontal(|ui| {
            if self.search_running {
                ui.spinner();
                ui.label(format!("Searching\u{2026} {} matches so far", self.match_line_numbers.len()));
            } else if let Some(e) = &self.regex_error {
                ui.colored_label(egui::Color32::RED, e);
            } else if !self.match_line_numbers.is_empty() {
                ui.label(format!("{} matches found", self.match_line_numbers.len()));
            } else if self.regex_input.is_empty() {
                ui.label("Type a regex above and press Enter or click Filter.");
            } else {
                ui.label("No matches.");
            }
        });

        if let Some(p) = history_sel {
            self.regex_input = p;
            self.run_search();
        }
    }

    // -----------------------------------------------------------------------
    // Shared pane renderer
    // -----------------------------------------------------------------------
    //
    // Both panes call this function. It handles:
    //   - Virtual scrolling (only visible rows are painted)
    //   - Note rows above their content rows
    //   - SelectableLabel widgets (uniform between panes)
    //   - Width measurement for horizontal scrollbar
    //   - Click event collection (returned, applied by caller)
    //   - Double-click -> note popup (returned, applied by caller)
    //
    // Arguments:
    //   rows          Pre-built (DisplayRow, label) pairs for a window of rows.
    //   total_rows    Total logical row count for the pane (for scrollbar sizing).
    //   row_offset    Global display-row index of rows[0].
    //   total_height  Full virtual content height in pixels.
    //   pane_id       Unique ScrollArea salt.
    //   selected      Which line numbers are currently selected.
    //   scroll_offset If Some, force vertical scroll to this pixel value.
    //
    // Returns: (clicked, double_clicked, viewport_height, observed_scroll_top_px)
    fn render_pane(
        ui: &mut Ui,
        rows: &[(DisplayRow, String)],
        total_rows: usize,
        row_offset: usize,
        total_height: f32,
        pane_id: &str,
        selected: &BTreeSet<usize>,
        scroll_offset: Option<f32>,
    ) -> (Option<(usize, egui::Modifiers)>, Option<usize>, f32, f32) {
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);

        let mut scroll_area = ScrollArea::both()
            .id_salt(pane_id)
            .auto_shrink([false; 2]);
        if let Some(offset) = scroll_offset {
            scroll_area = scroll_area.vertical_scroll_offset(offset);
        }

        let mut clicked: Option<(usize, egui::Modifiers)> = None;
        let mut double_clicked: Option<usize> = None;
        let mut observed_scroll_top: f32 = 0.0;

        let output = scroll_area.show(ui, |ui| {
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());

            // Compute visible row range in global display-row index space.
            // This reflects WHEREVER the user has actually scrolled to —
            // whether via mouse wheel, scrollbar drag, or our own
            // vertical_scroll_offset() call above — since clip_rect()
            // always reflects egui's current real scroll state for this
            // frame, not just the offset we requested.
            let origin_y   = ui.cursor().min.y;
            let scroll_top = (ui.clip_rect().min.y - origin_y).max(0.0);
            observed_scroll_top = scroll_top;

            let first_global = (scroll_top / row_height).floor() as usize;
            let vis_count    = (ui.clip_rect().height() / row_height).ceil() as usize + 1;
            let last_global  = (first_global + vis_count).min(total_rows.saturating_sub(1));

            // The caller built `rows` as a WINDOW starting at row_offset.
            // If the actual visible range falls entirely outside that
            // window — in EITHER direction — we have nothing to paint this
            // frame (the window will be rebuilt around the new position on
            // the next frame, once this return value is read by the
            // caller). Without this two-sided check, scrolling past the
            // built window silently fell through to compute nonsensical
            // local indices instead of just skipping rendering cleanly.
            let window_end = row_offset + rows.len();
            let outside_window = rows.is_empty()
                || last_global < row_offset
                || first_global >= window_end;

            if outside_window {
                ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), total_height),
                    egui::Sense::hover(),
                );
                return;
            }
            let first_local = first_global.saturating_sub(row_offset);
            let last_local  = (last_global - row_offset).min(rows.len().saturating_sub(1));

            // Measure widths of visible rows for the horizontal scrollbar.
            let mut max_width = ui.available_width();
            for i in first_local..=last_local {
                if i >= rows.len() { break; }
                let (_, label) = &rows[i];
                if label.is_empty() { continue; }
                let w = ui.fonts(|f| {
                    f.layout_no_wrap(
                        label.clone(),
                        font_id.clone(),
                        ui.visuals().text_color(),
                    )
                }).size().x;
                max_width = max_width.max(w + 8.0);
            }

            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(max_width, total_height),
                egui::Sense::hover(),
            );

            // Render visible rows.
            for i in first_local..=last_local {
                if i >= rows.len() { break; }
                let (display_row, label) = &rows[i];
                if label.is_empty() { continue; }

                let global_idx = row_offset + i;
                let y_top = rect.min.y + global_idx as f32 * row_height;
                let row_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.min.x, y_top),
                    egui::vec2(max_width, row_height),
                );

                let line_no     = display_row.line_no();
                let is_selected = selected.contains(&line_no);

                // SelectableLabel in a left-aligned child Ui so it doesn't
                // center itself inside the allocated row rect.
                let mut row_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(row_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::TOP)),
                );
                let resp = row_ui.add(egui::SelectableLabel::new(
                    is_selected,
                    egui::RichText::new(label).monospace(),
                ));

                if resp.clicked() {
                    clicked = Some((line_no, ui.input(|i| i.modifiers)));
                }
                if resp.double_clicked() {
                    // Double-click on either Note or Content row opens the
                    // popup — if you see a note row, clicking it to edit is
                    // natural.
                    double_clicked = Some(line_no);
                }
            }
        });

        (clicked, double_clicked, output.inner_rect.height(), observed_scroll_top)
    }

    // -----------------------------------------------------------------------
    // Top pane
    // -----------------------------------------------------------------------

    fn show_top_pane(&mut self, ui: &mut Ui) {
        let (Some(reader), Some(indexer)) = (&self.file_reader, &self.line_indexer) else {
            ui.centered_and_justified(|ui| {
                ui.label("No file open. Use File \u{2192} Open\u{2026} to load a file.");
            });
            return;
        };

        let total_lines        = indexer.total_lines();
        let row_height         = ui.text_style_height(&egui::TextStyle::Monospace);
        let total_display_rows = total_lines + self.notes.len();
        let total_height       = total_display_rows as f32 * row_height;

        // Convert pending scroll target (line number) to a pixel offset.
        let scroll_offset = self.top_scroll_target.take().and_then(|target_ln| {
            if self.top_viewport_height.is_finite() {
                let target_row = target_ln + notes_before(&self.notes, target_ln);
                let px = target_row as f32 * row_height;
                Some((px - self.top_viewport_height / 2.0).max(0.0))
            } else {
                // Viewport not yet measured — retry next frame.
                self.top_scroll_target = Some(target_ln);
                None
            }
        });

        let reader_arc   = Arc::clone(reader);
        let notes_snap   = self.notes.clone();
        let note_matched = self.note_matched_lines.clone();

        // Estimate which pixel row is at the top of the viewport, to decide
        // which window of lines to build this frame.
        //
        // Priority: a fresh scroll_offset (from a bottom-pane click or
        // shift+arrow) always wins, since it reflects where we're ABOUT to
        // scroll to. Otherwise, fall back to top_scroll_top — the REAL
        // scroll position render_pane observed and reported back last
        // frame. Without this fallback, every frame without an active
        // click defaulted to "estimate row 0", which is the bug: once the
        // user manually scrolled past the first window (mouse wheel or
        // scrollbar drag), the next frame rebuilt a window starting back at
        // line 0 again, leaving nothing built for wherever they'd actually
        // scrolled to.
        let est_top_px = scroll_offset.unwrap_or(self.top_scroll_top);
        let est_first_row = (est_top_px / row_height).floor() as usize;

        // Build a window generously padded on both sides of the estimate.
        // The padding absorbs two sources of slack: (a) notes_before()
        // estimation error when converting a row estimate back to a
        // starting LINE number, and (b) ordinary scroll movement between
        // this frame and the next (the window must still cover the
        // viewport after a typical wheel-scroll before we rebuild it).
        const BUFFER: usize = 30;
        let start_line = est_first_row
            .saturating_sub(notes_snap.len() + BUFFER)
            .min(total_lines.saturating_sub(1));

        let row_offset = start_line + notes_before(&notes_snap, start_line);

        let vis_rows  = if self.top_viewport_height.is_finite() {
            (self.top_viewport_height / row_height).ceil() as usize + 1
        } else {
            // First-ever frame: viewport height isn't known yet. Build a
            // reasonably large default window so SOMETHING renders before
            // we have real measurements to refine it next frame.
            80
        };
        let window_sz = vis_rows + notes_snap.len() + BUFFER * 2;

        let mut rows: Vec<(DisplayRow, String)> = Vec::with_capacity(window_sz);
        let mut line_cursor = start_line;
        while rows.len() < window_sz && line_cursor < total_lines {
            if let Some(note_text) = notes_snap.get(&line_cursor) {
                rows.push((
                    DisplayRow::Note { line_no: line_cursor },
                    note_label(note_text, note_matched.contains(&line_cursor)),
                ));
            }
            rows.push((
                DisplayRow::Content { line_no: line_cursor },
                content_label(line_cursor, &reader_arc, indexer),
            ));
            line_cursor += 1;
        }

        let top_selected = self.top_selected.clone();

        let (clicked, double_clicked, viewport_h, scroll_top) = Self::render_pane(
            ui,
            &rows,
            total_display_rows,
            row_offset,
            total_height,
            "top_pane",
            &top_selected,
            scroll_offset,
        );

        self.top_viewport_height = viewport_h;
        // Persist the real observed scroll position for next frame's window
        // estimate — this is the fix: without storing this, the top pane's
        // window forgot where the user had manually scrolled to.
        self.top_scroll_top = scroll_top;

        if let Some(ln) = double_clicked {
            self.open_note_popup(ln);
        }
        if let Some((ln, mods)) = clicked {
            self.active_pane = ActivePane::Top;
            let (sel, anc, cur) = apply_click(ln, mods, &self.top_selected, self.top_anchor);
            self.top_selected = sel;
            self.top_anchor   = anc;
            self.top_cursor   = cur;
        }
    }

    // -----------------------------------------------------------------------
    // Bottom pane
    // -----------------------------------------------------------------------

    fn show_bottom_pane(&mut self, ui: &mut Ui) {
        if self.match_line_numbers.is_empty() {
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
        let reader_arc   = Arc::clone(reader);
        let notes_snap   = self.notes.clone();
        let note_matched = self.note_matched_lines.clone();

        // Expand the full result list into display rows. Bounded by
        // MAX_SEARCH_RESULTS — far smaller than total file lines, so
        // expanding all upfront avoids the windowed-estimation complexity.
        let mut rows: Vec<(DisplayRow, String)> = Vec::new();
        for &ln in &self.match_line_numbers {
            if let Some(note_text) = notes_snap.get(&ln) {
                rows.push((
                    DisplayRow::Note { line_no: ln },
                    note_label(note_text, note_matched.contains(&ln)),
                ));
            }
            rows.push((
                DisplayRow::Content { line_no: ln },
                content_label(ln, &reader_arc, indexer),
            ));
        }

        let total_rows   = rows.len();
        let row_height   = ui.text_style_height(&egui::TextStyle::Monospace);
        let total_height = total_rows as f32 * row_height;
        let bot_selected = self.bot_selected.clone();

        let (clicked, double_clicked, _, _) = Self::render_pane(
            ui,
            &rows,
            total_rows,
            0,             // row_offset = 0: rows[0] is global row 0
            total_height,
            "bottom_pane",
            &bot_selected,
            None,          // bottom pane has no programmatic scroll target
        );

        if let Some(ln) = double_clicked {
            self.open_note_popup(ln);
        }
        if let Some((ln, mods)) = clicked {
            self.active_pane = ActivePane::Bottom;
            let (sel, anc, cur) = apply_click(ln, mods, &self.bot_selected, self.bot_anchor);
            self.bot_selected = sel;
            self.bot_anchor   = anc;
            self.bot_cursor   = cur;
            // Clicking a bottom-pane result scrolls the top pane to center
            // that line — the core cross-pane navigation feature.
            self.top_scroll_target = Some(ln);
        }
    }
}

// ---------------------------------------------------------------------------
// egui App trait
// ---------------------------------------------------------------------------

impl eframe::App for FilterApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if !self.font_settings_applied {
            self.apply_font_settings(ctx);
            self.font_settings_applied = true;
        }
        if !self.theme_applied {
            self.apply_theme(ctx);
            self.theme_applied = true;
        }

        self.poll_search_results();
        if self.search_running { ctx.request_repaint(); }

        // Global Ctrl+C — copies from whichever pane is active.
        let no_text_focused = ctx.memory(|m| m.focused().is_none());
        let has_selection = match self.active_pane {
            ActivePane::Top    => !self.top_selected.is_empty(),
            ActivePane::Bottom => !self.bot_selected.is_empty(),
        };
        if no_text_focused && has_selection
            && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C))
        {
            self.copy_selected_to_clipboard(ctx);
        }

        self.handle_arrow_keys(ctx);

        // Menu bar
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open\u{2026}").clicked() {
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
                    let has_sel = match self.active_pane {
                        ActivePane::Top    => !self.top_selected.is_empty(),
                        ActivePane::Bottom => !self.bot_selected.is_empty(),
                    };
                    if ui.add_enabled(
                        has_sel,
                        egui::Button::new("Copy Selected Lines    Ctrl+C"),
                    ).clicked() {
                        self.copy_selected_to_clipboard(ctx);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Font Settings\u{2026}").clicked() {
                        self.show_font_settings = true;
                        ui.close_menu();
                    }
                    ui.menu_button("Theme", |ui| {
                        if ui.selectable_label(
                            self.theme == egui::Theme::Light, "Light",
                        ).clicked() {
                            self.theme = egui::Theme::Light;
                            self.apply_theme(ctx);
                            ui.close_menu();
                        }
                        if ui.selectable_label(
                            self.theme == egui::Theme::Dark, "Dark",
                        ).clicked() {
                            self.theme = egui::Theme::Dark;
                            self.apply_theme(ctx);
                            ui.close_menu();
                        }
                    });
                });
            });
        });

        self.show_font_settings_window(ctx);
        self.show_note_popup_window(ctx);

        // Bottom pane — declared before CentralPanel so egui allocates
        // its space first and the top pane fills what remains.
        egui::TopBottomPanel::bottom("bottom_pane_container")
            .resizable(true)
            .min_height(80.0)
            .default_height(200.0)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                self.show_bottom_pane(ui);
            });

        egui::TopBottomPanel::bottom("search_bar")
            .resizable(false)
            .exact_height(56.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                self.show_search_bar(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.show_top_pane(ui);
        });
    }
}
