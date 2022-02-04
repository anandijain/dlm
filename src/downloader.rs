use hyper::HeaderMap;
use indicatif::ProgressBar;
use reqwest::Client;
use std::path::Path;
use tokio::fs as tfs;
use tokio::io::AsyncWriteExt;
use tokio::time::{timeout, Duration};

use crate::dlm_error::DlmError;
use crate::file_link::FileLink;
use crate::utils::pretty_bytes_size;
use crate::ProgressBarManager;

const NO_EXTENSION: &str = "NO_EXTENSION_FOUND";

pub async fn download_link(
    raw_link: &str,
    client: &Client,
    output_dir: &str,
    pb_dl: &ProgressBar,
    pb_manager: &ProgressBarManager,
) -> Result<String, DlmError> {
    let file_link = FileLink::new(raw_link.to_string())?;
    // validate file extension, necessary when the URL does not contain clearly the filename (in case of a redirect for instance)
    let (extension, filename_without_extension) =
        check_filename_extension(&file_link, client, pb_manager).await?;
    let filename_with_extension = format!("{}.{}", filename_without_extension, extension);
    let final_file_path = &format!("{}/{}", output_dir, filename_with_extension);
    if Path::new(final_file_path).exists() {
        let final_file_size = tfs::File::open(final_file_path)
            .await?
            .metadata()
            .await?
            .len();
        let msg = format!(
            "Skipping {} because the file is already completed [{}]",
            filename_with_extension,
            pretty_bytes_size(final_file_size)
        );
        Ok(msg)
    } else {
        let url = file_link.url.as_str();
        let head_result = client.head(url).send().await?;
        if !head_result.status().is_success() {
            let message = format!("{} {}", url, head_result.status());
            Err(DlmError::ResponseStatusNotSuccess { message })
        } else {
            let (content_length, accept_ranges) =
                try_hard_to_extract_headers(head_result.headers(), url, client).await?;
            // setup progress bar for the file
            pb_dl.set_message(ProgressBarManager::message_progress_bar(
                &filename_with_extension,
            ));
            if let Some(total_size) = content_length {
                pb_dl.set_length(total_size);
            };

            let tmp_name = format!("{}/{}.part", output_dir, filename_without_extension);
            let query_range =
                compute_query_range(pb_dl, pb_manager, content_length, accept_ranges, &tmp_name)
                    .await?;

            // create/open file.part
            let mut file = match query_range {
                Some(_) => {
                    tfs::OpenOptions::new()
                        .append(true)
                        .create(false)
                        .open(&tmp_name)
                        .await?
                }
                None => tfs::File::create(&tmp_name).await?,
            };

            // building the request
            let mut request = client.get(url);
            if let Some(range) = query_range {
                request = request.header("Range", range)
            }

            // initiate file download
            let mut dl_response = request.send().await?;
            if !dl_response.status().is_success() {
                let message = format!("{} {}", url, dl_response.status());
                Err(DlmError::ResponseStatusNotSuccess { message })
            } else {
                // incremental save chunk by chunk into part file
                let chunk_timeout = Duration::from_secs(60);
                while let Some(chunk) = timeout(chunk_timeout, dl_response.chunk()).await?? {
                    file.write_all(&chunk).await?;
                    pb_dl.inc(chunk.len() as u64);
                }
                let final_file_size = file.metadata().await?.len();
                // rename part file to final
                tfs::rename(&tmp_name, final_file_path).await?;
                let msg = format!(
                    "Completed {} [{}]",
                    filename_with_extension,
                    pretty_bytes_size(final_file_size)
                );
                Ok(msg)
            }
        }
    }
}

async fn try_hard_to_extract_headers(
    head_headers: &HeaderMap,
    url: &str,
    client: &Client,
) -> Result<(Option<u64>, Option<String>), DlmError> {
    let tuple = match content_length_value(head_headers) {
        Some(0) => {
            // if "content-length": "0" then it is likely the server does not support HEAD, let's try harder with a GET
            let get_result = client.get(url).send().await?;
            let get_headers = get_result.headers();
            (
                content_length_value(get_headers),
                accept_ranges_value(get_headers),
            )
        }
        ct_option @ Some(_) => (ct_option, accept_ranges_value(head_headers)),
        _ => (None, None),
    };
    Ok(tuple)
}

fn content_length_value(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("content-length")
        .and_then(|ct_len| ct_len.to_str().ok())
        .and_then(|ct_len| ct_len.parse().ok())
}

fn accept_ranges_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get("accept-ranges")
        .and_then(|ct_len| ct_len.to_str().ok())
        .map(|v| v.to_string())
}

fn content_disposition_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get("content-disposition")
        .and_then(|ct_len| ct_len.to_str().ok())
        .map(|v| v.to_string())
}

async fn compute_query_range(
    pb_dl: &ProgressBar,
    pb_manager: &ProgressBarManager,
    content_length: Option<u64>,
    accept_ranges: Option<String>,
    tmp_name: &str,
) -> Result<Option<String>, DlmError> {
    if Path::new(&tmp_name).exists() {
        // get existing file size
        let tmp_size = tfs::File::open(&tmp_name).await?.metadata().await?.len();
        match (accept_ranges, content_length) {
            (Some(range), Some(cl)) if range == "bytes" => {
                pb_dl.set_position(tmp_size);
                let range_msg = format!("bytes={}-{}", tmp_size, cl);
                Ok(Some(range_msg))
            }
            _ => {
                let log = format!(
                    "Found part file {} with size {} but it will be overridden because the server does not support resuming the download (range bytes)",
                    tmp_name, tmp_size
                );
                pb_manager.log_above_progress_bars(log);
                Ok(None)
            }
        }
    } else {
        if accept_ranges.is_none() {
            let log = format!(
                "The download of file {} should not be interrupted because the server does not support resuming the download (range bytes)",
                tmp_name
            );
            pb_manager.log_above_progress_bars(log);
        };
        Ok(None)
    }
}

async fn check_filename_extension(
    file_link: &FileLink,
    client: &Client,
    pb_manager: &ProgressBarManager,
) -> Result<(String, String), DlmError> {
    let url = &file_link.url;
    let filename_without_extension = file_link.filename_without_extension.to_owned();
    match &file_link.extension {
        Some(ext) => Ok((ext.to_owned(), filename_without_extension)),
        None => {
            // try get the file name from the HTTP headers
            let filename_header_value = compute_filename_from_headers(url, client).await?;
            match filename_header_value {
                Some(fh) => {
                    let (ext, filename) = FileLink::extract_extension_from_filename(fh);
                    match ext {
                        Some(e) => Ok((e, filename)),
                        None => {
                            // no extension extracted from header value
                            let msg = format!(
                                "Could not determine file extension based on header {} for {}",
                                filename, url
                            );
                            pb_manager.log_above_progress_bars(msg);
                            Ok((NO_EXTENSION.to_owned(), filename_without_extension))
                        }
                    }
                }
                None => {
                    // no extension found with a HEAD request - use default value
                    let msg = format!("Could not determine file extension for {}", url);
                    pb_manager.log_above_progress_bars(msg);
                    Ok((NO_EXTENSION.to_owned(), filename_without_extension))
                }
            }
        }
    }
}

async fn compute_filename_from_headers(
    url: &str,
    client: &Client,
) -> Result<Option<String>, DlmError> {
    let head_result = client.head(url).send().await?;
    if !head_result.status().is_success() {
        let message = format!("{} {}", url, head_result.status());
        Err(DlmError::ResponseStatusNotSuccess { message })
    } else {
        // https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Disposition#as_a_response_header_for_the_main_body
        let content_disposition = content_disposition_value(head_result.headers());
        Ok(content_disposition.and_then(parse_filename_header))
    }
}

fn parse_filename_header(content_disposition: String) -> Option<String> {
    content_disposition
        .split("attachment; filename=")
        .last()
        .and_then(|s| s.strip_prefix('"'))
        .and_then(|s| s.strip_suffix('"'))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod downloader_tests {
    use crate::downloader::*;

    #[test]
    fn parse_filename_header_ok() {
        let header_value = "attachment; filename=\"code-stable-x64-1639562789.tar.gz\"";
        let parsed = parse_filename_header(header_value.to_string());
        assert_eq!(parsed, Some("code-stable-x64-1639562789.tar.gz".to_owned()));
    }
}
