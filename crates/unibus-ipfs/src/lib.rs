//! IPFS retrieval belongs to Kubo; Unibus carries bounded immutable ranges.
use anyhow::{Result, ensure};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{io::Read, time::Duration};

pub const FETCH: &str = "unibus.ipfs.fetch.v1";
pub const CONTENT: &str = "unibus.ipfs.content.v1";
pub const ERROR: &str = "unibus.ipfs.error.v1";
pub const MAX_RANGE: u64 = 64 * 1024;
pub const MAX_CONTENT: u64 = 64 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fetch {
    pub cid: String,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Content {
    pub cid: String,
    pub offset: u64,
    pub length: u64,
    pub eof: bool,
}

impl Content {
    pub fn validate(&self, request: &Fetch, payload: &[u8]) -> Result<()> {
        ensure!(
            self.cid == request.cid && self.offset == request.offset,
            "content provenance mismatch"
        );
        ensure!(
            self.length == payload.len() as u64 && self.length <= request.length,
            "content length mismatch"
        );
        ensure!(
            self.eof == (self.length < request.length),
            "invalid EOF flag"
        );
        Ok(())
    }
}

impl Fetch {
    pub fn validate(&self) -> Result<cid::Cid> {
        ensure!(self.cid.len() <= 128, "CID too long");
        let cid = self.cid.parse::<cid::Cid>()?;
        ensure!(
            (1..=MAX_RANGE).contains(&self.length),
            "invalid range length"
        );
        ensure!(
            self.offset
                .checked_add(self.length)
                .is_some_and(|end| end <= MAX_CONTENT),
            "range exceeds content budget"
        );
        Ok(cid)
    }
}

pub struct Kubo {
    endpoint: String,
    client: Client,
}

#[derive(Deserialize)]
struct FileStat {
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "Type")]
    kind: String,
}

impl Kubo {
    /// Admin RPC stays loopback-only. The adapter never forwards arbitrary URLs.
    pub fn new(endpoint: &str) -> Result<Self> {
        let url = reqwest::Url::parse(endpoint)?;
        ensure!(
            url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]")),
            "Kubo RPC must use a loopback IP literal"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "expected bare Kubo origin"
        );
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }

    pub fn fetch(&self, request: &Fetch) -> Result<Vec<u8>> {
        let cid = request.validate()?;
        let stat_response = self
            .client
            .post(format!("{}/api/v0/files/stat", self.endpoint))
            .query(&[("arg", format!("/ipfs/{cid}"))])
            .send()?
            .error_for_status()?;
        ensure!(
            stat_response.status().is_success(),
            "Kubo stat returned non-success"
        );
        let mut stat_bytes = Vec::new();
        stat_response.take(4097).read_to_end(&mut stat_bytes)?;
        ensure!(stat_bytes.len() <= 4096, "Kubo stat response too large");
        let stat: FileStat = serde_json::from_slice(&stat_bytes)?;
        ensure!(
            stat.kind == "file" && stat.size < MAX_CONTENT,
            "CID is not a bounded file"
        );
        ensure!(request.offset <= stat.size, "range starts beyond EOF");
        let expected = request.length.min(stat.size - request.offset);
        if expected == 0 {
            return Ok(Vec::new());
        }
        let response = self
            .client
            .post(format!("{}/api/v0/cat", self.endpoint))
            .query(&[
                ("arg", cid.to_string()),
                ("offset", request.offset.to_string()),
                ("length", request.length.to_string()),
            ])
            .send()?
            .error_for_status()?;
        ensure!(
            response.status().is_success(),
            "Kubo returned a non-success status"
        );
        let mut bytes = Vec::new();
        response.take(request.length + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == expected,
            "Kubo returned incomplete or oversized content"
        );
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CID: &str = "QmYwAPJzv5CZsnAzt8auVZRnGi2C5pnPZGoMHfGMhL6VWJ";
    #[test]
    fn bounded_cid_only_contract() {
        assert!(
            Fetch {
                cid: CID.into(),
                offset: 0,
                length: MAX_RANGE
            }
            .validate()
            .is_ok()
        );
        for bad in ["../../config", "https://example.com", "ipfs://test", ""] {
            assert!(
                Fetch {
                    cid: bad.into(),
                    offset: 0,
                    length: 1
                }
                .validate()
                .is_err()
            );
        }
        for (offset, length) in [(0, 0), (0, MAX_RANGE + 1), (u64::MAX, 1), (MAX_CONTENT, 1)] {
            assert!(
                Fetch {
                    cid: CID.into(),
                    offset,
                    length
                }
                .validate()
                .is_err()
            );
        }
    }
    #[test]
    fn rpc_admin_boundary() {
        assert!(Kubo::new("http://127.0.0.1:5001").is_ok());
        for endpoint in [
            "http://example.com",
            "http://localhost:5001",
            "http://127.0.0.1:5001/foo",
            "http://secret@127.0.0.1:5001",
            "http://127.0.0.1:5001?x=y",
        ] {
            assert!(Kubo::new(endpoint).is_err());
        }
    }

    fn server(status: &str, body: &[u8]) -> (String, std::thread::JoinHandle<String>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let body = body.to_vec();
        let thread = std::thread::spawn(move || {
            let (mut stat_stream, _) = listener.accept().unwrap();
            stat_stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut stat_header = Vec::new();
            while !stat_header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stat_stream.read_exact(&mut byte).unwrap();
                stat_header.push(byte[0]);
                assert!(stat_header.len() < 8192);
            }
            assert!(
                String::from_utf8(stat_header)
                    .unwrap()
                    .starts_with("POST /api/v0/files/stat?arg=")
            );
            let stat = br#"{"Size":6,"Type":"file"}"#;
            write!(
                stat_stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                stat.len()
            )
            .unwrap();
            stat_stream.write_all(stat).unwrap();
            drop(stat_stream);
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut header = Vec::new();
            loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
                assert!(header.len() < 8192);
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            String::from_utf8(header).unwrap()
        });
        (endpoint, thread)
    }

    #[test]
    fn actual_rpc_preserves_range_and_rejects_oversize_and_http_failure() {
        let request = Fetch {
            cid: CID.into(),
            offset: 3,
            length: 4,
        };
        let (endpoint, thread) = server("200 OK", b"pdf");
        assert_eq!(
            Kubo::new(&endpoint).unwrap().fetch(&request).unwrap(),
            b"pdf"
        );
        let header = thread.join().unwrap();
        assert!(header.starts_with("POST /api/v0/cat?arg="));
        assert!(header.contains("offset=3&length=4"));
        for (status, body) in [
            ("200 OK", b"12345".as_slice()),
            ("500 Error", b"bad".as_slice()),
            ("302 Found", b"".as_slice()),
        ] {
            let (endpoint, thread) = server(status, body);
            assert!(Kubo::new(&endpoint).unwrap().fetch(&request).is_err());
            thread.join().unwrap();
        }
    }
}
