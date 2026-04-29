mod auth;
mod aws;
mod drive;

use anyhow::Result;
use chrono::Utc;
use drive::DriveFile;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Client;
use std::time::Duration;

const DRIVE_FOLDER_NAME: &str = "Takeout";
const MAX_RETRIES: u32 = 3;

async fn retry<F, Fut, T>(op: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = Duration::from_secs(5);
    for attempt in 1..=MAX_RETRIES {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt == MAX_RETRIES {
                    let msg = e.to_string();
                    if msg.contains("RequestTimeTooSkewed") {
                        return Err(e.context(
                            "AWS clock skew — sync your system clock with: sudo sntp -sS time.apple.com",
                        ));
                    }
                    return Err(e);
                }
                eprintln!("  attempt {attempt}/{MAX_RETRIES} failed: {e:#} — retrying in {}s ...", delay.as_secs());
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
    unreachable!()
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let test_mode = std::env::args().any(|a| a == "--test");

    let creds_file = std::env::var("GOOGLE_CREDENTIALS_FILE")
        .unwrap_or_else(|_| "credentials.json".to_string());
    let token_file = std::env::var("GOOGLE_TOKEN_FILE")
        .unwrap_or_else(|_| "token.json".to_string());
    let bucket = std::env::var("S3_BUCKET_NAME").expect("S3_BUCKET_NAME must be set");
    let role_arn = std::env::var("AWS_UPLOAD_ROLE_ARN").expect("AWS_UPLOAD_ROLE_ARN must be set");

    let http = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        // Allow up to 30 minutes per request to accommodate large file downloads.
        .timeout(Duration::from_secs(1800))
        .build()?;

    let date_prefix = Utc::now().format("%Y-%m-%d").to_string();

    // In test mode skip Google Drive entirely and generate a small local file.
    // In normal mode authenticate, find the folder, and list files from Drive.
    let tmp_dir = tempfile::tempdir()?;
    let (files, mut drive_client, mut google_token) = if test_mode {
        println!("Test mode: skipping Google Drive, using local test file.");

        let path = tmp_dir.path().join("test-upload.txt");
        tokio::fs::write(
            &path,
            format!(
                "google-photos-backup test file\nTimestamp: {}\n",
                Utc::now().to_rfc3339()
            ),
        )
        .await?;
        let size = tokio::fs::metadata(&path).await?.len();

        let fake_file = DriveFile {
            id: "test-id".to_string(),
            name: "test-upload.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some(size.to_string()),
            local_path: Some(path),
        };

        (vec![fake_file], None, None)
    } else {
        println!("Authenticating with Google Drive ...");
        let token = auth::load_or_authenticate(&http, &creds_file, &token_file).await?;
        let drive = drive::DriveClient::new(&http, token.access_token.clone());

        println!("Looking up folder \"{DRIVE_FOLDER_NAME}\" ...");
        let folder_id = drive.find_folder(DRIVE_FOLDER_NAME).await?;

        println!("Listing files ...");
        let all_files = drive.list_files(&folder_id).await?;

        let (workspace, files): (Vec<_>, Vec<_>) =
            all_files.into_iter().partition(drive::is_workspace_file);

        if !workspace.is_empty() {
            println!(
                "Skipping {} Google Workspace file(s) (not downloadable as binary):",
                workspace.len()
            );
            for f in &workspace {
                println!("  - {} ({})", f.name, f.mime_type);
            }
            println!();
        }

        (files, Some(drive), Some(token))
    };

    println!(
        "Found {} file(s) to back up under s3://{bucket}/{date_prefix}/\n",
        files.len()
    );

    println!("Assuming upload role ...");
    let s3 = aws::S3Uploader::new(bucket.clone(), &role_arn).await?;

    let total = files.len();
    let (mut uploaded, mut failed, mut not_deleted) = (0usize, 0usize, 0usize);

    let mp = MultiProgress::new();

    let overall = mp.add(ProgressBar::new(total as u64));
    overall.set_style(
        ProgressStyle::with_template("[{pos}/{len}] {bar:40.green/white} {msg}")?
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    overall.set_message("starting ...");

    let dl_style = ProgressStyle::with_template(
        "  ↓  {bar:30.cyan/white} {bytes}/{total_bytes} at {bytes_per_sec} eta {eta}",
    )?
    .progress_chars("█▉▊▋▌▍▎▏ ");

    let spinner_style =
        ProgressStyle::with_template("  {spinner:.yellow}  {msg}")?.tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ");

    for (i, file) in files.iter().enumerate() {
        overall.set_message(file.name.clone());

        // Refresh the Google token before each file in case it expired mid-run.
        if let Some(token) = google_token.take() {
            match auth::ensure_fresh(&http, &creds_file, &token_file, token).await {
                Ok(fresh) => {
                    if let Some(ref mut drive) = drive_client {
                        drive.set_token(fresh.access_token.clone());
                    }
                    google_token = Some(fresh);
                }
                Err(e) => {
                    overall.println(format!("Warning: token refresh failed: {e:#}"));
                }
            }
        }

        // Sanitize the filename to prevent path traversal when writing to the
        // temp directory. Replace any path separator or null byte with '_'.
        let safe_name: String = file
            .name
            .chars()
            .map(|c| if matches!(c, '/' | '\\' | '\0') { '_' } else { c })
            .collect();

        let s3_key = format!("{date_prefix}/{safe_name}");

        // In test mode the file is already local; in normal mode download from Drive.
        let tmp_path = if let Some(ref local) = file.local_path {
            local.clone()
        } else {
            let path = tmp_dir.path().join(&safe_name);
            let dl_bar = mp.insert_after(&overall, ProgressBar::new(0));
            dl_bar.set_style(dl_style.clone());

            let dl_result = retry(|| async {
                dl_bar.reset();
                drive_client.as_ref().unwrap().download(file, &path, &dl_bar).await
            }).await;
            match dl_result {
                Err(e) => {
                    dl_bar.finish_and_clear();
                    overall.println(format!(
                        "[{}/{}] ✗ {} — download error: {e:#}",
                        i + 1,
                        total,
                        file.name
                    ));
                    failed += 1;
                    continue;
                }
                Ok(()) => dl_bar.finish_and_clear(),
            }
            path
        };

        // Upload with a spinner (S3 SDK doesn't expose byte-level progress).
        let spinner = mp.insert_after(&overall, ProgressBar::new_spinner());
        spinner.set_style(spinner_style.clone());
        spinner.set_message(format!("Uploading to s3://{bucket}/{s3_key}"));
        spinner.enable_steady_tick(Duration::from_millis(80));
        match retry(|| s3.upload(&s3_key, &tmp_path)).await {
            Err(e) => {
                spinner.finish_and_clear();
                overall.println(format!(
                    "[{}/{}] ✗ {} — upload error: {e:#}",
                    i + 1,
                    total,
                    file.name
                ));
                failed += 1;
                if file.local_path.is_none() {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                }
                continue;
            }
            Ok(()) => spinner.finish_and_clear(),
        }

        // Only delete from Drive after a confirmed successful S3 upload.
        // In test mode there is no Drive file to delete.
        if let Some(token) = google_token.take() {
            match auth::ensure_fresh(&http, &creds_file, &token_file, token).await {
                Ok(fresh) => {
                    if let Some(ref mut drive) = drive_client {
                        drive.set_token(fresh.access_token.clone());
                    }
                    google_token = Some(fresh);
                }
                Err(e) => {
                    overall.println(format!("Warning: token refresh failed before delete: {e:#}"));
                }
            }
        }
        if let Some(drive) = &drive_client {
            match drive.delete(&file.id).await {
                Ok(()) => {
                    overall.println(format!("[{}/{}] ✓ {}", i + 1, total, file.name));
                }
                Err(e) => {
                    overall.println(format!(
                        "[{}/{}] ✓ {} (uploaded) — warning: Drive delete failed: {e}",
                        i + 1,
                        total,
                        file.name
                    ));
                    not_deleted += 1;
                }
            }
        } else {
            overall.println(format!("[{}/{}] ✓ {}", i + 1, total, file.name));
        }

        if file.local_path.is_none() {
            let _ = tokio::fs::remove_file(&tmp_path).await;
        }
        uploaded += 1;
        overall.inc(1);
    }

    overall.finish_and_clear();

    println!("\nBackup complete: {uploaded}/{total} uploaded, {failed} failed.");
    if not_deleted > 0 {
        eprintln!(
            "Warning: {not_deleted} file(s) were archived to S3 but could not be \
             deleted from Google Drive. Check Drive manually."
        );
    }
    Ok(())
}
