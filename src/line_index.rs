// line_index.rs — exact, full-scan line indexing.
//
// This replaces large_text_core::LineIndexer for rlogg's purposes.
//
// WHY THIS EXISTS
// ================
// large_text_core::LineIndexer switches to "sparse sampling" for files over
// 10MB: instead of recording every line's exact byte offset, it stores
// checkpoints every 10MB and estimates intermediate line positions via
// `byte_offset / average_line_length`. Because real files rarely have
// perfectly uniform line lengths, this estimate drifts as you go deeper into
// the file — confirmed in testing to be off by a full line (or more) by
// ~63,000 lines into a 12MB file with varying line lengths. Worse, because
// the estimate isn't 1:1 with real lines, two different estimated line
// numbers can resolve to the SAME real line (duplicates), or skip a real
// line entirely (gaps).
//
// FullLineIndex avoids all of this by always doing an exact, full byte scan
// — the same thing large_text_core does internally for files ≤ 10MB, just
// applied unconditionally regardless of file size.
//
// MEMORY COST
// ===========
// The index is one `usize` (8 bytes on 64-bit) per line. For very large
// files this is still small in absolute terms:
//   1,000,000 lines  →  8 MB
//   10,000,000 lines →  80 MB
//   100,000,000 lines → 800 MB
// This scales with LINE COUNT, not file size or line content — a file with
// one 10MB line costs the same 8 bytes to index as a file with one short
// line. For any realistic log file this is trivial against available DRAM,
// and far smaller than the file itself (which lives in the OS page cache via
// FileReader's mmap, not duplicated here).
//
// WHAT IS / ISN'T STORED
// =======================
// Only byte POSITIONS are stored — never line content. Looking up a line's
// actual text is always a separate step: find its [start, end) byte range
// here, then read those exact bytes from FileReader on demand. This is the
// same two-step pattern large_text_core uses; we're only replacing the
// position-lookup half, not the byte-reading half.

use large_text_core::file_reader::FileReader;

pub struct FullLineIndex {
    /// line_offsets[n] = byte offset where line n starts.
    /// line_offsets[n+1] (or file_size, for the last line) marks where it ends.
    /// Always has at least one entry (0), even for an empty file.
    line_offsets: Vec<usize>,
    file_size: usize,
}

impl FullLineIndex {
    /// Scan `reader`'s entire contents once and build an exact line index.
    ///
    /// This is a blocking, O(file_size) operation — by design, per the
    /// decision to do full indexing synchronously at file-open rather than
    /// sparse-sampling or background-indexing. For a multi-GB file this is a
    /// one-time wait measured in seconds, not the per-frame cost; once built,
    /// every lookup below is O(1) (direct index) or O(log n) (binary search).
    pub fn build(reader: &FileReader) -> Self {
        let data = reader.all_data();
        let mut line_offsets = Vec::with_capacity(data.len() / 40 + 1); // rough guess, just an allocation hint
        line_offsets.push(0);

        for (i, &byte) in data.iter().enumerate() {
            if byte == b'\n' {
                line_offsets.push(i + 1);
            }
        }

        // If the file doesn't end with a newline, the last partial line is
        // still a real line (line_offsets already has its start; its end
        // will be resolved as file_size by line_range/get_line_with_reader).
        // If the file is empty, line_offsets is just [0] and total_lines()
        // correctly reports 1 (one empty line) — matching how a text editor
        // would treat an empty file.

        let file_size = reader.len();
        Self {
            line_offsets,
            file_size,
        }
    }

    /// Total number of lines in the file. Exact — not an estimate.
    pub fn total_lines(&self) -> usize {
        self.line_offsets.len()
    }

    /// Return the [start, end) byte range for line `line_no` (0-based).
    /// `end` is exclusive and points at the start of the next line (or
    /// file_size for the last line). Returns None if line_no is out of range.
    ///
    /// Signature deliberately matches large_text_core::LineIndexer's
    /// get_line_with_reader(line_no, reader) so call sites in app.rs needed
    /// minimal changes — though here `reader` isn't actually needed (no
    /// estimation/scanning required, the exact offset is already known), it's
    /// kept as an unused parameter for drop-in compatibility. If you'd rather
    /// have a cleaner signature, it's safe to drop the parameter and update
    /// call sites — nothing about it is load-bearing.
    pub fn get_line_with_reader(
        &self,
        line_no: usize,
        _reader: &FileReader,
    ) -> Option<(usize, usize)> {
        let start = *self.line_offsets.get(line_no)?;
        let end = self
            .line_offsets
            .get(line_no + 1)
            .copied()
            .unwrap_or(self.file_size);
        Some((start, end))
    }

    /// Find which line (0-based) contains the given byte offset.
    /// Exact — uses binary search over real recorded line starts, not
    /// arithmetic estimation, so there's no drift and no possibility of two
    /// different offsets in different real lines resolving to the same line
    /// number (the bug we hit with the old sparse-mode estimator).
    pub fn find_line_at_offset(&self, offset: usize) -> usize {
        match self.line_offsets.binary_search(&offset) {
            // Exact match: offset IS a line's start byte.
            Ok(line) => line,
            // No exact match: insertion_point is the index where `offset`
            // would be inserted to keep the vec sorted, i.e. one past the
            // line that contains it. Subtract 1 to get the containing line.
            Err(insertion_point) => insertion_point.saturating_sub(1),
        }
    }
}
