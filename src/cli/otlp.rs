//! `lore otlp send`: post an OTLP JSON file to `<url>/v1/traces`.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use prost::Message;

use super::client::{Auth, Client, error_message};
use crate::otlp::{JSON, PROTOBUF, decode};

#[derive(Subcommand)]
pub enum OtlpCommand {
    /// Post an OTLP JSON trace file to <url>/v1/traces, to lore or any OTLP/HTTP receiver. Uses
    /// the global --url and --key
    Send(SendArgs),
}

#[derive(Args)]
pub struct SendArgs {
    /// OTLP JSON file (a trace export request)
    file: PathBuf,
    /// Send protobuf instead of JSON (the file is parsed and re-encoded)
    #[arg(long)]
    protobuf: bool,
    /// Compress the body with gzip
    #[arg(long)]
    gzip: bool,
}

/// The content type and body to send for the JSON file `json`.
fn build_body(json: &[u8], protobuf: bool, gzip: bool) -> Result<(&'static str, Vec<u8>)> {
    let (content_type, mut body) = if protobuf {
        let req = decode(JSON, None, json).map_err(|e| anyhow::anyhow!("{}", e.message()))?;
        (PROTOBUF, req.encode_to_vec())
    } else {
        (JSON, json.to_vec())
    };
    if gzip {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&body).context("compressing the body")?;
        body = enc.finish().context("compressing the body")?;
    }
    Ok((content_type, body))
}

pub async fn run_otlp(client: &Client, command: OtlpCommand) -> Result<()> {
    match command {
        OtlpCommand::Send(args) => send(client, args).await,
    }
}

async fn send(client: &Client, args: SendArgs) -> Result<()> {
    let json = tokio::fs::read(&args.file)
        .await
        .with_context(|| format!("reading {}", args.file.display()))?;
    let (content_type, body) = build_body(&json, args.protobuf, args.gzip)?;
    let encoding = args.gzip.then_some("gzip");
    let (status, text) = client
        .post_raw("/v1/traces", Auth::Key, content_type, encoding, body)
        .await?;
    println!("HTTP {status}");
    if !(200..300).contains(&status) {
        let message = error_message(&text);
        if message.is_empty() {
            bail!("server returned {status}");
        }
        bail!("server returned {status}: {message}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    const MINIMAL: &str = r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{
        "traceId":"5b8efff798038103d269b633813fc60c","spanId":"0102030405060708","name":"x"}]}]}]}"#;

    #[test]
    fn json_body_is_sent_as_is() {
        let (ct, body) = build_body(MINIMAL.as_bytes(), false, false).unwrap();
        assert_eq!(ct, "application/json");
        assert_eq!(body, MINIMAL.as_bytes());
    }

    #[test]
    fn protobuf_body_decodes_back() {
        let (ct, body) = build_body(MINIMAL.as_bytes(), true, false).unwrap();
        assert_eq!(ct, "application/x-protobuf");
        let req = decode(ct, None, &body).unwrap();
        assert_eq!(req.resource_spans[0].scope_spans[0].spans[0].name, "x");
    }

    #[test]
    fn gzip_body_decodes_back() {
        let (ct, body) = build_body(MINIMAL.as_bytes(), true, true).unwrap();
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(&body[..])
            .read_to_end(&mut plain)
            .unwrap();
        assert!(decode(ct, None, &plain).is_ok());
        assert!(decode(ct, Some("gzip"), &body).is_ok());
    }

    #[test]
    fn invalid_json_fails_for_protobuf() {
        assert!(build_body(b"nope", true, false).is_err());
    }
}
