//! `lore doc put|rm|get`: write, delete and read notes and documents.

use std::fmt::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::Value;

use super::client::{Auth, Client};
use super::print_json;

#[derive(Subcommand)]
pub enum DocCommand {
    /// Create or replace a note or document, and index it for search
    Put(PutArgs),
    /// Delete a note or document
    Rm(RmArgs),
    /// Print the text of a document, by the id search results carry
    Get {
        /// Document id
        document_id: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Kind {
    Note,
    Doc,
}

#[derive(Args)]
pub struct PutArgs {
    /// Source name that groups the documents (A-Z, a-z, 0-9, . _ and -)
    #[arg(long)]
    source: String,
    /// Id of the document within the source. Putting it again replaces the document
    #[arg(long = "id")]
    external_id: String,
    #[arg(long)]
    title: String,
    /// Read the text from this file
    #[arg(long, conflicts_with = "text", required_unless_present = "text")]
    file: Option<PathBuf>,
    /// The text itself
    #[arg(long)]
    text: Option<String>,
    /// note (default) or doc
    #[arg(long, value_enum)]
    kind: Option<Kind>,
    /// Where the document lives, shown with search results
    #[arg(long)]
    uri: Option<String>,
}

#[derive(Args)]
pub struct RmArgs {
    #[arg(long)]
    source: String,
    /// Id the document was put with
    #[arg(long = "id")]
    external_id: String,
}

#[derive(Serialize)]
struct PutBody<'a> {
    source: &'a str,
    documents: [PutDocument<'a>; 1],
}

#[derive(Serialize)]
struct PutDocument<'a> {
    external_id: &'a str,
    title: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<&'a str>,
}

/// Percent-encodes one URL path segment: everything but unreserved characters.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The path of the delete route. Each segment of the id is encoded on its own, so a `/` in the
/// id stays a separator, which is how the server's catch-all segment reads it.
fn delete_path(source: &str, external_id: &str) -> String {
    let id: Vec<String> = external_id.split('/').map(encode_segment).collect();
    format!("/v1/documents/{}/{}", encode_segment(source), id.join("/"))
}

pub async fn run_doc(client: &Client, command: DocCommand) -> Result<()> {
    match command {
        DocCommand::Put(args) => put(client, args).await,
        DocCommand::Rm(args) => {
            client
                .delete(&delete_path(&args.source, &args.external_id), Auth::Key)
                .await
        }
        DocCommand::Get { document_id } => {
            let path = format!("/v1/documents/{}", encode_segment(&document_id));
            let v: Value = client.get_json(&path, Auth::Key).await?;
            let text = v["text"].as_str().unwrap_or_default();
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            Ok(())
        }
    }
}

async fn put(client: &Client, args: PutArgs) -> Result<()> {
    let text = match (&args.file, &args.text) {
        (Some(path), _) => tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?,
        (None, Some(text)) => text.clone(),
        (None, None) => unreachable!("clap requires --file or --text"),
    };
    let kind = args.kind.map(|k| match k {
        Kind::Note => "note",
        Kind::Doc => "doc",
    });
    let body = PutBody {
        source: &args.source,
        documents: [PutDocument {
            external_id: &args.external_id,
            title: &args.title,
            text: &text,
            kind,
            uri: args.uri.as_deref(),
        }],
    };
    let v: Value = client.post_json("/v1/documents", Auth::Key, &body).await?;
    print_json(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(encode_segment("plain-1.2_x~"), "plain-1.2_x~");
        assert_eq!(encode_segment("a b?#%"), "a%20b%3F%23%25");
        assert_eq!(encode_segment("caf\u{e9}"), "caf%C3%A9");
    }

    #[test]
    fn slashes_in_the_id_stay_separators() {
        assert_eq!(
            delete_path("notes", "team/q 3/plan?"),
            "/v1/documents/notes/team/q%203/plan%3F"
        );
        assert_eq!(delete_path("notes", "n1"), "/v1/documents/notes/n1");
    }

    #[test]
    fn put_body_omits_unset_fields() {
        let body = PutBody {
            source: "s",
            documents: [PutDocument {
                external_id: "i",
                title: "t",
                text: "x",
                kind: None,
                uri: None,
            }],
        };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"source":"s","documents":[{"external_id":"i","title":"t","text":"x"}]}"#
        );
    }
}
