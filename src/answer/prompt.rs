//! The prompt of fast mode: a fixed system prompt, and the retrieved passages as numbered
//! sources followed by the question.

use crate::search::Hit;

pub const FAST_SYSTEM: &str = "You answer questions about a team's code, documents and agent sessions. \
Use only the numbered sources. Cite every claim with its source number in square brackets, like [2]. \
If the sources do not contain the answer, say so and say what is missing. \
Answer in at most 8 short sentences or a short list. Count carefully when asked how many.";

/// The most lines of a passage that go into the prompt.
const MAX_LINES: usize = 120;

const PREFIX: &str = "Sources:\n\n";

/// One numbered source: a header line, then the text in a fence cut to its first 120 lines.
fn render_source(n: usize, hit: &Hit) -> String {
    let header = match (&hit.path, hit.start_line, hit.end_line) {
        (Some(path), Some(start), Some(end)) => {
            format!("[{n}] {path}:{start}-{end} ({})", hit.kind)
        }
        (Some(path), _, _) => format!("[{n}] {path} ({})", hit.kind),
        (None, _, _) => format!("[{n}] {} ({})", hit.title, hit.kind),
    };
    let text = hit
        .text
        .lines()
        .take(MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}\n```\n{text}\n```")
}

/// The user message: `Sources:`, the hits numbered from 1 in the order given, then the question.
/// Hits are added while the whole message stays within `max_chars`. Returns the message and how
/// many sources it holds.
pub fn fast_user_message(question: &str, hits: &[Hit], max_chars: usize) -> (String, usize) {
    let suffix = format!("\n\nQuestion: {question}");
    let mut used = PREFIX.chars().count() + suffix.chars().count();
    let mut sources: Vec<String> = Vec::new();
    for (i, hit) in hits.iter().enumerate() {
        let source = render_source(i + 1, hit);
        // Sources are joined by a blank line.
        let sep = if sources.is_empty() { 0 } else { 2 };
        let cost = sep + source.chars().count();
        if used + cost > max_chars {
            break;
        }
        used += cost;
        sources.push(source);
    }
    let message = format!("{PREFIX}{}{suffix}", sources.join("\n\n"));
    (message, sources.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn hit(path: Option<&str>, text: &str) -> Hit {
        Hit {
            chunk_id: 1,
            document_id: Uuid::nil(),
            source: "sample".to_string(),
            kind: "code".to_string(),
            title: "Standup notes".to_string(),
            uri: None,
            path: path.map(str::to_string),
            external_id: None,
            start_line: path.map(|_| 7),
            end_line: path.map(|_| 14),
            symbols: Vec::new(),
            text: text.to_string(),
            score: 0.03,
        }
    }

    #[test]
    fn sources_are_numbered_from_one() {
        let hits = vec![
            hit(Some("src/a.rs"), "fn a() {}"),
            hit(None, "meeting notes"),
        ];
        let (msg, n) = fast_user_message("what is a?", &hits, 60_000);
        assert_eq!(n, 2);
        assert_eq!(
            msg,
            "Sources:\n\n\
             [1] src/a.rs:7-14 (code)\n```\nfn a() {}\n```\n\n\
             [2] Standup notes (code)\n```\nmeeting notes\n```\n\n\
             Question: what is a?"
        );
    }

    #[test]
    fn a_session_hit_is_listed_by_its_title_and_kind() {
        let mut h = hit(None, "user: How many plans do we have?");
        h.kind = "session".to_string();
        h.title = "How many plans do we have?".to_string();
        h.start_line = None;
        h.end_line = None;
        h.external_id = Some("sess-1".to_string());
        let (msg, n) = fast_user_message("q", &[h], 60_000);
        assert_eq!(n, 1);
        assert!(
            msg.contains("[1] How many plans do we have? (session)\n```\n"),
            "{msg}"
        );
    }

    #[test]
    fn stops_before_max_chars() {
        let hits = vec![
            hit(Some("src/a.rs"), &"x".repeat(100)),
            hit(Some("src/b.rs"), &"y".repeat(100)),
            hit(Some("src/c.rs"), &"z".repeat(100)),
        ];
        let (full, n) = fast_user_message("q", &hits, usize::MAX);
        assert_eq!(n, 3);
        let one = fast_user_message("q", &hits[..1], usize::MAX)
            .0
            .chars()
            .count();
        let two = fast_user_message("q", &hits[..2], usize::MAX)
            .0
            .chars()
            .count();
        assert!(one < two && two < full.chars().count());

        // Exactly enough room for two sources: the third is left out.
        let (msg, n) = fast_user_message("q", &hits, two);
        assert_eq!(n, 2);
        assert_eq!(msg.chars().count(), two);
        assert!(!msg.contains("src/c.rs"));
        // One char less than that and only the first fits.
        let (msg, n) = fast_user_message("q", &hits, two - 1);
        assert_eq!(n, 1);
        assert!(msg.ends_with("\n\nQuestion: q"));
        // Nothing fits.
        let (msg, n) = fast_user_message("q", &hits, one - 1);
        assert_eq!(n, 0);
        assert_eq!(msg, "Sources:\n\n\n\nQuestion: q");
    }

    #[test]
    fn each_source_is_cut_to_120_lines() {
        let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let (msg, n) = fast_user_message("q", &[hit(Some("src/a.rs"), &text)], 60_000);
        assert_eq!(n, 1);
        assert!(msg.contains("line 120\n```"));
        assert!(!msg.contains("line 121"));
    }

    #[test]
    fn a_path_without_lines_shows_the_path_alone() {
        let mut h = hit(Some("README.md"), "hello");
        h.start_line = None;
        h.end_line = None;
        let (msg, _) = fast_user_message("q", &[h], 60_000);
        assert!(msg.contains("[1] README.md (code)\n"), "{msg}");
    }
}
