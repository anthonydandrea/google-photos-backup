use anyhow::Result;
use indicatif::ProgressBar;
use reqwest::Client;
use serde::Deserialize;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

const DRIVE_API: &str = "https://www.googleapis.com/drive/v3";

const WORKSPACE_MIMETYPES: &[&str] = &[
    "application/vnd.google-apps.document",
    "application/vnd.google-apps.spreadsheet",
    "application/vnd.google-apps.presentation",
    "application/vnd.google-apps.drawing",
    "application/vnd.google-apps.form",
    "application/vnd.google-apps.map",
    "application/vnd.google-apps.site",
];

#[derive(Debug, Deserialize)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    /// File size in bytes as a string, as returned by the Drive API.
    /// Absent for Google Workspace native files.
    pub size: Option<String>,
}

pub fn is_workspace_file(f: &DriveFile) -> bool {
    WORKSPACE_MIMETYPES.contains(&f.mime_type.as_str())
}

pub struct DriveClient<'a> {
    http: &'a Client,
    access_token: String,
}

impl<'a> DriveClient<'a> {
    pub fn new(http: &'a Client, access_token: String) -> Self {
        Self { http, access_token }
    }

    pub async fn find_folder(&self, name: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct FolderEntry {
            id: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            files: Vec<FolderEntry>,
        }

        let resp: Resp = self
            .http
            .get(format!("{DRIVE_API}/files"))
            .bearer_auth(&self.access_token)
            .query(&[
                (
                    "q",
                    format!("name='{name}' and mimeType='application/vnd.google-apps.folder' and trashed=false"),
                ),
                ("fields", "files(id,name)".to_string()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.files.is_empty() {
            anyhow::bail!("No folder named \"{name}\" found in Google Drive");
        }
        if resp.files.len() > 1 {
            eprintln!(
                "Warning: {} folders named \"{name}\", using the first",
                resp.files.len()
            );
        }
        Ok(resp.files.into_iter().next().unwrap().id)
    }

    pub async fn list_files(&self, folder_id: &str) -> Result<Vec<DriveFile>> {
        #[derive(Deserialize)]
        struct Resp {
            files: Vec<DriveFile>,
            #[serde(rename = "nextPageToken")]
            next_page_token: Option<String>,
        }

        let mut all = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut params = vec![
                (
                    "q".to_string(),
                    format!("'{folder_id}' in parents and trashed=false"),
                ),
                (
                    "fields".to_string(),
                    // Include size so we can verify completeness after download.
                    "nextPageToken,files(id,name,mimeType,size)".to_string(),
                ),
                ("pageSize".to_string(), "1000".to_string()),
            ];
            if let Some(ref t) = page_token {
                params.push(("pageToken".to_string(), t.clone()));
            }

            let resp: Resp = self
                .http
                .get(format!("{DRIVE_API}/files"))
                .bearer_auth(&self.access_token)
                .query(&params)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            all.extend(resp.files);
            page_token = resp.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(all)
    }

    pub async fn download(&self, file: &DriveFile, dest: &Path, bar: &ProgressBar) -> Result<()> {
        let mut response = self
            .http
            .get(format!("{DRIVE_API}/files/{}", file.id))
            .bearer_auth(&self.access_token)
            .query(&[("alt", "media")])
            .send()
            .await?
            .error_for_status()?;

        if let Some(expected) = file.size.as_deref().and_then(|s| s.parse::<u64>().ok()) {
            bar.set_length(expected);
        }

        let mut f = File::create(dest).await?;
        let mut bytes_written: u64 = 0;
        while let Some(chunk) = response.chunk().await? {
            bytes_written += chunk.len() as u64;
            bar.set_position(bytes_written);
            f.write_all(&chunk).await?;
        }
        f.flush().await?;

        // Verify the downloaded byte count against the size reported by Drive.
        // This catches truncated downloads before we attempt to upload them.
        if let Some(expected) = file.size.as_deref().and_then(|s| s.parse::<u64>().ok()) {
            if bytes_written != expected {
                // Remove the incomplete file so we don't leave garbage behind.
                let _ = tokio::fs::remove_file(dest).await;
                anyhow::bail!(
                    "Incomplete download: expected {expected} bytes, received {bytes_written} bytes"
                );
            }
        }

        Ok(())
    }

    pub async fn delete(&self, file_id: &str) -> Result<()> {
        self.http
            .delete(format!("{DRIVE_API}/files/{file_id}"))
            .bearer_auth(&self.access_token)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
