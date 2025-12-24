use bytes::Bytes;
use eyre::Result;
use futures_util::StreamExt;
use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use std::{env, path::Path, sync::OnceLock, time::Duration};
use tokio::{fs, io::AsyncWriteExt};
use tokio::io::{BufReader, BufWriter};
use tokio_util::io::ReaderStream;

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 60;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 5;
const DEFAULT_METADATA_TIMEOUT_SECS: u64 = 2;
const DEFAULT_RETRIES: usize = 3;
const DEFAULT_MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct SnapshotHttpConfig {
    http_timeout_secs: u64,
    connect_timeout_secs: u64,
    metadata_timeout_secs: u64,
    retries: usize,
    max_bytes: u64,
}

static HTTP_CONFIG: OnceLock<SnapshotHttpConfig> = OnceLock::new();

fn http_config() -> SnapshotHttpConfig {
    *HTTP_CONFIG.get_or_init(|| SnapshotHttpConfig {
        http_timeout_secs: env_u64("RETH_SNAPSHOT_HTTP_TIMEOUT_SECS").unwrap_or(DEFAULT_HTTP_TIMEOUT_SECS),
        connect_timeout_secs: env_u64("RETH_SNAPSHOT_HTTP_CONNECT_TIMEOUT_SECS")
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
        metadata_timeout_secs: env_u64("RETH_SNAPSHOT_METADATA_TIMEOUT_SECS")
            .unwrap_or(DEFAULT_METADATA_TIMEOUT_SECS),
        retries: env_u64("RETH_SNAPSHOT_HTTP_RETRIES").unwrap_or(DEFAULT_RETRIES as u64) as usize,
        max_bytes: env_u64("RETH_SNAPSHOT_MAX_BYTES").unwrap_or(DEFAULT_MAX_SNAPSHOT_BYTES),
    })
}

#[derive(Debug, Deserialize)]
struct MetadataTokenResponse {
    access_token: String,
}

static TOKEN_CACHE: OnceLock<tokio::sync::Mutex<Option<String>>> = OnceLock::new();

fn token_cache() -> &'static tokio::sync::Mutex<Option<String>> {
    TOKEN_CACHE.get_or_init(|| tokio::sync::Mutex::new(None))
}

async fn fetch_access_token(client: &Client) -> Result<String> {
    let meta_timeout = http_config().metadata_timeout_secs;
    let resp = client
        .get(METADATA_TOKEN_URL)
        .header("Metadata-Flavor", "Google")
        .timeout(Duration::from_secs(meta_timeout))
        .send()
        .await?
        .error_for_status()?;

    let token = resp.json::<MetadataTokenResponse>().await?;

    Ok(token.access_token)
}

async fn get_access_token_cached(client: &Client) -> Result<String> {
    {
        let guard = token_cache().lock().await;
        if let Some(token) = guard.as_ref() {
            return Ok(token.clone())
        }
    }

    let token = fetch_access_token(client).await?;
    let mut guard = token_cache().lock().await;
    *guard = Some(token.clone());
    Ok(token)
}

async fn invalidate_token_cache() {
    let mut guard = token_cache().lock().await;
    *guard = None;
}

fn env_u64(key: &str) -> Option<u64> {
    env::var(key).ok().and_then(|v| v.parse::<u64>().ok())
}

fn is_retryable_reqwest_error(err: &eyre::Report) -> bool {
    if let Some(err) = err.downcast_ref::<reqwest::Error>() {
        err.is_timeout() || err.is_connect()
    } else {
        false
    }
}

fn encode_object_name(object: &str) -> String {
    let mut out = String::with_capacity(object.len());
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for b in object.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

pub fn default_client() -> Result<Client> {
    let cfg = http_config();
    Ok(Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_secs))
        .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
        .build()
        .map_err(eyre::Report::new)?)
}

pub async fn download_to_path(
    client: &Client,
    bucket: &str,
    object: &str,
    dest: &Path,
) -> Result<bool> {
    let cfg = http_config();
    let max_bytes = cfg.max_bytes;

    let encoded_object = encode_object_name(object);
    let url = format!(
        "https://storage.googleapis.com/storage/v1/b/{}/o/{}?alt=media",
        bucket, encoded_object
    );

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }

    let tmp_path = dest.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp_path).await;

    let retries = cfg.retries;
    let mut delay = Duration::from_millis(200);
    for attempt in 1..=retries {
        let token = match get_access_token_cached(client).await {
            Ok(token) => token,
            Err(err) if attempt < retries && is_retryable_reqwest_error(&err) => {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                continue;
            }
            Err(err) => return Err(err),
        };

        let resp = match client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                let err: eyre::Report = err.into();
                if attempt < retries && is_retryable_reqwest_error(&err) {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }
                return Err(err)
            }
        };

        if resp.status() == StatusCode::NOT_FOUND {
            let _ = fs::remove_file(&tmp_path).await;
            return Ok(false)
        }

        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            invalidate_token_cache().await;
            if attempt < retries {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                continue;
            }
        }

        if resp.status() == StatusCode::TOO_MANY_REQUESTS || resp.status().is_server_error() {
            if attempt < retries {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                continue;
            }
        }

        let content_length = resp.content_length();
        if let Some(expected) = content_length {
            if expected > max_bytes {
                eyre::bail!(
                    "gcs snapshot too large: {expected} bytes exceeds limit {max_bytes} (RETH_SNAPSHOT_MAX_BYTES)"
                )
            }
        }
        let resp = resp.error_for_status()?;

        let file = fs::File::create(&tmp_path).await?;
        let mut file = BufWriter::with_capacity(8 * 1024 * 1024, file);
        let mut written: u64 = 0;
        let mut stream = resp.bytes_stream();
        let mut stream_err: Option<eyre::Report> = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    let chunk: Bytes = chunk;
                    written = written.saturating_add(chunk.len() as u64);
                    if written > max_bytes {
                        stream_err = Some(eyre::eyre!(
                            "gcs snapshot too large: exceeded limit {max_bytes} bytes (RETH_SNAPSHOT_MAX_BYTES)"
                        ));
                        break;
                    }
                    file.write_all(&chunk).await?;
                }
                Err(err) => {
                    stream_err = Some(err.into());
                    break;
                }
            }
        }
        file.flush().await?;

        if let Some(err) = stream_err {
            let _ = fs::remove_file(&tmp_path).await;
            if attempt < retries && is_retryable_reqwest_error(&err) {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                continue;
            }
            return Err(err)
        }

        if let Some(expected) = content_length {
            if written != expected {
                let _ = fs::remove_file(&tmp_path).await;
                eyre::bail!("gcs download size mismatch: expected {expected} bytes, got {written}")
            }
        }

        fs::rename(&tmp_path, dest).await?;
        return Ok(true)
    }

    eyre::bail!("gcs download failed after retries")
}

pub async fn upload_from_path(client: &Client, bucket: &str, object: &str, src: &Path) -> Result<()> {
    let encoded_object = encode_object_name(object);
    let url = format!(
        "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}"
        ,
        bucket, encoded_object
    );

    let cfg = http_config();
    let max_bytes = cfg.max_bytes;

    let meta = fs::metadata(src).await?;
    let content_length = meta.len();
    if content_length > max_bytes {
        eyre::bail!(
            "snapshot file too large: {content_length} bytes exceeds limit {max_bytes} (RETH_SNAPSHOT_MAX_BYTES)"
        )
    }

    let content_length_header = content_length.to_string();

    let retries = cfg.retries;
    let mut delay = Duration::from_millis(200);
    for attempt in 1..=retries {
        let token = match get_access_token_cached(client).await {
            Ok(token) => token,
            Err(err) if attempt < retries && is_retryable_reqwest_error(&err) => {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                continue;
            }
            Err(err) => return Err(err),
        };

        let file = fs::File::open(src).await?;
        let file = BufReader::with_capacity(8 * 1024 * 1024, file);
        let stream = ReaderStream::with_capacity(file, 8 * 1024 * 1024);
        let body = reqwest::Body::wrap_stream(stream);

        let resp = match client
            .post(&url)
            .bearer_auth(token)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, content_length_header.clone())
            .body(body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                let err: eyre::Report = err.into();
                if attempt < retries && is_retryable_reqwest_error(&err) {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }
                return Err(err)
            }
        };

        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            invalidate_token_cache().await;
            if attempt < retries {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                continue;
            }
        }

        if resp.status() == StatusCode::TOO_MANY_REQUESTS || resp.status().is_server_error() {
            if attempt < retries {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                continue;
            }
        }

        resp.error_for_status()?;
        return Ok(())
    }

    eyre::bail!("gcs upload failed after retries")
}
