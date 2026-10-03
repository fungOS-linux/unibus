//! Minimal authenticated DCP 0.1 HTTP client for provider edges.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use dcp::{Catalog, Discovery, ExecuteRequest, Receipt};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub node_id: String,
    pub producer_id: String,
    pub endpoint: String,
    pub bearer_token_file: PathBuf,
    pub user_email: String,
    #[serde(default)]
    pub zone: Option<String>,
}

pub struct DcpHttpClient {
    client: Client,
    endpoint: reqwest::Url,
    headers: HeaderMap,
    zone: Option<String>,
}

impl DcpHttpClient {
    pub fn connect(config: &ProviderConfig) -> Result<Self> {
        let endpoint = reqwest::Url::parse(&format!("{}/", config.endpoint.trim_end_matches('/')))
            .context("parse DCP provider endpoint")?;
        ensure!(
            endpoint.scheme() == "https"
                || endpoint
                    .host_str()
                    .is_some_and(|host| host == "127.0.0.1" || host == "localhost"),
            "DCP provider endpoint must use HTTPS or loopback"
        );
        let token = std::fs::read_to_string(&config.bearer_token_file)
            .with_context(|| format!("read {}", config.bearer_token_file.display()))?;
        let token = token.trim();
        ensure!(!token.is_empty(), "DCP bearer token is empty");
        ensure!(
            !config.user_email.trim().is_empty(),
            "DCP user email is empty"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        headers.insert("dcp-version", HeaderValue::from_static(dcp::VERSION));
        headers.insert(
            "x-fpl-user-email",
            HeaderValue::from_str(config.user_email.trim())?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        Ok(Self {
            client: Client::builder().default_headers(headers.clone()).build()?,
            endpoint,
            headers,
            zone: config.zone.clone(),
        })
    }

    pub fn discovery(&self) -> Result<Discovery> {
        self.get_json(".well-known/dcp")
    }

    pub fn catalog(&self) -> Result<Catalog> {
        self.get_json("v1/decisions")
    }

    pub fn execute(&self, request: &ExecuteRequest) -> Result<Receipt> {
        let response = self
            .client
            .post(self.url("v1/decisions/execute")?)
            .headers(self.headers.clone())
            .json(request)
            .send()?;
        ensure!(
            response.status().is_success() || response.status().as_u16() == 409,
            "DCP execute HTTP status {}",
            response.status()
        );
        ensure_version(&response)?;
        Ok(response.json()?)
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(self.url(path)?)
            .headers(self.headers.clone())
            .send()?;
        ensure!(
            response.status().is_success(),
            "DCP HTTP status {}",
            response.status()
        );
        ensure_version(&response)?;
        Ok(response.json()?)
    }

    fn url(&self, path: &str) -> Result<reqwest::Url> {
        let mut url = self.endpoint.join(path)?;
        if let Some(zone) = self.zone.as_deref().filter(|zone| !zone.trim().is_empty()) {
            url.query_pairs_mut().append_pair("zone", zone.trim());
        }
        Ok(url)
    }
}

fn ensure_version(response: &reqwest::blocking::Response) -> Result<()> {
    ensure!(
        response
            .headers()
            .get("dcp-version")
            .and_then(|value| value.to_str().ok())
            == Some(dcp::VERSION),
        "DCP provider did not echo version {}",
        dcp::VERSION
    );
    Ok(())
}
