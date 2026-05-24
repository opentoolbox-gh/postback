//! postback — PostgreSQL backup with rolling retention → Google Drive
//!
//! Auth: Google Service Account (no browser, fully headless)
//!
//! Retention strategy
//! ──────────────────
//!  hourly : keep last 6  → oldest promoted to 6h tier
//!  6h     : keep last 4  → oldest promoted to daily tier
//!  daily  : keep last 28 → oldest deleted (4 weeks)
//!
//! Folder layout on Google Drive (all under a single parent folder):
//!   <backup_folder>/hourly/
//!   <backup_folder>/6h/
//!   <backup_folder>/daily/

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Parser;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::{
    multipart::{Form, Part},
    Client,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{info, warn};

// ─── CLI / config ────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cfg {
    /// PostgreSQL host
    #[arg(long, env = "PGHOST", default_value = "localhost")]
    db_host: String,

    /// PostgreSQL port
    #[arg(long, env = "PGPORT", default_value_t = 5432)]
    db_port: u16,

    /// Database name
    #[arg(long, env = "PGDATABASE")]
    db_name: String,

    /// Database user
    #[arg(long, env = "PGUSER")]
    db_user: String,

    /// Path to the Google service account JSON key file
    #[arg(long, env = "GOOGLE_SERVICE_ACCOUNT_KEY")]
    sa_key: PathBuf,

    /// Google Drive folder ID to store backups in
    /// (share this folder with your service account email)
    #[arg(long, env = "GDRIVE_FOLDER_ID")]
    gdrive_folder_id: String,

    /// Local temp directory for the dump before upload
    #[arg(long, env = "LOCAL_TMP_DIR", default_value = "/tmp/postback")]
    tmp_dir: PathBuf,

    /// Max hourly backups before promoting oldest to 6h tier
    #[arg(long, default_value_t = 6)]
    keep_hourly: usize,

    /// Max 6h backups before promoting oldest to daily tier
    #[arg(long, default_value_t = 4)]
    keep_6h: usize,

    /// Max daily backups to keep (28 = 4 weeks)
    #[arg(long, default_value_t = 28)]
    keep_daily: usize,

    /// Slack bot token for backup notifications (requires chat:write scope)
    #[arg(long, env = "SLACK_BOT_TOKEN")]
    slack_token: Option<String>,

    /// Slack channel ID or name to post notifications to (e.g. #alerts or C1234567890)
    #[arg(long, env = "SLACK_CHANNEL")]
    slack_channel: Option<String>,

    /// Comma-separated list of events that trigger a Slack notification.
    /// Available: start, auth, success, failure, retention
    #[arg(long, env = "SLACK_NOTIFY_ON", default_value = "success,failure")]
    slack_notify_on: String,
}

// ─── Service account & JWT ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
}

#[derive(Serialize)]
struct JwtClaims {
    iss: String,
    scope: String,
    aud: String,
    exp: u64,
    iat: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

async fn get_access_token(sa: &ServiceAccount) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs();

    let claims = JwtClaims {
        iss: sa.client_email.clone(),
        scope: "https://www.googleapis.com/auth/drive".into(),
        aud: "https://oauth2.googleapis.com/token".into(),
        exp: now + 3600,
        iat: now,
    };

    let key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
        .context("Failed to parse RSA private key from service account JSON")?;
    let jwt = encode(&Header::new(Algorithm::RS256), &claims, &key)
        .context("Failed to encode JWT")?;

    let client = Client::new();
    let resp: TokenResponse = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &jwt),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(resp.access_token)
}

// ─── Google Drive API helpers ────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
struct DriveFile {
    id: String,
    name: String,
    #[serde(rename = "createdTime")]
    created_time: Option<String>,
}

#[derive(Deserialize)]
struct FileList {
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

struct Drive {
    client: Client,
    token: String,
}

impl Drive {
    fn new(token: String) -> Self {
        Self {
            client: Client::new(),
            token,
        }
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.token)
    }

    /// Find or create a folder by name under a parent folder ID.
    async fn get_or_create_folder(&self, name: &str, parent_id: &str) -> Result<String> {
        let q = format!(
            "name = '{name}' and mimeType = 'application/vnd.google-apps.folder' \
             and '{parent_id}' in parents and trashed = false"
        );
        let resp: FileList = self
            .auth(self.client.get("https://www.googleapis.com/drive/v3/files"))
            .query(&[("q", &q), ("fields", &"files(id,name)".to_string())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if let Some(f) = resp.files.into_iter().next() {
            return Ok(f.id);
        }

        // Create it
        let body = serde_json::json!({
            "name": name,
            "mimeType": "application/vnd.google-apps.folder",
            "parents": [parent_id]
        });
        let created: DriveFile = self
            .auth(self.client.post("https://www.googleapis.com/drive/v3/files"))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        info!("Created Drive folder: {name} ({})", created.id);
        Ok(created.id)
    }

    /// List all files in a folder, sorted oldest → newest by createdTime.
    async fn list_files_sorted(&self, folder_id: &str) -> Result<Vec<DriveFile>> {
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let q = format!("'{folder_id}' in parents and trashed = false and mimeType != 'application/vnd.google-apps.folder'");
            let mut req = self
                .auth(self.client.get("https://www.googleapis.com/drive/v3/files"))
                .query(&[
                    ("q", q.as_str()),
                    ("fields", "nextPageToken,files(id,name,createdTime)"),
                    ("pageSize", "1000"),
                    ("orderBy", "createdTime"),
                ]);

            if let Some(ref token) = page_token {
                req = req.query(&[("pageToken", token.as_str())]);
            }

            let page: FileList = req.send().await?.error_for_status()?.json().await?;
            all.extend(page.files);
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(all)
    }

    /// Upload a local file into a Drive folder.
    async fn upload_file(&self, local_path: &Path, folder_id: &str) -> Result<DriveFile> {
        let filename = local_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let bytes = tokio::fs::read(local_path)
            .await
            .with_context(|| format!("Reading {}", local_path.display()))?;

        let metadata = serde_json::json!({
            "name": filename,
            "parents": [folder_id]
        });

        let form = Form::new()
            .part(
                "metadata",
                Part::text(metadata.to_string())
                    .mime_str("application/json; charset=UTF-8")?,
            )
            .part(
                "file",
                Part::bytes(bytes).mime_str("application/gzip")?,
            );

        let uploaded: DriveFile = self
            .auth(
                self.client
                    .post("https://www.googleapis.com/upload/drive/v3/files")
                    .query(&[("uploadType", "multipart"), ("fields", "id,name,createdTime")]),
            )
            .multipart(form)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(uploaded)
    }

    /// Move a file to a different folder (update its parents).
    async fn move_file(&self, file_id: &str, from_folder: &str, to_folder: &str) -> Result<()> {
        self.auth(
            self.client
                .patch(format!(
                    "https://www.googleapis.com/drive/v3/files/{file_id}"
                ))
                .query(&[
                    ("addParents", to_folder),
                    ("removeParents", from_folder),
                    ("fields", "id,parents"),
                ]),
        )
        .send()
        .await?
        .error_for_status()?;
        Ok(())
    }

    /// Permanently delete a file.
    async fn delete_file(&self, file_id: &str) -> Result<()> {
        self.auth(
            self.client.delete(format!(
                "https://www.googleapis.com/drive/v3/files/{file_id}"
            )),
        )
        .send()
        .await?
        .error_for_status()?;
        Ok(())
    }
}

// ─── Slack notifications ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SlackApiResponse {
    ok: bool,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct NotifyOn {
    start: bool,
    auth: bool,
    success: bool,
    failure: bool,
    retention: bool,
}

impl NotifyOn {
    fn from_str(s: &str) -> Self {
        let mut n = NotifyOn::default();
        for token in s.split(',').map(str::trim) {
            match token {
                "start"     => n.start = true,
                "auth"      => n.auth = true,
                "success"   => n.success = true,
                "failure"   => n.failure = true,
                "retention" => n.retention = true,
                other if !other.is_empty() => warn!("Unknown slack_notify_on value: {other}"),
                _ => {}
            }
        }
        n
    }
}

struct Slack {
    client: Client,
    token: String,
    channel: String,
}

impl Slack {
    fn from_cfg(cfg: &Cfg) -> Option<Self> {
        match (&cfg.slack_token, &cfg.slack_channel) {
            (Some(token), Some(channel)) => Some(Self {
                client: Client::new(),
                token: token.clone(),
                channel: channel.clone(),
            }),
            _ => None,
        }
    }

    async fn post(&self, payload: serde_json::Value) -> Result<()> {
        let resp: SlackApiResponse = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.token)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if !resp.ok {
            bail!(
                "Slack API error: {}",
                resp.error.unwrap_or_else(|| "unknown".into())
            );
        }
        Ok(())
    }

    async fn notify_start(&self, db: &str) -> Result<()> {
        self.post(serde_json::json!({
            "channel": self.channel,
            "attachments": [{
                "color": "#439fe0",
                "title": "postback starting \u{1f504}",
                "fields": [
                    {"title": "Database", "value": db, "short": true}
                ]
            }]
        }))
        .await
    }

    async fn notify_auth(&self, db: &str, service_account: &str) -> Result<()> {
        self.post(serde_json::json!({
            "channel": self.channel,
            "attachments": [{
                "color": "#439fe0",
                "title": "postback authenticated \u{1f511}",
                "fields": [
                    {"title": "Database",        "value": db,              "short": true},
                    {"title": "Service Account", "value": service_account, "short": true}
                ]
            }]
        }))
        .await
    }

    async fn notify_success(&self, db: &str, filename: &str, size: &str) -> Result<()> {
        self.post(serde_json::json!({
            "channel": self.channel,
            "attachments": [{
                "color": "#36a64f",
                "title": "postback succeeded ✅",
                "fields": [
                    {"title": "Database", "value": db,       "short": true},
                    {"title": "File",     "value": filename, "short": true},
                    {"title": "Size",     "value": size,     "short": true}
                ]
            }]
        }))
        .await
    }

    async fn notify_failure(&self, db: &str, error: &str) -> Result<()> {
        self.post(serde_json::json!({
            "channel": self.channel,
            "attachments": [{
                "color": "#cc0000",
                "title": "postback FAILED ❌",
                "fields": [
                    {"title": "Database", "value": db,    "short": true},
                    {"title": "Error",    "value": error, "short": false}
                ]
            }]
        }))
        .await
    }

    async fn notify_retention(&self, db: &str, actions: &[String]) -> Result<()> {
        let text = actions
            .iter()
            .map(|a| format!("\u{2022} {a}"))
            .collect::<Vec<_>>()
            .join("\n");

        self.post(serde_json::json!({
            "channel": self.channel,
            "attachments": [{
                "color": "#ff9800",
                "title": "Retention actions \u{1f504}",
                "text": text,
                "fields": [
                    {"title": "Database", "value": db, "short": true}
                ]
            }]
        }))
        .await
    }
}

// ─── Database dump ───────────────────────────────────────────────────────────

fn dump_database(cfg: &Cfg) -> Result<(PathBuf, String)> {
    std::fs::create_dir_all(&cfg.tmp_dir).context("Creating tmp dir")?;

    let timestamp = Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let filename = format!("hourly_{timestamp}.sql.gz");
    let dump_path = cfg.tmp_dir.join(&filename);

    info!("Dumping database `{}` → {}", cfg.db_name, dump_path.display());

    let cmd = format!(
        "pg_dump -h {host} -p {port} -U {user} -d {db} -Fp | gzip -9 > {out}",
        host = cfg.db_host,
        port = cfg.db_port,
        user = cfg.db_user,
        db   = cfg.db_name,
        out  = dump_path.display(),
    );

    let status = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .status()
        .context("Failed to spawn pg_dump")?;

    if !status.success() {
        bail!("pg_dump failed with status {status}");
    }

    let size = std::fs::metadata(&dump_path)
        .map(|m| format!("{:.1} MB", m.len() as f64 / 1_048_576.0))
        .unwrap_or_else(|_| "unknown".into());
    info!("Dump complete — {size}");

    Ok((dump_path, size))
}

// ─── Retention ───────────────────────────────────────────────────────────────

/// Enforce retention for a tier. Returns descriptions of every action taken.
/// Files beyond `keep` (oldest first) are moved to `promote_folder_id`
/// if Some, or permanently deleted if None.
async fn enforce_retention(
    drive: &Drive,
    tier: &str,
    folder_id: &str,
    keep: usize,
    promote_folder_id: Option<&str>,
) -> Result<Vec<String>> {
    let files = drive.list_files_sorted(folder_id).await?;
    let count = files.len();
    info!("Tier `{tier}`: {count} files, keeping {keep}");

    let mut actions = Vec::new();

    if count <= keep {
        return Ok(actions);
    }

    let excess = count - keep;
    for file in files.iter().take(excess) {
        match promote_folder_id {
            Some(dst_id) => {
                info!("Promoting [{tier}] → next tier: {}", file.name);
                match drive.move_file(&file.id, folder_id, dst_id).await {
                    Ok(()) => actions.push(format!("Promoted `{}` ({tier} → next tier)", file.name)),
                    Err(e) => warn!("Failed to promote {}: {e}", file.name),
                }
            }
            None => {
                info!("Deleting expired backup: {}", file.name);
                match drive.delete_file(&file.id).await {
                    Ok(()) => actions.push(format!("Deleted expired `{}` from {tier}", file.name)),
                    Err(e) => warn!("Failed to delete {}: {e}", file.name),
                }
            }
        }
    }

    Ok(actions)
}

// ─── Backup orchestration ────────────────────────────────────────────────────

async fn run_backup(
    cfg: &Cfg,
    drive: &Drive,
    folder_ids: &HashMap<&str, String>,
) -> Result<(String, String, Vec<String>)> {
    let (dump_path, size) = dump_database(cfg)?;

    info!("Uploading to Drive/hourly …");
    let uploaded = drive
        .upload_file(&dump_path, &folder_ids["hourly"])
        .await
        .context("Uploading dump to Drive")?;
    info!("Uploaded: {} ({})", uploaded.name, uploaded.id);

    tokio::fs::remove_file(&dump_path)
        .await
        .context("Removing local dump")?;

    let mut retention_actions = Vec::new();
    retention_actions.extend(
        enforce_retention(
            drive,
            "hourly",
            &folder_ids["hourly"],
            cfg.keep_hourly,
            Some(&folder_ids["6h"]),
        )
        .await?,
    );
    retention_actions.extend(
        enforce_retention(
            drive,
            "6h",
            &folder_ids["6h"],
            cfg.keep_6h,
            Some(&folder_ids["daily"]),
        )
        .await?,
    );
    retention_actions.extend(
        enforce_retention(drive, "daily", &folder_ids["daily"], cfg.keep_daily, None)
            .await?,
    );

    Ok((uploaded.name, size, retention_actions))
}

// ─── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "postback=info".into()),
        )
        .init();

    let cfg = Cfg::parse();

    info!("=== postback starting ===");
    info!("DB: {}@{}:{}/{}", cfg.db_user, cfg.db_host, cfg.db_port, cfg.db_name);

    let slack = Slack::from_cfg(&cfg);
    let notify_on = NotifyOn::from_str(&cfg.slack_notify_on);
    if slack.is_some() {
        info!(
            "Slack notifications enabled → channel: {}, events: {}",
            cfg.slack_channel.as_deref().unwrap_or("?"),
            cfg.slack_notify_on,
        );
    }

    if let Some(ref slack) = slack {
        if notify_on.start {
            slack
                .notify_start(&cfg.db_name)
                .await
                .unwrap_or_else(|e| warn!("Slack notification failed: {e}"));
        }
    }

    // ── Auth ──────────────────────────────────────────────────────────────────
    let sa_raw = std::fs::read_to_string(&cfg.sa_key)
        .with_context(|| format!("Reading service account key: {}", cfg.sa_key.display()))?;
    let sa: ServiceAccount =
        serde_json::from_str(&sa_raw).context("Parsing service account JSON")?;

    info!("Authenticating as {}", sa.client_email);
    let token = get_access_token(&sa).await.context("Getting access token")?;
    let drive = Drive::new(token);

    if let Some(ref slack) = slack {
        if notify_on.auth {
            slack
                .notify_auth(&cfg.db_name, &sa.client_email)
                .await
                .unwrap_or_else(|e| warn!("Slack notification failed: {e}"));
        }
    }

    // ── Ensure tier folders exist ─────────────────────────────────────────────
    let mut folder_ids: HashMap<&str, String> = HashMap::new();
    for tier in ["hourly", "6h", "daily"] {
        let id = drive
            .get_or_create_folder(tier, &cfg.gdrive_folder_id)
            .await
            .with_context(|| format!("Getting/creating Drive folder `{tier}`"))?;
        info!("Folder `{tier}` → Drive ID: {id}");
        folder_ids.insert(tier, id);
    }

    // ── Run backup, then dispatch Slack notifications ─────────────────────────
    let result = run_backup(&cfg, &drive, &folder_ids).await;

    if let Some(ref slack) = slack {
        match &result {
            Ok((filename, size, retention_actions)) => {
                if notify_on.success {
                    slack
                        .notify_success(&cfg.db_name, filename, size)
                        .await
                        .unwrap_or_else(|e| warn!("Slack notification failed: {e}"));
                }
                if notify_on.retention && !retention_actions.is_empty() {
                    slack
                        .notify_retention(&cfg.db_name, retention_actions)
                        .await
                        .unwrap_or_else(|e| warn!("Slack notification failed: {e}"));
                }
            }
            Err(err) => {
                if notify_on.failure {
                    slack
                        .notify_failure(&cfg.db_name, &format!("{err:#}"))
                        .await
                        .unwrap_or_else(|e| warn!("Slack notification failed: {e}"));
                }
            }
        }
    }

    result.map(|_| ())?;
    info!("=== postback complete ===");
    Ok(())
}
