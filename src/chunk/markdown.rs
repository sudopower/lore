//! Markdown sections: each `#`, `##` or `###` heading starts one.

use super::lines::windows;
use super::{Chunk, ChunkKind};

const SECTION_MAX_LINES: usize = 120;
const WINDOW_OVERLAP: usize = 20;

/// Splits markdown into sections. A section runs from its heading line to the line before the
/// next heading, and its symbol is the heading text without the `#`s. Text before the first
/// heading is a section whose symbol is `default_symbol`. A section over 120 lines becomes
/// windows of 120 lines with 20 overlap. Headings inside fenced code blocks do not count.
pub fn chunk_markdown(text: &str, default_symbol: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let default_symbols = vec![default_symbol.to_string()];

    // (0-based line, symbols) for every section start, with the preamble first if there is one.
    let mut starts: Vec<(usize, Vec<String>)> = Vec::new();
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
            let symbols = if heading.is_empty() {
                default_symbols.clone()
            } else {
                vec![heading.to_string()]
            };
            starts.push((i, symbols));
        }
    }
    if starts.first().is_none_or(|(i, _)| *i > 0) {
        starts.insert(0, (0, default_symbols));
    }

    let mut out = Vec::new();
    for (n, (start, symbols)) in starts.iter().enumerate() {
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
        out.extend(windows(
            section,
            a,
            SECTION_MAX_LINES,
            WINDOW_OVERLAP,
            symbols,
            ChunkKind::Section,
        ));
    }
    out
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
}
