//! Markdown sections: each `#`, `##` or `###` heading starts one.

use super::lines::{Sink, split_windows, windows};
use super::{Chunk, ChunkKind};

const SECTION_MAX_LINES: usize = 120;
const WINDOW_OVERLAP: usize = 20;

/// The most bytes of a heading that [`chunk_markdown_split`] keeps as a section's symbol. It
/// matches the title cap in the documents API, so a heading line cannot multiply into every chunk
/// of its section.
const MAX_SYMBOL_BYTES: usize = 1024;

/// Splits markdown into sections. A section runs from its heading line to the line before the
/// next heading, and its symbol is the heading text without the `#`s. Text before the first
/// heading is a section whose symbol is `default_symbol`. A section over 120 lines becomes
/// windows of 120 lines with 20 overlap. Headings inside fenced code blocks do not count.
/// Chunk text is cut at 8,000 characters, which is right for repo files; see
/// [`chunk_markdown_split`] for text that must be kept whole.
pub fn chunk_markdown(text: &str, default_symbol: &str) -> Vec<Chunk> {
    let mut sink = Sink::new(usize::MAX);
    collect(text, default_symbol, false, &mut sink);
    sink.chunks
}

/// Like [`chunk_markdown`], but no text is lost: windows shrink to fit 8,000 characters and a
/// longer line is cut into several chunks with the same line numbers and symbols. Returns `None`
/// as soon as the text would need more than `max_chunks` chunks, without building the rest.
pub fn chunk_markdown_split(
    text: &str,
    default_symbol: &str,
    max_chunks: usize,
) -> Option<Vec<Chunk>> {
    let mut sink = Sink::new(max_chunks);
    collect(text, default_symbol, true, &mut sink).then_some(sink.chunks)
}

/// Chunks `text` into `sink`. Returns false when `split` is set and `sink` filled up.
fn collect(text: &str, default_symbol: &str, split: bool, sink: &mut Sink) -> bool {
    let lines: Vec<&str> = text.lines().collect();

    // (0-based line, heading text) for every section start, with the preamble first if there is
    // one. No heading text means the default symbol.
    let mut starts: Vec<(usize, Option<&str>)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(marker) = fence_marker(line) {
            match fence {
                None => fence = Some(marker),
                Some((ch, n)) if marker.0 == ch && marker.1 >= n && is_bare_fence(line) => {
                    fence = None;
                }
                Some(_) => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some(heading) = heading_text(line) {
            starts.push((i, Some(heading).filter(|h| !h.is_empty())));
            // Every heading line has text, so each section gives at least one chunk.
            if split && starts.len() > sink.max {
                return false;
            }
        }
    }
    if starts.first().is_none_or(|(i, _)| *i > 0) {
        starts.insert(0, (0, None));
    }

    for (n, (start, heading)) in starts.iter().enumerate() {
        // Split mode is for documents, where a heading line can be as long as the text. Its
        // symbol is copied into every chunk of the section, so it is cut; the heading line itself
        // stays in the chunk text. The default symbol is the caller's and is used as given.
        let symbol = match *heading {
            Some(h) if split => cut_symbol(h),
            Some(h) => h,
            None => default_symbol,
        };
        let symbols = vec![symbol.to_string()];
        let end = starts.get(n + 1).map_or(lines.len(), |(next, _)| *next);
        let (mut a, mut b) = (*start, end);
        // Trim blank edges so line ranges describe the text, not the spacing around it.
        while a < b && lines.get(a).is_none_or(|l| l.trim().is_empty()) {
            a += 1;
        }
        while b > a && lines.get(b - 1).is_none_or(|l| l.trim().is_empty()) {
            b -= 1;
        }
        let Some(section) = lines.get(a..b) else {
            continue;
        };
        if section.iter().all(|l| l.trim().is_empty()) {
            continue;
        }
        if split {
            if !split_windows(
                section,
                a,
                SECTION_MAX_LINES,
                WINDOW_OVERLAP,
                &symbols,
                ChunkKind::Section,
                sink,
            ) {
                return false;
            }
        } else {
            sink.chunks.extend(windows(
                section,
                a,
                SECTION_MAX_LINES,
                WINDOW_OVERLAP,
                &symbols,
                ChunkKind::Section,
            ));
        }
    }
    true
}

/// `s` cut to at most [`MAX_SYMBOL_BYTES`] bytes, on a char boundary.
fn cut_symbol(s: &str) -> &str {
    s.get(..s.floor_char_boundary(MAX_SYMBOL_BYTES))
        .unwrap_or(s)
}

/// The heading text if `line` is an ATX heading of level 1 to 3: one to three `#`s at the start
/// of the line, then a space or tab (or the end of the line).
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=3).contains(&hashes) {
        return None;
    }
    let rest = line.get(hashes..)?;
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(rest.trim())
}

/// The fence character and run length if `line` opens or closes a fenced code block.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let ch = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = trimmed.chars().take_while(|c| *c == ch).count();
    (n >= 3).then_some((ch, n))
}

/// A closing fence has nothing after the fence characters.
fn is_bare_fence(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(ch) = trimmed.chars().next() else {
        return false;
    };
    trimmed.chars().all(|c| c == ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(chunks: &[Chunk]) -> Vec<(u32, u32)> {
        chunks.iter().map(|c| (c.start_line, c.end_line)).collect()
    }

    #[test]
    fn headings_become_sections() {
        let text =
            "# Title\nintro\n\n## Setup\nrun it\n\n### Detail\nmore\n#### Not a section\nx\n";
        let chunks = chunk_markdown(text, "doc");
        let symbols: Vec<&[String]> = chunks.iter().map(|c| c.symbols.as_slice()).collect();
        assert_eq!(
            symbols,
            vec![
                &["Title".to_string()][..],
                &["Setup".to_string()][..],
                &["Detail".to_string()][..],
            ]
        );
        assert_eq!(ranges(&chunks), vec![(1, 2), (4, 5), (7, 10)]);
        assert!(chunks[0].text.starts_with("# Title"));
        assert!(chunks[2].text.contains("#### Not a section"));
        assert!(chunks.iter().all(|c| c.kind == ChunkKind::Section));
    }

    #[test]
    fn text_before_first_heading_uses_the_default_symbol() {
        let chunks = chunk_markdown("preface line\n\n# First\nbody", "readme");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].symbols, vec!["readme".to_string()]);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 1));
        assert_eq!(chunks[1].symbols, vec!["First".to_string()]);
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (3, 4));
    }

    #[test]
    fn no_headings_is_one_default_section() {
        let chunks = chunk_markdown("just\nprose\n", "notes");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].symbols, vec!["notes".to_string()]);
    }

    #[test]
    fn long_section_becomes_windows_with_the_heading_symbol() {
        let mut text = String::from("## Big\n");
        for i in 1..=200 {
            text.push_str(&format!("row {i}\n"));
        }
        let chunks = chunk_markdown(&text, "doc");
        assert_eq!(ranges(&chunks), vec![(1, 120), (101, 201)]);
        for c in &chunks {
            assert_eq!(c.symbols, vec!["Big".to_string()]);
            assert_eq!(c.kind, ChunkKind::Section);
        }
    }

    #[test]
    fn hash_lines_in_code_fences_are_not_headings() {
        let text = "# Install\n```sh\n# comment, not a heading\nmake\n```\n## Next\nx\n";
        let chunks = chunk_markdown(text, "doc");
        assert_eq!(ranges(&chunks), vec![(1, 5), (6, 7)]);
    }

    #[test]
    fn not_headings() {
        // No space after the hashes, or too many hashes, or indented.
        let chunks = chunk_markdown("#nospace\n####### seven\n  # indented\n", "doc");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].symbols, vec!["doc".to_string()]);
    }

    #[test]
    fn empty_heading_and_blank_input() {
        let chunks = chunk_markdown("#\ntext", "doc");
        assert_eq!(chunks[0].symbols, vec!["doc".to_string()]);
        assert!(chunk_markdown("", "doc").is_empty());
        assert!(chunk_markdown("\n\n", "doc").is_empty());
    }

    #[test]
    fn crlf_headings() {
        let chunks = chunk_markdown("# A\r\nbody\r\n# B\r\nmore", "doc");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].symbols, vec!["A".to_string()]);
        assert_eq!(chunks[0].text, "# A\nbody");
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (3, 4));
    }

    const BUDGET: usize = super::super::MAX_CHUNK_CHARS;

    fn split(text: &str) -> Vec<Chunk> {
        chunk_markdown_split(text, "doc", usize::MAX).unwrap()
    }

    #[test]
    fn split_mode_matches_the_plain_mode_for_ordinary_text() {
        let mut text = String::from("preface\n\n## Big\n");
        for i in 1..=200 {
            text.push_str(&format!("row {i}\n"));
        }
        text.push_str("\n# Last\ntail\n");
        assert_eq!(split(&text), chunk_markdown(&text, "doc"));
        assert_eq!(
            split("# A\r\nbody\r\n# B\r\nmore"),
            chunk_markdown("# A\r\nbody\r\n# B\r\nmore", "doc")
        );
        assert!(split("").is_empty());
        assert!(split("\n \n").is_empty());
    }

    #[test]
    fn a_long_single_line_is_split_not_cut() {
        let words: Vec<String> = (0..4000).map(|i| format!("w{i}")).collect();
        let line = format!("{} farendword", words.join(" "));
        assert!(line.chars().count() > 20_000);
        let chunks = split(&line);
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.text.chars().count() <= BUDGET);
            assert_eq!((c.start_line, c.end_line), (1, 1));
            assert_eq!(c.symbols, vec!["doc".to_string()]);
            assert_eq!(c.kind, ChunkKind::Section);
        }
        // The pieces add up to the line, and the last word is whole in the last piece.
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, line);
        assert!(chunks.last().unwrap().text.ends_with(" farendword"));
        // Cuts fall after a space, so no word is torn between two chunks.
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.text.ends_with(' '), "{:?}", &c.text[c.text.len() - 10..]);
        }
    }

    #[test]
    fn an_unbroken_line_is_cut_on_char_boundaries_and_keeps_every_char() {
        let line = "é€𝄞x".repeat(5000);
        let chunks = split(&line);
        assert_eq!(chunks.len(), 3);
        for c in &chunks {
            assert!(c.text.chars().count() <= BUDGET);
        }
        assert_eq!(chunks[0].text.chars().count(), BUDGET);
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, line);
    }

    #[test]
    fn a_long_line_among_short_ones_keeps_its_own_line_number() {
        let long = "z".repeat(BUDGET * 2 + 5);
        let text = format!("# Head\nbefore\n{long}\nafter");
        let chunks = split(&text);
        let long_chunks: Vec<&Chunk> = chunks.iter().filter(|c| c.text.starts_with('z')).collect();
        assert_eq!(long_chunks.len(), 3);
        for c in &long_chunks {
            assert_eq!((c.start_line, c.end_line), (3, 3));
            assert_eq!(c.symbols, vec!["Head".to_string()]);
        }
        // The lines on both sides are still in some chunk.
        assert!(chunks.iter().any(|c| c.text.contains("before")));
        assert!(chunks.iter().any(|c| c.text.contains("after")));
        // Nothing is repeated: the oversized line does not drag its neighbours into windows.
        assert_eq!(
            chunks.iter().filter(|c| c.text.contains("before")).count(),
            1
        );
        assert_eq!(
            chunks.iter().filter(|c| c.text.contains("after")).count(),
            1
        );
    }

    #[test]
    fn windows_shrink_to_the_budget_and_every_row_is_in_a_chunk() {
        let rows: Vec<String> = (0..300)
            .map(|i| format!("row{i:03} {}", "x".repeat(93)))
            .collect();
        assert!(rows.iter().all(|r| r.chars().count() == 100));
        let text = rows.join("\n");
        let chunks = split(&text);
        for c in &chunks {
            assert!(c.text.chars().count() <= BUDGET);
            assert_eq!(c.text.lines().count() as u32, c.end_line - c.start_line + 1);
        }
        for r in &rows {
            assert!(chunks.iter().any(|c| c.text.contains(r.as_str())), "{r}");
        }
        // The plain mode loses rows to its cut, which is what this mode is for.
        let plain = chunk_markdown(&text, "doc");
        assert!(
            rows.iter()
                .any(|r| !plain.iter().any(|c| c.text.contains(r.as_str())))
        );
        // Windows overlap by 20 lines and still move forward.
        assert!(chunks[1].start_line <= chunks[0].end_line);
        assert!(chunks[1].start_line > chunks[0].start_line);
        assert_eq!(chunks.last().unwrap().end_line, 300);
    }

    #[test]
    fn short_windows_still_move_forward() {
        // Two 3,000 character lines per window.
        let rows: Vec<String> = (0..10)
            .map(|i| format!("r{i} {}", "y".repeat(2997)))
            .collect();
        let text = rows.join("\n");
        let chunks = split(&text);
        for r in &rows {
            assert!(chunks.iter().any(|c| c.text.contains(r.as_str())));
        }
        assert!(chunks.len() <= 20, "{} chunks", chunks.len());
        for pair in chunks.windows(2) {
            assert!(pair[1].start_line > pair[0].start_line);
        }
    }

    #[test]
    fn whitespace_only_long_lines_are_not_chunks() {
        let text = format!("a\n{}\nb", " ".repeat(BUDGET * 3));
        let chunks = split(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| !c.text.trim().is_empty()));
    }

    #[test]
    fn the_chunk_cap_is_exact() {
        let text: String = (0..10).map(|i| format!("# h{i}\nbody\n")).collect();
        assert_eq!(chunk_markdown_split(&text, "doc", 10).unwrap().len(), 10);
        assert!(chunk_markdown_split(&text, "doc", 9).is_none());
        assert!(chunk_markdown_split("text", "doc", 0).is_none());
        assert_eq!(chunk_markdown_split("", "doc", 0).unwrap().len(), 0);
    }

    #[test]
    fn the_chunk_cap_counts_pieces_of_a_long_line_and_windows() {
        let line = "q".repeat(BUDGET * 4);
        assert_eq!(chunk_markdown_split(&line, "doc", 4).unwrap().len(), 4);
        assert!(chunk_markdown_split(&line, "doc", 3).is_none());
        let rows: String = (0..300).map(|_| format!("{}\n", "x".repeat(99))).collect();
        let n = split(&rows).len();
        assert!(n > 3);
        assert!(chunk_markdown_split(&rows, "doc", n).is_some());
        assert!(chunk_markdown_split(&rows, "doc", n - 1).is_none());
    }

    #[test]
    fn chunking_stops_at_the_cap_instead_of_building_everything() {
        // 2,000 lines of 7,000 characters would make 2,000 chunks.
        let text: String = (0..2000)
            .map(|_| format!("{}\n", "m".repeat(7000)))
            .collect();
        let mut sink = Sink::new(5);
        assert!(!collect(&text, "doc", true, &mut sink));
        assert_eq!(sink.chunks.len(), 5);

        // A long line stops mid-way through its pieces.
        let mut sink = Sink::new(2);
        assert!(!collect(&"n".repeat(BUDGET * 50), "doc", true, &mut sink));
        assert_eq!(sink.chunks.len(), 2);

        // Bare headings are counted while scanning, before any chunk is built.
        let text = "#\n".repeat(100_000);
        let mut sink = Sink::new(5);
        assert!(!collect(&text, "doc", true, &mut sink));
        assert!(sink.chunks.is_empty());

        // The plain mode has no cap.
        let mut sink = Sink::new(usize::MAX);
        assert!(collect(&"#\n".repeat(3000), "doc", false, &mut sink));
        assert_eq!(sink.chunks.len(), 3000);
    }

    #[test]
    fn a_long_heading_is_cut_in_the_symbol_of_every_chunk_but_not_in_the_text() {
        let heading: String = (0..20_000).map(|i| format!("w{i} ")).collect();
        let heading = heading.trim_end();
        assert!(heading.len() > 100_000);
        let text = format!("# {heading}\nbody");
        let chunks = split(&text);
        assert!(chunks.len() > 10);
        for c in &chunks {
            assert_eq!(c.symbols.len(), 1);
            assert!(
                c.symbols[0].len() <= MAX_SYMBOL_BYTES,
                "{}",
                c.symbols[0].len()
            );
            assert!(heading.starts_with(&c.symbols[0]));
        }
        assert_eq!(chunks[0].symbols[0].len(), MAX_SYMBOL_BYTES);
        // The heading line is split like any long line: none of it is lost.
        assert!(
            chunks
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>()
                .starts_with("# w0 w1 ")
        );
        assert!(chunks.iter().any(|c| c.text.contains("w19999")));
        // The default symbol is the caller's and is not touched, even when it is long.
        let title = "t".repeat(MAX_SYMBOL_BYTES * 3);
        let chunks = chunk_markdown_split("no heading here", &title, usize::MAX).unwrap();
        assert_eq!(chunks[0].symbols, vec![title]);
    }

    #[test]
    fn a_heading_is_cut_on_a_char_boundary() {
        // 2 bytes each: the cap falls between characters.
        let heading = "\u{e9}".repeat(1000);
        let chunks = split(&format!("# {heading}"));
        assert_eq!(chunks[0].symbols[0], "\u{e9}".repeat(512));
        // 3 bytes each: 341 characters are 1,023 bytes, and the 342nd would pass the cap.
        let heading = "\u{20ac}".repeat(1000);
        let chunks = split(&format!("# {heading}"));
        assert_eq!(chunks[0].symbols[0], "\u{20ac}".repeat(341));
        // 4 bytes each, behind one ASCII byte: the cap falls inside the 256th emoji.
        let heading = format!("a{}", "\u{1f600}".repeat(1000));
        let chunks = split(&format!("# {heading}"));
        assert_eq!(
            chunks[0].symbols[0],
            format!("a{}", "\u{1f600}".repeat(255))
        );
        for c in &chunks {
            assert_eq!(c.symbols[0], chunks[0].symbols[0]);
        }
    }

    #[test]
    fn a_heading_at_the_cap_is_kept_whole_and_one_byte_over_is_cut() {
        let at = "h".repeat(MAX_SYMBOL_BYTES);
        assert_eq!(split(&format!("# {at}"))[0].symbols, vec![at.clone()]);
        let over = "h".repeat(MAX_SYMBOL_BYTES + 1);
        assert_eq!(split(&format!("# {over}"))[0].symbols, vec![at.clone()]);
        // A two-byte character that straddles the cap is dropped whole.
        let straddle = format!("{}\u{e9}", "h".repeat(MAX_SYMBOL_BYTES - 1));
        assert_eq!(
            split(&format!("# {straddle}"))[0].symbols,
            vec!["h".repeat(MAX_SYMBOL_BYTES - 1)]
        );
    }

    #[test]
    fn the_plain_mode_keeps_a_long_heading_symbol() {
        let heading = "h".repeat(MAX_SYMBOL_BYTES * 4);
        let chunks = chunk_markdown(&format!("# {heading}\nbody"), "doc");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].symbols, vec![heading]);
    }

    #[test]
    fn split_mode_is_total() {
        for text in [
            "\u{0}",
            "\r",
            "\r\n\r\n",
            "```",
            "# ",
            "\t#\t",
            "é",
            &"\n".repeat(1000),
        ] {
            let _ = chunk_markdown_split(text, "", 10);
        }
    }
}
