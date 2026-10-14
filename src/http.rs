//! HTTP client shared by every command: retries, concurrency limit and the
//! monitoring query parameters expected by the Marine Data Store.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use log::debug;
use serde_json::Value;
use tokio::sync::Semaphore;

const MAX_RETRIES: u32 = 5;
/// The object store sometimes stalls in the middle of a response body.
/// When no data is received during this delay, the request is retried.
const STALL_TIMEOUT: Duration = Duration::from_secs(2);

async fn read_body(response: reqwest::Response) -> Result<Bytes> {
    use futures::StreamExt;
    let mut body = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
    let mut stream = response.bytes_stream();
    loop {
        match tokio::time::timeout(STALL_TIMEOUT, stream.next()).await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk?),
            Ok(None) => return Ok(Bytes::from(body)),
            Err(_) => bail!("no data received for {STALL_TIMEOUT:?}"),
        }
    }
}

#[derive(Clone)]
pub struct Http {
    pub client: reqwest::Client,
    query_params: Vec<(String, String)>,
    semaphore: Arc<Semaphore>,
}

impl Http {
    pub fn new(username: Option<&str>, max_concurrent_requests: usize) -> Result<Self> {
        let timeout = std::env::var("COPERNICUSMARINE_HTTPS_TIMEOUT")
            .ok()
            .and_then(|t| t.parse::<f64>().ok())
            .unwrap_or(60.0);
        let client = reqwest::Client::builder()
            .user_agent(format!("copernicusmarine-rs/{}", crate::VERSION))
            .connect_timeout(Duration::from_secs_f64(timeout))
            .read_timeout(Duration::from_secs_f64(timeout))
            .build()?;
        let mut query_params = vec![
            (
                "x-cop-client".to_string(),
                "copernicus-marine-toolbox-rust".to_string(),
            ),
            (
                "x-cop-client-version".to_string(),
                crate::VERSION.to_string(),
            ),
        ];
        if let Some(username) = username {
            query_params.push(("x-cop-user".to_string(), username.to_string()));
        }
        Ok(Self {
            client,
            query_params,
            semaphore: Arc::new(Semaphore::new(max_concurrent_requests.max(1))),
        })
    }

    pub fn with_username(&self, username: &str) -> Self {
        let mut new = self.clone();
        new.query_params.retain(|(k, _)| k != "x-cop-user");
        new.query_params
            .push(("x-cop-user".to_string(), username.to_string()));
        new
    }

    /// GET with retries. Returns `None` when the object does not exist
    /// (S3 answers 403 or 404 for missing keys on public buckets).
    pub async fn get_bytes_optional(&self, url: &str) -> Result<Option<Bytes>> {
        let _permit = self.semaphore.acquire().await?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self.client.get(url).query(&self.query_params).send().await;
            match result {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        match read_body(response).await {
                            Ok(bytes) => return Ok(Some(bytes)),
                            Err(e) if attempt < MAX_RETRIES => {
                                debug!("Error reading body of {url}: {e:#}, retrying");
                                // A stalled transfer is retried right away
                                continue;
                            }
                            Err(e) => return Err(e).context(format!("GET {url}")),
                        }
                    } else if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        debug!("GET {url} -> {status}");
                        return Ok(None);
                    } else if (status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS)
                        && attempt < MAX_RETRIES
                    {
                        debug!("GET {url} -> {status}, retrying");
                    } else {
                        bail!("GET {url} failed with status {status}");
                    }
                }
                Err(e) if attempt < MAX_RETRIES => {
                    debug!("GET {url} failed: {e}, retrying");
                }
                Err(e) => return Err(e).context(format!("GET {url}")),
            }
            tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt - 1))).await;
        }
    }

    pub async fn get_bytes(&self, url: &str) -> Result<Bytes> {
        self.get_bytes_optional(url)
            .await?
            .with_context(|| format!("Resource not found: {url}"))
    }

    pub async fn get_json(&self, url: &str) -> Result<Value> {
        let bytes = self.get_bytes(url).await?;
        serde_json::from_slice(&bytes).with_context(|| format!("Invalid JSON at {url}"))
    }

    /// Streaming GET used to download large files. The caller is responsible
    /// for the retry logic since the body is consumed progressively.
    pub async fn get_stream(&self, url: &str) -> Result<reqwest::Response> {
        let _permit = self.semaphore.acquire().await?;
        let response = self
            .client
            .get(url)
            .query(&self.query_params)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !response.status().is_success() {
            bail!("GET {url} failed with status {}", response.status());
        }
        Ok(response)
    }

    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.semaphore.clone()
    }
}
