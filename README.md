# google-photos-backup

Backs up your Google Photos to Amazon S3 Glacier Deep Archive — the cheapest long-term cloud storage available (~$1/TB/month). Export your library from Google Takeout, point this tool at the resulting Drive folder, and it downloads and archives every file one at a time, then deletes it from Drive once safely stored in S3.

---

## Quick Start

### Prerequisites

| Tool | Install |
|------|---------|
| Rust | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| AWS CLI | `brew install awscli` |
| AWS CDK | `npm install -g aws-cdk` |

### 1. Clone and build

```bash
git clone https://github.com/anthonydandrea/google-photos-backup.git
cd google-photos-backup
cargo build
```

### 2. Deploy AWS infrastructure

```bash
cd infra
npm install
cdk bootstrap   # first time only
cdk deploy
cd ..
```

This creates:
- An S3 bucket with an immediate Glacier Deep Archive lifecycle policy
- An IAM user (`google-photos-backup-user`) and a least-privilege upload role

### 3. Create AWS access keys

In the [AWS Console](https://console.aws.amazon.com/iam) go to **IAM → Users → google-photos-backup-user → Security credentials → Create access key**.

### 4. Set up Google Drive credentials

1. Go to [Google Cloud Console](https://console.cloud.google.com) and create a project
2. Enable the **Google Drive API** (APIs & Services → Library)
3. Configure the OAuth consent screen (External, add your email as a test user)
4. Create credentials: **OAuth client ID → Desktop app** → Download JSON
5. Save the downloaded file as `credentials.json` in the repo root

### 5. Configure environment

```bash
cp .env.example .env
```

Fill in `.env`:

```env
GOOGLE_CREDENTIALS_FILE=credentials.json
GOOGLE_TOKEN_FILE=token.json
S3_BUCKET_NAME=<BucketName from cdk deploy output>
AWS_UPLOAD_ROLE_ARN=<UploadRoleArn from cdk deploy output>
AWS_ACCESS_KEY_ID=<from step 3>
AWS_SECRET_ACCESS_KEY=<from step 3>
```

### 6. Authenticate with Google (first run only)

```bash
cargo run
```

A browser window will open for Google OAuth consent. After approving, `token.json` is saved and future runs are fully automatic.

---

## Usage

**Normal backup:**
```bash
cargo run
```

**Test the S3 upload path without touching Google Drive:**
```bash
cargo run -- --test
```

The backup downloads and uploads one file at a time with a live progress bar showing download speed and ETA. After each successful S3 upload the file is deleted from Google Drive.

Files are stored in S3 under a date-stamped prefix:
```
s3://<bucket>/2026-02-22/takeout-20260222T210156Z-001.zip
```

---

## Scheduling (macOS cron)

Authenticate manually at least once to generate `token.json`, then add this to your crontab (`crontab -e`):

```cron
0 2 1 1,3,5,7,9,11 * cd /path/to/google-photos-backup && /Users/<you>/.cargo/bin/cargo run >> /tmp/google-photos-backup.log 2>&1
```

This runs at 2 AM on the 1st of every odd month (January, March, May, July, September, November). Adjust the schedule to taste.

**With failure alerting via [healthchecks.io](https://healthchecks.io):**
```cron
0 2 1 1,3,5,7,9,11 * cd /path/to/google-photos-backup && curl -fsS https://hc-ping.com/<your-uuid>/start && /Users/<you>/.cargo/bin/cargo run >> /tmp/google-photos-backup.log 2>&1 && curl -fsS https://hc-ping.com/<your-uuid> || curl -fsS https://hc-ping.com/<your-uuid>/fail
```

The job can run while the machine is logged out as long as it is not sleeping.

---

## How It Works

1. **Authentication** — On first run, opens a browser for Google OAuth2 consent and saves a refresh token to `token.json`. Subsequent runs silently refresh the token; no browser needed.

2. **Discovery** — Lists all files in the Google Drive folder named `Takeout`, skipping any Google Workspace native files (Docs, Sheets, etc.) that cannot be downloaded as binary.

3. **Transfer loop** — For each file:
   - Downloads the file to a temporary directory, verifying the byte count against the Drive-reported size to catch truncated downloads
   - Uploads to S3 — files ≤ 100 MB via a single `PutObject`, larger files via multipart upload in 64 MB chunks (supports files well beyond the 5 GB single-PUT limit)
   - Deletes from Google Drive only after the S3 upload is confirmed

4. **Cleanup** — The temporary file is deleted from disk after each successful upload.

---

## AWS Infrastructure

Managed with AWS CDK (TypeScript) in the `infra/` directory.

| Resource | Details |
|----------|---------|
| S3 Bucket | SSE-S3 encryption, public access blocked, SSL enforced, versioning enabled, `RETAIN` on stack deletion |
| Lifecycle rule | All objects immediately transition to `DEEP_ARCHIVE` |
| IAM User | `google-photos-backup-user` — holds the long-term access key used to assume the upload role |
| IAM Role | Trusted only by the backup user; allows `s3:PutObject` on the bucket only; max session 12 hours |

**Redeploy after infra changes:**
```bash
cd infra && cdk deploy
```

---

## Security

- **OAuth tokens** are written atomically and stored at `0600` permissions (owner read/write only)
- **CSRF protection** — a random state token is generated for each OAuth flow and validated on the callback
- **Download integrity** — byte count is verified against Drive metadata before any upload attempt
- **Filename sanitization** — path separators are stripped from Drive filenames before writing to disk or S3
- **Least-privilege IAM** — the upload role allows only `s3:PutObject`; the IAM user can only assume that role
- **No credentials in source** — all secrets are in `.env` (gitignored) or `credentials.json` / `token.json` (gitignored)

---

## File Reference

```
.
├── src/
│   ├── main.rs        # Entry point and transfer loop
│   ├── auth.rs        # Google OAuth2 (browser flow, token refresh)
│   ├── drive.rs       # Google Drive API (list, download, delete)
│   └── aws.rs         # STS role assumption and S3 upload (multipart)
├── infra/
│   └── lib/
│       └── infra-stack.ts  # CDK stack (S3 bucket + IAM)
├── credentials.json   # Google OAuth client credentials (gitignored)
├── token.json         # Google OAuth token cache (gitignored)
└── .env               # Runtime configuration (gitignored)
```
