//! Fixed-size line windows: the fallback for unknown languages and unparseable code, and the
//! splitter for oversized items and markdown sections.

use super::{Chunk, ChunkKind};

const WINDOW_LINES: usize = 60;
const WINDOW_OVERLAP: usize = 10;

/// Windows of 60 lines with 10 overlap, no symbols. Whitespace-only windows are skipped.
pub fn chunk(content: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    windows(
        &lines,
        0,
        WINDOW_LINES,
        WINDOW_OVERLAP,
        &[],
        ChunkKind::Lines,
    )
}

/// Overlapping windows over `lines`, where `lines[0]` is the 0-based line `first` of the file.
/// Each window carries `symbols`. The last window always reaches the end of `lines`, and
/// whitespace-only windows are skipped.
pub(crate) fn windows(
    lines: &[&str],
    first: usize,
    size: usize,
    overlap: usize,
    symbols: &[String],
    kind: ChunkKind,
) -> Vec<Chunk> {
    let size = size.max(1);
    let step = size.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let end = start.saturating_add(size).min(lines.len());
        if let Some(window) = lines.get(start..end)
            && window.iter().any(|l| !l.trim().is_empty())
            && let Some(c) =
                Chunk::from_lines(window, first.saturating_add(start), symbols.to_vec(), kind)
        {
            out.push(c);
        }
        if end >= lines.len() {
            break;
        }
        start += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn windows_of_60_with_10_overlap() {
        let chunks = chunk(&numbered(130));
        let ranges: Vec<(u32, u32)> = chunks.iter().map(|c| (c.start_line, c.end_line)).collect();
        assert_eq!(ranges, vec![(1, 60), (51, 110), (101, 130)]);
        // The overlap is real text: lines 51..=60 appear in both neighbours.
        assert!(chunks[0].text.ends_with("line 60"));
        assert!(chunks[1].text.starts_with("line 51\n"));
        assert!(chunks[1].text.contains("line 60\n"));
        for c in &chunks {
            assert_eq!(c.kind, ChunkKind::Lines);
            assert!(c.symbols.is_empty());
        }
    }

    #[test]
    fn short_input_is_one_window() {
        let chunks = chunk("a\nb\nc");
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 3));
        assert_eq!(chunks[0].text, "a\nb\nc");
    }

    #[test]
    fn exactly_one_window_has_no_trailing_duplicate() {
        let chunks = chunk(&numbered(60));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].end_line, 60);
    }

    #[test]
    fn whitespace_only_windows_are_skipped() {
        let mut content = numbered(5);
        content.push_str(&"\n".repeat(200));
        let chunks = chunk(&content);
        // Only the first window has text; later windows are all blank lines.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert!(chunk("   \n\t\n").is_empty());
    }

    #[test]
    fn crlf_and_missing_trailing_newline() {
        let chunks = chunk("one\r\ntwo\r\nthree");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "one\ntwo\nthree");
        assert_eq!(chunks[0].end_line, 3);
    }

    #[test]
    fn one_very_long_line_is_a_single_truncated_chunk() {
        let chunks = chunk(&"x".repeat(100_000));
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 1));
        assert_eq!(chunks[0].text.len(), 8000);
    }
}
