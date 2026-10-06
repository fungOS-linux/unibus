use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::json;
use std::{fs, io::Write};
use unibus::{
    Message,
    client::{Client, ClientConfig},
};
use unibus_ipfs::{CONTENT, Content, ERROR, FETCH, Fetch, Kubo, MAX_CONTENT, MAX_RANGE};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    client: ClientConfig,
    kubo_rpc: String,
    allowed_producers: Vec<String>,
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") if args.len() == 2 => {
            let config: Config = serde_json::from_slice(&fs::read(&args[1])?)?;
            ensure!(
                !config.allowed_producers.is_empty() && config.allowed_producers.len() <= 64,
                "explicit producer allowlist required"
            );
            let kubo = Kubo::new(&config.kubo_rpc)?;
            let client = Client::connect(config.client)?;
            eprintln!("unibus-ipfs ready");
            loop {
                let Some((request, payload)) = client.try_read()? else {
                    continue;
                };
                if request.message.kind != FETCH {
                    continue;
                }
                ensure!(
                    config.allowed_producers.contains(&request.producer_id),
                    "unapproved fetch producer"
                );
                let result = (|| -> Result<_> {
                    ensure!(payload.is_empty(), "fetch requests cannot carry payloads");
                    let fetch: Fetch =
                        serde_json::from_value(serde_json::Value::Object(request.message.body))?;
                    let bytes = kubo.fetch(&fetch)?;
                    Ok((fetch, bytes))
                })();
                let (message, bytes) = match result {
                    Ok((fetch, bytes)) => (
                        Message::new(
                            CONTENT,
                            json!({"cid":fetch.cid, "offset":fetch.offset, "length":bytes.len(), "eof":bytes.len() < fetch.length as usize}),
                        ),
                        bytes,
                    ),
                    Err(error) => {
                        eprintln!("IPFS fetch failed: {error}");
                        (
                            Message::new(ERROR, json!({"error":"fetch_failed"})),
                            Vec::new(),
                        )
                    }
                };
                let mut reply = client.envelope(Some(&request.producer_id), message)?;
                reply.request_id = request.request_id;
                reply.payload_bytes = bytes.len();
                client.send(&reply, &bytes)?;
            }
        }
        Some("fetch") if args.len() == 4 => {
            let kubo = Kubo::new(&args[1])?;
            download(&args[2], &args[3], |request| kubo.fetch(request))?;
        }
        Some("download") if args.len() == 5 => {
            let mut config: ClientConfig = serde_json::from_slice(&fs::read(&args[1])?)?;
            config.read_timeout_ms = 100;
            let client = Client::connect(config)?;
            download(&args[3], &args[4], |request| {
                let envelope = client.envelope(
                    Some(&args[2]),
                    Message::new(FETCH, serde_json::to_value(request)?),
                )?;
                client.send(&envelope, &[])?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
                loop {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "IPFS reply deadline exceeded"
                    );
                    let Some((reply, bytes)) = client.try_read()? else {
                        continue;
                    };
                    if reply.request_id != envelope.request_id || reply.producer_id != args[2] {
                        continue;
                    }
                    ensure!(reply.message.kind != ERROR, "adapter fetch failed");
                    ensure!(reply.message.kind == CONTENT, "unexpected reply kind");
                    let content: Content =
                        serde_json::from_value(serde_json::Value::Object(reply.message.body))?;
                    content.validate(request, &bytes)?;
                    return Ok(bytes);
                }
            })?;
        }
        _ => anyhow::bail!(
            "usage: unibus-ipfs serve CONFIG.json | fetch KUBO_ORIGIN CID NEW_OUTPUT_FILE | download CLIENT_CONFIG.json ADAPTER_PRODUCER CID NEW_OUTPUT_FILE"
        ),
    }
    Ok(())
}

fn download(cid: &str, path: &str, mut fetch: impl FnMut(&Fetch) -> Result<Vec<u8>>) -> Result<()> {
    Fetch {
        cid: cid.into(),
        offset: 0,
        length: MAX_RANGE,
    }
    .validate()?;
    let destination = std::path::Path::new(path);
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    let mut offset = 0;
    loop {
        ensure!(offset < MAX_CONTENT, "content exceeds 64 MiB");
        let length = MAX_RANGE.min(MAX_CONTENT - offset);
        let bytes = fetch(&Fetch {
            cid: cid.into(),
            offset,
            length,
        })?;
        ensure!(bytes.len() as u64 <= length, "range exceeded request");
        output.write_all(&bytes)?;
        offset += bytes.len() as u64;
        if bytes.len() < length as usize {
            break;
        }
    }
    output.as_file().sync_all()?;
    output.persist_noclobber(destination)?;
    println!("fetched {offset} bytes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CID: &str = "QmYwAPJzv5CZsnAzt8auVZRnGi2C5pnPZGoMHfGMhL6VWJ";
    #[test]
    fn failed_download_removes_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("document.pdf");
        let mut calls = 0;
        assert!(
            download(CID, path.to_str().unwrap(), |_| {
                calls += 1;
                if calls == 1 {
                    Ok(vec![0; MAX_RANGE as usize])
                } else {
                    anyhow::bail!("disconnected")
                }
            })
            .is_err()
        );
        assert!(!path.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    #[test]
    fn atomic_commit_does_not_replace_existing_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("document.pdf");
        fs::write(&path, b"original").unwrap();
        assert!(download(CID, path.to_str().unwrap(), |_| Ok(b"pdf".to_vec())).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
    }
    #[test]
    fn ordered_chunks_are_committed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("document.pdf");
        download(CID, path.to_str().unwrap(), |request| {
            match request.offset {
                0 => Ok(vec![1; MAX_RANGE as usize]),
                MAX_RANGE => Ok(vec![2; 3]),
                _ => anyhow::bail!("unexpected range"),
            }
        })
        .unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), MAX_RANGE as usize + 3);
        assert_eq!(&bytes[MAX_RANGE as usize..], &[2; 3]);
    }
}
