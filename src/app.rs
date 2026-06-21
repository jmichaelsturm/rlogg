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
    /// Exact, full-scan line index built from the file reader. Gives us line
    /// count and the ability to map line numbers ↔ byte offsets. See
    /// line_index.rs for why this replaces large_text_core::LineIndexer
    /// (whose sparse-sampling mode for files >10MB has estimation drift).
    line_indexer: Option<FullLineIndex>,
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

    // --- Notes ---------------------------------------------------------------
    /// User notes, keyed by 0-based file line number. A single-line String
    /// per line, as specified — not a list, so adding a note to an
    /// already-noted line overwrites it (this is what the edit-in-place
    /// popup behavior implies).
    notes: BTreeMap<usize, String>,
    /// Which line (if any) the note popup is currently open for. None means
    /// the popup is closed. Distinct from "has a note" — this can be Some
    /// for a line with no existing note yet (adding a new one).
    note_popup_line: Option<usize>,
    /// Scratch buffer for the popup's text field. Copied from `notes` when
    /// the popup opens (pre-filled for editing), copied back into `notes`
    /// when the popup is confirmed.
    note_popup_text: String,
    /// "Show all notes" checkbox state, next to the search bar. When true,
    /// every noted line is unioned into the bottom pane's results regardless
    /// of the current regex (see run_search / poll_search_results).
    show_all_notes: bool,
    /// Which file LINE NUMBERS (not result-row indices — those shift as the
    /// list gets sorted/inserted into during streaming) are in the bottom
    /// pane's results because their NOTE text matched the regex (as opposed
    /// to the file line's own text). Used purely for the visual "matched via
    /// note" indicator — doesn't affect selection/copy/scroll behavior.
    note_matched_rows: BTreeSet<usize>,
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
            notes: BTreeMap::new(),
            note_popup_line: None,
            note_popup_text: String::new(),
            show_all_notes: false,
            note_matched_rows: BTreeSet::new(),
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

                // Build the line index synchronously. Always a full,
                // exact byte scan — see line_index.rs for why we always do
                // this rather than switching to estimation for large files.
                // This is the one-time, blocking cost we accepted in
                // exchange for correctness: a few seconds for a multi-GB
                // file, never a per-frame cost afterward.
                let indexer = FullLineIndex::build(&reader);

                self.file_reader = Some(reader);
                self.line_indexer = Some(indexer);
                self.file_path = Some(path);
                // Notes belong to the file being opened, not the previous
                // one — reset them here. clear_search() deliberately leaves
                // notes alone (see its comment) since it's also called from
                // the "✕ Clear" button, where notes must survive.
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
        self.note_matched_rows.clear();
        self.selected_matches.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        self.top_pane_scroll_target = None;
        // Deliberately NOT clearing `notes` or `show_all_notes` here — both
        // belong to the open file and should survive a search being cleared
        // (e.g. clicking "✕ Clear"). They're reset separately in open_file,
        // since notes from a previous file are meaningless for a new one.
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

        self.match_line_numbers.clear();
        self.note_matched_rows.clear();
        self.selected_matches.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;

        // Special case: empty regex with "show all notes" checked means
        // "just list every noted line" — skip running an actual (expensive,
        // full-file) regex search for an empty/match-everything pattern.
        if self.regex_input.is_empty() {
            self.regex_error = None;
            self.search_running = false;
            if self.show_all_notes {
                self.match_line_numbers = self.notes.keys().copied().collect();
            }
            return;
        }

        // Validate the regex before doing anything else — both for the file
        // search AND for matching note text below, since both use the same
        // compiled pattern.
        let compiled_regex = match regex::Regex::new(&self.regex_input) {
            Ok(re) => re,
            Err(e) => {
                self.regex_error = Some(format!("Invalid regex: {e}"));
                return;
            }
        };

        self.regex_error = None;
        self.add_to_history(self.regex_input.clone());

        // ── Search note text first ──────────────────────────────────────
        //
        // Notes aren't part of the file's bytes, so SearchEngine (which only
        // searches the mmap'd file) can never see them. We run the same
        // compiled regex against each note's text ourselves and seed the
        // result list with any matches before the file search starts
        // streaming in its own results. note_matched_rows records which LINE
        // NUMBERS matched via their note (vs. the file's own text) — storing
        // line numbers rather than list positions means this stays correct
        // even as match_line_numbers gets sorted/inserted into later.
        for (&line_no, note_text) in &self.notes {
            if compiled_regex.is_match(note_text) {
                self.note_matched_rows.insert(line_no);
                self.match_line_numbers.push(line_no);
            }
        }

        // If "show all notes" is also checked, union in every remaining
        // noted line that didn't already match the regex by its own text —
        // per the union behavior: regex matches + ALL noted lines.
        if self.show_all_notes {
            for &line_no in self.notes.keys() {
                if !self.match_line_numbers.contains(&line_no) {
                    self.match_line_numbers.push(line_no);
                    // Not a regex-text match, but still "from a note" in the
                    // sense that it wouldn't be here without the checkbox —
                    // mark it too so the indicator is consistent.
                    self.note_matched_rows.insert(line_no);
                }
            }
        }

        // At this point match_line_numbers contains note-text matches first
        // (in note iteration order), then any unioned-in notes — NOT sorted
        // by file position. Sort it now so results read top-to-bottom in
        // file order, the way a person scanning the bottom pane expects.
        // note_matched_rows needs no remapping since it's keyed by line
        // number, not position.
        self.match_line_numbers.sort_unstable();
        self.match_line_numbers.dedup();

        // Configure SearchEngine for the file-content search. set_query
        // compiles the regex internally (a second compile of the same
        // pattern — slightly redundant with compiled_regex above, but
        // SearchEngine owns its own Regex internally and doesn't expose a
        // way to inject one, so this is the simplest correct option).
        self.search_engine.set_query(
            self.regex_input.clone(),
            true,  // use_regex
            false, // case_sensitive
        );

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
    /// Convert a byte offset (from a search match) into the line number that
    /// actually contains it.
    ///
    /// `FullLineIndex::find_line_at_offset()` is always exact — it's a binary
    /// search over real recorded line-start positions (see line_index.rs),
    /// not the estimation large_text_core::LineIndexer used to do for files
    /// over 10MB. The verification loop below is kept anyway as a cheap
    /// safety net: against an exact index it confirms the result on the
    /// first check and returns immediately (negligible overhead), but it
    /// guards against any future regression in how the index is built.
    fn resolve_line_for_offset(
        indexer: &FullLineIndex,
        reader: &FileReader,
        byte_offset: usize,
    ) -> usize {
        let mut line_no = indexer.find_line_at_offset(byte_offset);

        // Look up the real byte range for our current estimate.
        let Some((mut start, mut end)) = indexer.get_line_with_reader(line_no, reader) else {
            return line_no;
        };

        // Step forward if the offset lies beyond this line's real end.
        // Bounded loop: drift has only ever been observed as a handful of
        // lines, so this guards against runaway iteration if something
        // unexpected happens rather than looping indefinitely.
        const MAX_CORRECTION_STEPS: usize = 64;
        let mut steps = 0;
        while byte_offset >= end && steps < MAX_CORRECTION_STEPS {
            line_no += 1;
            match indexer.get_line_with_reader(line_no, reader) {
                Some((s, e)) => {
                    start = s;
                    end = e;
                }
                None => break, // ran off the end of the file
            }
            steps += 1;
        }

        // Step backward if the offset lies before this line's real start
        // (can happen if the initial estimate overshot).
        steps = 0;
        while byte_offset < start && line_no > 0 && steps < MAX_CORRECTION_STEPS {
            line_no -= 1;
            match indexer.get_line_with_reader(line_no, reader) {
                Some((s, _e)) => start = s,
                None => break,
            }
            steps += 1;
        }

        line_no
    }

    fn poll_search_results(&mut self) {
        let Some(rx) = &self.search_rx else { return };
        let Some(indexer) = &self.line_indexer else { return };
        let Some(reader) = &self.file_reader else { return };

        // Track every line number already present in match_line_numbers so
        // we can deduplicate against the WHOLE list, not just the previous
        // entry. This used to be a simple "differs from the last push" check,
        // which was sufficient when results only ever arrived in increasing
        // file order. Now that the list can be pre-seeded with note-matched
        // lines (run_search, before the file search even starts), a line
        // whose OWN text and NOTE both match the regex would otherwise be
        // pushed twice — once during note-seeding, once when the file search
        // reaches its byte offset.
        let mut seen: std::collections::HashSet<usize> =
            self.match_line_numbers.iter().copied().collect();

        // Drain up to 10 000 messages per frame to keep the UI responsive.
        for _ in 0..10_000 {
            match rx.try_recv() {
                Ok(SearchMessage::ChunkResult(chunk)) => {
                    for result in chunk.matches {
                        // Convert byte offset → verified-correct 0-based line
                        // number (see resolve_line_for_offset doc comment).
                        let line_no =
                            Self::resolve_line_for_offset(indexer, reader, result.byte_offset);
                        if seen.insert(line_no) {
                            // Insert at the correct sorted position rather
                            // than always pushing to the end, since the list
                            // may already contain note-seeded entries sorted
                            // in run_search. binary_search's Err case gives
                            // the correct insertion index directly.
                            let pos = self
                                .match_line_numbers
                                .binary_search(&line_no)
                                .unwrap_or_else(|i| i);
                            self.match_line_numbers.insert(pos, line_no);
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
    // Notes
    // -----------------------------------------------------------------------

    /// Open the note popup for `line_no`, pre-filling it with the existing
    /// note if there is one (the "edit in place" behavior).
    fn open_note_popup(&mut self, line_no: usize) {
        self.note_popup_text = self.notes.get(&line_no).cloned().unwrap_or_default();
        self.note_popup_line = Some(line_no);
    }

    /// Renders the note-entry popup, opened by double-clicking a line in the
    /// top pane. A single-line text field, confirmed with Enter or a Save
    /// button, or dismissed with Cancel / closing the window.
    ///
    /// On save: writes into `self.notes` (or removes the entry if the text
    /// was cleared to empty — an easy way to delete a note without a
    /// separate button) and re-runs the active search so the new/edited note
    /// is immediately reflected in the bottom pane if it now matches the
    /// regex, or if "show all notes" is checked.
    fn show_note_popup_window(&mut self, ctx: &Context) {
        let Some(line_no) = self.note_popup_line else {
            return;
        };

        let mut still_open = true;
        let mut save = false;
        let mut cancel = false;

        egui::Window::new(format!("Note — line {}", line_no + 1))
            .open(&mut still_open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.note_popup_text)
                        .hint_text("Enter a note for this line…")
                        .desired_width(320.0),
                );
                // Auto-focus so the user can start typing immediately.
                response.request_focus();

                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }

                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    // Only offer an explicit delete when a note already
                    // exists — saving an emptied field also deletes it, but
                    // this is a more discoverable affordance.
                    if self.notes.contains_key(&line_no) && ui.button("Delete").clicked() {
                        self.notes.remove(&line_no);
                        cancel = true; // close without re-saving the (now
                                        // stale) popup text
                    }
                });
            });

        if save {
            let trimmed = self.note_popup_text.trim();
            if trimmed.is_empty() {
                self.notes.remove(&line_no);
            } else {
                self.notes.insert(line_no, trimmed.to_owned());
            }
            self.note_popup_line = None;
            self.note_popup_text.clear();
            // Re-run so a new/changed note's effect on the bottom pane
            // (regex-on-note-text matches, or "show all notes") shows up
            // immediately rather than waiting for the next manual search.
            self.run_search();
        } else if cancel || !still_open {
            self.note_popup_line = None;
            self.note_popup_text.clear();
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

            // "Show all notes" checkbox — union every noted line into the
            // bottom pane's results regardless of the regex (see run_search
            // for the merge logic). Re-runs the search immediately on toggle
            // so the effect is visible without needing to press Filter again.
            if ui
                .checkbox(&mut self.show_all_notes, "Show notes")
                .changed()
            {
                self.run_search();
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
        // Snapshot notes for rendering (avoids a borrow conflict with the
        // &mut self mutation we do after the closure when a double-click
        // is detected).
        let notes_snap = self.notes.clone();

        // Collected outside the closure so &mut self is available afterward.
        let mut double_clicked_line: Option<usize> = None;

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
                    let mut line_label = format!("{:>6}  {}", line_no + 1, line_text);

                    // Append the note inline, visually set off with a
                    // distinct marker so it's clearly not part of the file's
                    // own content. This is purely a DISPLAY concatenation —
                    // the note is never written into the file or merged into
                    // line_text used elsewhere (e.g. clipboard copy uses the
                    // raw file text only, not this label).
                    if let Some(note) = notes_snap.get(&line_no) {
                        line_label.push_str("   📝 ");
                        line_label.push_str(note);
                    }

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
                let row_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.min.x, y_top),
                    egui::vec2(max_width, row_height),
                );

                // Give this row its own click-sensing region, layered on top
                // of the hover-only rect allocated above. interact() doesn't
                // paint anything itself — painting still happens via the
                // painter calls below — it only adds the row to egui's
                // input-hit-testing for this frame so double_clicked() can
                // fire. A unique Id per row (derived from line_no) is
                // required since interact() needs persistent widget identity.
                let row_id = ui.id().with(("top_pane_row", *line_no));
                let row_response = ui.interact(row_rect, row_id, egui::Sense::click());
                if row_response.double_clicked() {
                    double_clicked_line = Some(*line_no);
                }

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

        // Apply the double-click outside the closure so &mut self is free.
        if let Some(line_no) = double_clicked_line {
            self.open_note_popup(line_no);
        }
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
        let notes_snap = self.notes.clone();
        let note_matched_rows_snap = self.note_matched_rows.clone();

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
                    let mut line_label = format!("{:>6}  {}", line_no + 1, line_text);

                    // Append the note inline, mirroring the top pane. Use a
                    // different marker when this row is in the result list
                    // BECAUSE the note text matched the regex (rather than
                    // the file line's own text, or just "show all notes") —
                    // the spec calls for visually distinguishing that case.
                    if let Some(note) = notes_snap.get(&line_no) {
                        if note_matched_rows_snap.contains(&line_no) {
                            line_label.push_str("   🔎📝 "); // matched via note text
                        } else {
                            line_label.push_str("   📝 "); // has a note, matched some other way
                        }
                        line_label.push_str(note);
                    }

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
        self.show_note_popup_window(ctx);

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
