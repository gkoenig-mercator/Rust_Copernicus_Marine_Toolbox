//! `get` command: download the original (native) files of a dataset.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures::StreamExt;
use log::{debug, info};
use regex::Regex;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::catalogue::get_dataset_metadata;
use crate::config::fetch_config;
use crate::credentials::get_and_check_user;
use crate::http::Http;
use crate::utils::{size_to_mb, unique_path};

#[derive(Debug, Clone)]
pub struct GetOptions {
    pub dataset_id: String,
    pub dataset_version: Option<String>,
    pub dataset_part: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub credentials_file: Option<PathBuf>,
    pub output_directory: PathBuf,
    pub no_directories: bool,
    pub filter: Option<String>,
    pub regex: Option<String>,
    pub file_list: Option<PathBuf>,
    pub create_file_list: Option<String>,
    pub overwrite: bool,
    pub skip_existing: bool,
    pub dry_run: bool,
    pub max_concurrent_requests: usize,
    pub show_progress: bool,
    pub staging: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileGet {
    pub s3_url: String,
    pub https_url: String,
    pub file_size: f64,
    pub last_modified_datetime: String,
    pub etag: String,
    pub file_format: String,
    pub output_directory: PathBuf,
    pub filename: String,
    pub file_path: PathBuf,
    pub file_status: String,
}

#[derive(Debug, Serialize)]
pub struct GetResponse {
    pub files: Vec<FileGet>,
    pub files_not_found: Vec<String>,
    pub number_of_files_to_download: usize,
    pub total_size: f64,
    pub status: String,
    pub message: String,
}

struct S3Object {
    key: String,
    size: u64,
    last_modified: String,
    etag: String,
}

/// Split "https://host/bucket/prefix" into ("https://host", "bucket", "prefix").
fn split_s3_url(url: &str) -> Result<(String, String, String)> {
    let (scheme, rest) = url.split_once("://").context("Invalid URL")?;
    let mut parts = rest.splitn(3, '/');
    let host = parts.next().context("Invalid URL")?;
    let bucket = parts.next().context("Invalid URL: no bucket")?;
    let prefix = parts.next().unwrap_or_default();
    Ok((
        format!("{scheme}://{host}"),
        bucket.to_string(),
        prefix.to_string(),
    ))
}

async fn list_objects(
    http: &Http,
    endpoint: &str,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<S3Object>> {
    let mut objects = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let mut url = reqwest::Url::parse(&format!("{endpoint}/{bucket}"))?;
        url.query_pairs_mut()
            .append_pair("list-type", "2")
            .append_pair("prefix", prefix);
        if let Some(token) = &continuation {
            url.query_pairs_mut()
                .append_pair("continuation-token", token);
        }
        let body = http.get_bytes(url.as_str()).await?;
        let text = String::from_utf8_lossy(&body);
        let document = roxmltree::Document::parse(&text).context("Invalid S3 listing")?;
        let root = document.root_element();
        let child_text = |node: roxmltree::Node, name: &str| -> Option<String> {
            node.children()
                .find(|c| c.tag_name().name() == name)
                .and_then(|c| c.text())
                .map(str::to_string)
        };
        for contents in root
            .children()
            .filter(|c| c.tag_name().name() == "Contents")
        {
            let Some(key) = child_text(contents, "Key") else {
                continue;
            };
            if key.ends_with('/') {
                continue;
            }
            objects.push(S3Object {
                key,
                size: child_text(contents, "Size")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                last_modified: child_text(contents, "LastModified").unwrap_or_default(),
                etag: child_text(contents, "ETag").unwrap_or_default(),
            });
        }
        let truncated = child_text(root, "IsTruncated").as_deref() == Some("true");
        continuation = child_text(root, "NextContinuationToken");
        if !truncated || continuation.is_none() {
            break;
        }
        debug!("Listed {} files so far...", objects.len());
    }
    Ok(objects)
}

/// Translate a shell glob (Python fnmatch) to a regex matching the end of the path.
fn glob_to_regex(glob: &str) -> String {
    let mut regex = String::from("(?s:");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '[' => {
                let mut class = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == ']' && !class.is_empty() {
                        closed = true;
                        break;
                    }
                    class.push(c);
                }
                if closed {
                    let class = class
                        .strip_prefix('!')
                        .map(|c| format!("^{c}"))
                        .unwrap_or(class);
                    regex.push_str(&format!("[{}]", class.replace('\\', "\\\\")));
                } else {
                    regex.push_str(&regex::escape(&format!("[{class}")));
                }
            }
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex.push_str(")\\z");
    regex
}

fn build_regex(options: &GetOptions) -> Result<Option<Regex>> {
    let mut patterns = Vec::new();
    if let Some(regex) = &options.regex {
        patterns.push(regex.clone());
    }
    if let Some(filter) = &options.filter {
        patterns.push(glob_to_regex(filter));
    }
    if let Some(file_list) = &options.file_list {
        let content = std::fs::read_to_string(file_list)
            .with_context(|| format!("Cannot read file list {}", file_list.display()))?;
        let names: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(regex::escape)
            .collect();
        if !names.is_empty() {
            patterns.push(names.join("|"));
        }
    }
    if patterns.is_empty() {
        return Ok(None);
    }
    Ok(Some(Regex::new(&format!("({})", patterns.join(")|(")))?))
}

fn local_path(key: &str, output_directory: &Path, no_directories: bool) -> PathBuf {
    if no_directories {
        output_directory.join(key.rsplit('/').next().unwrap_or(key))
    } else {
        // key: native/<product-id>/<dataset-id>/... -> <product-id>/<dataset-id>/...
        output_directory.join(key.split_once('/').map_or(key, |(_, rest)| rest))
    }
}

async fn download_file(
    http: &Http,
    url: &str,
    path: &Path,
    progress: &indicatif::ProgressBar,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension(format!(
        "{}.part",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    let mut attempt = 0;
    loop {
        attempt += 1;
        let result: Result<()> = async {
            let response = http.get_stream(url).await?;
            let mut file = tokio::fs::File::create(&tmp).await?;
            let mut stream = response.bytes_stream();
            let mut written = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(e) => {
                        progress.dec(written);
                        return Err(e.into());
                    }
                };
                file.write_all(&chunk).await?;
                written += chunk.len() as u64;
                progress.inc(chunk.len() as u64);
            }
            file.flush().await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => break,
            Err(e) if attempt < 3 => debug!("Download of {url} failed ({e:#}), retrying"),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e);
            }
        }
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

pub async fn get(options: &GetOptions) -> Result<GetResponse> {
    let user = get_and_check_user(
        options.username.clone(),
        options.password.clone(),
        options.credentials_file.as_deref(),
    )
    .await?;
    let http = Http::new(Some(&user), options.max_concurrent_requests)?;
    let config = fetch_config(&http, options.staging).await?;
    let dataset = get_dataset_metadata(&http, &config, &options.dataset_id).await?;
    let version = dataset.get_version(options.dataset_version.as_deref())?;
    let part = version.get_part(options.dataset_part.as_deref())?;
    info!(
        "Selected dataset version: \"{}\", part: \"{}\"",
        version.label, part.name
    );
    let service = part
        .services
        .iter()
        .find(|s| s.service_name == "original-files")
        .context("No original files service ('files') available for this dataset")?;
    let (endpoint, bucket, prefix) = split_s3_url(&service.uri)?;
    let prefix = format!("{}/", prefix.trim_end_matches('/'));

    info!("Listing files on remote server...");
    let objects = list_objects(&http, &endpoint, &bucket, &prefix).await?;
    let regex = build_regex(options)?;
    let selected: Vec<&S3Object> = objects
        .iter()
        .filter(|o| {
            regex
                .as_ref()
                .is_none_or(|r| r.is_match(&format!("s3://{bucket}/{}", o.key)))
        })
        .collect();

    let files_not_found = match &options.file_list {
        Some(file_list) => std::fs::read_to_string(file_list)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .filter(|name| !selected.iter().any(|o| o.key.ends_with(name)))
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    };

    if let Some(file_list_name) = &options.create_file_list {
        let path = options.output_directory.join(file_list_name);
        std::fs::create_dir_all(&options.output_directory)?;
        let content: String = if file_list_name.ends_with(".csv") {
            let mut c = String::from("filename,size,last_modified_datetime,etag\n");
            for o in &selected {
                c.push_str(&format!(
                    "s3://{bucket}/{},{},{},{}\n",
                    o.key, o.size, o.last_modified, o.etag
                ));
            }
            c
        } else {
            selected
                .iter()
                .map(|o| format!("s3://{bucket}/{}\n", o.key))
                .collect()
        };
        std::fs::write(&path, content)?;
        info!("The file list is written at {}", path.display());
        return Ok(GetResponse {
            files: Vec::new(),
            files_not_found,
            number_of_files_to_download: selected.len(),
            total_size: size_to_mb(selected.iter().map(|o| o.size).sum()),
            status: "000".to_string(),
            message: "The request was successful.".to_string(),
        });
    }

    let mut files = Vec::new();
    for o in &selected {
        let mut path = local_path(&o.key, &options.output_directory, options.no_directories);
        let mut status = "DOWNLOADED";
        if path.exists() {
            if options.skip_existing {
                status = "IGNORED";
            } else if options.overwrite {
                status = "OVERWRITTEN";
            } else {
                path = unique_path(&path);
            }
        }
        files.push(FileGet {
            s3_url: format!("s3://{bucket}/{}", o.key),
            https_url: format!("{endpoint}/{bucket}/{}", o.key),
            file_size: size_to_mb(o.size),
            // Same format as the Python toolbox: 2024-07-07T01:14:34.242000+00:00
            last_modified_datetime: chrono::DateTime::parse_from_rfc3339(&o.last_modified)
                .map(|d| {
                    d.with_timezone(&chrono::Utc)
                        .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
                        .to_string()
                })
                .unwrap_or_else(|_| o.last_modified.clone()),
            etag: o.etag.clone(),
            file_format: path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default(),
            output_directory: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            filename: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            file_path: path,
            file_status: status.to_string(),
        });
    }
    let to_download: Vec<FileGet> = files
        .iter()
        .filter(|f| f.file_status != "IGNORED")
        .cloned()
        .collect();
    let total_bytes: u64 = selected
        .iter()
        .zip(&files)
        .filter(|(_, f)| f.file_status != "IGNORED")
        .map(|(o, _)| o.size)
        .sum();
    let mut response = GetResponse {
        number_of_files_to_download: to_download.len(),
        total_size: size_to_mb(total_bytes),
        files_not_found,
        files: Vec::new(),
        status: "000".to_string(),
        message: "The request was successful.".to_string(),
    };
    if selected.is_empty() {
        log::warn!("No data to download");
        response.status = "003".to_string();
        response.message =
            "No data to download from the remote server corresponding to your request.".to_string();
        return Ok(response);
    }
    if options.dry_run {
        response.status = "001".to_string();
        response.message =
            "The request was run with the dry-run option. No data was downloaded.".to_string();
        response.files = files;
        return Ok(response);
    }

    info!(
        "Downloading {} files ({:.2} MB)",
        response.number_of_files_to_download, response.total_size
    );
    let progress = indicatif::ProgressBar::new(total_bytes);
    if options.show_progress {
        progress.set_style(
            indicatif::ProgressStyle::with_template(
                "{bar:40} {bytes}/{total_bytes} [{elapsed}<{eta}, {bytes_per_sec}]",
            )
            .unwrap(),
        );
    } else {
        progress.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }
    let mut downloads = futures::stream::iter(to_download.iter().map(|f| {
        let progress = progress.clone();
        let http = http.clone();
        async move { download_file(&http, &f.https_url, &f.file_path, &progress).await }
    }))
    .buffer_unordered(options.max_concurrent_requests.max(1));
    while let Some(result) = downloads.next().await {
        result?;
    }
    progress.finish_and_clear();
    response.files = files;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_filter() {
        let r = Regex::new(&glob_to_regex("*_2023-0[1-3]*.nc")).unwrap();
        assert!(r.is_match("s3://b/native/P/D/file_2023-02-01.nc"));
        assert!(!r.is_match("s3://b/native/P/D/file_2023-05-01.nc"));
    }

    #[test]
    fn local_paths() {
        let p = local_path(
            "native/PROD/DS_202406/2022/06/f.nc",
            Path::new("out"),
            false,
        );
        assert_eq!(p, PathBuf::from("out/PROD/DS_202406/2022/06/f.nc"));
        let p = local_path("native/PROD/DS_202406/2022/06/f.nc", Path::new("out"), true);
        assert_eq!(p, PathBuf::from("out/f.nc"));
    }
}
