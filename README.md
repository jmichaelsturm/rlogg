# rlogg - A fast text viewer inspired by klogg, glogg, and large-text-viewer

A two-pane GUI for filtering large text files by regex, built in Rust with egui.

```
┌──────────────────────────────────────────┐
│  TOP PANE — full file, all lines         │  ← CentralPanel (fills space)
│  selected line highlighted in amber      │
│  scrolls to center the selected line     │
├──────────────────────────────────────────┤
│  🔍 [regex input          ] [Filter] [✕] │  ← fixed search bar
│  42 matches found                        │
├──────────────────────────────────────────┤
│  BOTTOM PANE — matched lines only        │  ← BottomPanel (resizable)
│  click any row → top pane centers it     │
└──────────────────────────────────────────┘
```

## Quick start

```bash
git clone <this repo>
cd regex-filter-viewer
cargo run --release
```

Then: **File → Open…** → pick any text file → type a regex → press **Enter** or **Filter**.

## Integrating large-text-core (for >4 GB files)

1. Uncomment the `large-text-core` dependency in `Cargo.toml`.
2. In `src/app.rs`, delete the stub `FileReader` struct and `impl` block.
3. Replace every use of `FileReader` with `large_text_core::FileReader` and
   `large_text_core::LineIndexer`.
4. Replace the `start_search` function body with
   `large_text_core::SearchEngine::find_matches(reader, pattern, tx)`.

The channel protocol (`SearchMessage::Match(line_no)` / `SearchMessage::Done`)
stays identical — only the implementation behind it changes.

## Key concepts to study

| Concept | Where |
|---|---|
| `egui::TopBottomPanel` layout order | `app.rs` `update()` |
| Virtual scrolling with `show_rows` | `show_top_pane`, `show_bottom_pane` |
| `mpsc` channels for background threads | `poll_search_results`, `start_search` |
| Centered scroll-to on click | `top_pane_scroll_target` + `vertical_scroll_offset` |
| Regex compilation + error handling | `run_search` |
