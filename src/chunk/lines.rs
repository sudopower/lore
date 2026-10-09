//! Fixed-size line windows: the fallback for unknown languages and unparseable code, and the
//! splitter for oversized items and markdown sections.

use super::{Chunk, ChunkKind, MAX_CHUNK_CHARS, line_number};

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

/// Where split chunking collects its chunks, and the most it will take. Once `max` chunks are
/// stored the next one is refused without being built, so a hostile input stops the work early.
pub(crate) struct Sink {
    pub(crate) chunks: Vec<Chunk>,
    pub(crate) max: usize,
}

impl Sink {
    pub(crate) fn new(max: usize) -> Self {
        Sink {
            chunks: Vec::new(),
            max,
        }
    }

    /// Stores the chunk `make` builds, or returns false (without calling `make`) when `max`
    /// chunks are already stored.
    fn push_with(&mut self, make: impl FnOnce() -> Chunk) -> bool {
        if self.chunks.len() >= self.max {
            return false;
        }
        self.chunks.push(make());
        true
    }
}

/// The characters of the last stretch of a piece in which a cut may move back to a space.
const SPLIT_LOOKBACK_CHARS: usize = 200;

/// Like [`windows`], but nothing is lost to the 8,000 character cap: a window stops growing
/// before it would pass the budget (so it can hold fewer than `size` lines), and a single line
/// over the budget becomes several chunks that all carry that line's number. Chunks go to `sink`;
/// the result is false when `sink` is full and the rest was not chunked.
///
/// The overlap shrinks to half the window when the window is short, and a window that ends in
/// front of an oversized line does not overlap into it, so every window moves forward and no
/// line is repeated more than the overlap asks.
pub(crate) fn split_windows(
    lines: &[&str],
    first: usize,
    size: usize,
    overlap: usize,
    symbols: &[String],
    kind: ChunkKind,
    sink: &mut Sink,
) -> bool {
    let size = size.max(1);
    let mut start = 0;
    while let Some(&line) = lines.get(start) {
        let number = line_number(first.saturating_add(start));
        if chars_up_to(line, MAX_CHUNK_CHARS) > MAX_CHUNK_CHARS {
            if !split_line(line, number, symbols, kind, sink) {
                return false;
            }
            start += 1;
            continue;
        }
        let mut end = start;
        let mut used = 0;
        while let Some(next) = lines.get(end)
            && end - start < size
        {
            let cost = chars_up_to(next, MAX_CHUNK_CHARS) + usize::from(end > start);
            if used + cost > MAX_CHUNK_CHARS {
                break;
            }
            used += cost;
            end += 1;
        }
        let Some(window) = lines.get(start..end) else {
            return true;
        };
        if window.iter().any(|l| !l.trim().is_empty())
            && !sink.push_with(|| {
                Chunk::new(
                    window.join("\n"),
                    number,
                    line_number(first.saturating_add(end - 1)),
                    symbols.to_vec(),
                    kind,
                )
            })
        {
            return false;
        }
        let Some(next) = lines.get(end) else {
            return true;
        };
        // The next line cannot join any window when it is over the budget on its own.
        let overlap = if chars_up_to(next, MAX_CHUNK_CHARS) > MAX_CHUNK_CHARS {
            0
        } else {
            overlap.min((end - start) / 2)
        };
        start = end - overlap;
    }
    true
}

/// Cuts one line into chunks of at most the budget, all numbered `number`. A cut moves back to
/// just after a space when there is one near the end of the piece, so words stay whole; the
/// pieces always add up to the line. Whitespace-only pieces are skipped.
fn split_line(
    line: &str,
    number: u32,
    symbols: &[String],
    kind: ChunkKind,
    sink: &mut Sink,
) -> bool {
    let mut rest = line;
    while !rest.is_empty() {
        let cut = match rest.char_indices().nth(MAX_CHUNK_CHARS) {
            None => rest.len(),
            Some((limit, _)) => rest
                .get(..limit)
                .and_then(|head| {
                    head.char_indices()
                        .rev()
                        .take(SPLIT_LOOKBACK_CHARS)
                        .find(|(_, c)| c.is_whitespace())
                        .map(|(i, c)| i + c.len_utf8())
                })
                .unwrap_or(limit),
        };
        let (piece, tail) = rest.split_at(cut);
        if !piece.trim().is_empty()
            && !sink
                .push_with(|| Chunk::new(piece.to_string(), number, number, symbols.to_vec(), kind))
        {
            return false;
        }
        rest = tail;
    }
    true
}

/// The number of characters in `s`, counting at most `cap + 1` of them.
fn chars_up_to(s: &str, cap: usize) -> usize {
    s.chars().take(cap.saturating_add(1)).count()
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

    fn split(lines: &[&str], size: usize, overlap: usize) -> Vec<Chunk> {
        let mut sink = Sink::new(usize::MAX);
        assert!(split_windows(
            lines,
            0,
            size,
            overlap,
            &[],
            ChunkKind::Section,
            &mut sink
        ));
        sink.chunks
    }

    #[test]
    fn a_joined_window_counts_its_newlines_against_the_budget() {
        // 4,000 + newline + 4,000 is 8,001 characters, one over, so the lines cannot share.
        let (a, b) = ("a".repeat(4000), "b".repeat(4000));
        let chunks = split(&[&a, &b], 120, 0);
        assert_eq!(chunks.len(), 2);
        for c in &chunks {
            assert!(c.text.chars().count() <= MAX_CHUNK_CHARS);
        }
    }

    #[test]
    fn a_window_of_exactly_the_budget_is_kept_whole() {
        // 3,999 + newline + 4,000 is exactly 8,000 characters and fits.
        let (a, b) = ("a".repeat(3999), "b".repeat(4000));
        let chunks = split(&[&a, &b], 120, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text.chars().count(), MAX_CHUNK_CHARS);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 2));
    }

    #[test]
    fn the_overlap_is_at_most_half_of_the_window() {
        // Seven 1,000 character lines fit a window. Half of 7 rounds down to an overlap of 3,
        // not the 20 asked for and not 6.
        let rows: Vec<String> = (0..20).map(|_| "r".repeat(1000)).collect();
        let lines: Vec<&str> = rows.iter().map(String::as_str).collect();
        let chunks = split(&lines, 120, 20);
        let ranges: Vec<(u32, u32)> = chunks.iter().map(|c| (c.start_line, c.end_line)).collect();
        assert_eq!(&ranges[..3], &[(1, 7), (5, 11), (9, 15)]);
    }

    #[test]
    fn a_cut_looks_for_a_space_only_in_the_last_200_characters() {
        // The only space is far from the limit, so the piece is cut at the limit.
        let far = format!("{} {}", "a".repeat(100), "b".repeat(9000));
        let chunks = split(&[&far], 120, 0);
        assert_eq!(chunks[0].text.chars().count(), MAX_CHUNK_CHARS);
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, far);
        // A space in the last 200 characters is used, and the cut falls after it.
        let near = format!("{} {}", "a".repeat(7850), "b".repeat(9000));
        let chunks = split(&[&near], 120, 0);
        assert_eq!(chunks[0].text.chars().count(), 7851);
        assert!(chunks[0].text.ends_with(' '));
    }

    #[test]
    fn whitespace_only_windows_are_not_chunks_in_split_mode() {
        // Windows of two lines: the middle one is blank and gets no chunk.
        let lines = ["a", "", "  ", "\t", "", "b"];
        let chunks = split(&lines, 2, 0);
        let ranges: Vec<(u32, u32)> = chunks.iter().map(|c| (c.start_line, c.end_line)).collect();
        assert_eq!(ranges, vec![(1, 2), (5, 6)]);
        assert!(chunks.iter().all(|c| !c.text.trim().is_empty()));
        assert!(split(&["", " ", "\t"], 2, 0).is_empty());
    }

    #[test]
    fn one_very_long_line_is_a_single_truncated_chunk() {
        let chunks = chunk(&"x".repeat(100_000));
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 1));
        assert_eq!(chunks[0].text.len(), 8000);
    }
}
