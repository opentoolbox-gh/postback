//! postback — PostgreSQL backup and restore with rolling retention
//!
//! Subcommands
//! ───────────
//!   (none) / backup  — dump the database and store it
//!   restore          — pull a backup from storage and restore it
//!   list             — list all backups across tiers
//!
//! Storage backends: local volume | rsync over SSH
//! Encryption:       optional age asymmetric encryption
//!
//! Retention strategy
//! ──────────────────
//!  hourly : keep last 6  → oldest promoted to 6h tier
//!  6h     : keep last 4  → oldest promoted to daily tier
//!  daily  : keep last 28 → oldest deleted (4 weeks)

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use reqwest::Client;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};
use tracing::{info, warn};

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    // ── PostgreSQL ────────────────────────────────────────────────────────────
    #[arg(long, env = "PGHOST", default_value = "localhost")]
    db_host: String,

    #[arg(long, env = "PGPORT", default_value_t = 5432)]
    db_port: u16,

    #[arg(long, env = "PGDATABASE")]
    db_name: String,

    #[arg(long, env = "PGUSER")]
    db_user: String,

    // ── Storage ───────────────────────────────────────────────────────────────

    /// Storage backend: "local" or "rsync"
    #[arg(long, env = "STORAGE_BACKEND", default_value = "local")]
    storage_backend: String,

    /// [local] Path inside the container where backups are written
    #[arg(long, env = "LOCAL_STORAGE_PATH", default_value = "/backup")]
    local_storage_path: PathBuf,

    /// [rsync] Remote host
    #[arg(long, env = "RSYNC_HOST")]
    rsync_host: Option<String>,

    /// [rsync] SSH user on the remote host
    #[arg(long, env = "RSYNC_USER")]
    rsync_user: Option<String>,

    /// [rsync] Remote path where backup tiers live
    #[arg(long, env = "RSYNC_PATH")]
    rsync_path: Option<String>,

    /// [rsync] Path to SSH private key
    #[arg(long, env = "RSYNC_SSH_KEY", default_value = "/secrets/rsync_key")]
    rsync_ssh_key: PathBuf,

    /// [rsync] SSH port
    #[arg(long, env = "RSYNC_PORT", default_value_t = 22)]
    rsync_port: u16,

    // ── Backup knobs ──────────────────────────────────────────────────────────

    #[arg(long, env = "LOCAL_TMP_DIR", default_value = "/tmp/postback")]
    tmp_dir: PathBuf,

    #[arg(long, env = "KEEP_HOURLY", default_value_t = 6)]
    keep_hourly: usize,

    #[arg(long, env = "KEEP_6H", default_value_t = 4)]
    keep_6h: usize,

    #[arg(long, env = "KEEP_DAILY", default_value_t = 28)]
    keep_daily: usize,

    // ── Encryption ────────────────────────────────────────────────────────────

    /// age public key to encrypt backups (optional)
    #[arg(long, env = "ENCRYPTION_PUBLIC_KEY")]
    encryption_key: Option<String>,

    // ── Slack ─────────────────────────────────────────────────────────────────

    #[arg(long, env = "SLACK_BOT_TOKEN")]
    slack_token: Option<String>,

    #[arg(long, env = "SLACK_CHANNEL")]
    slack_channel: Option<String>,

    /// Comma-separated: start, auth, success, failure, retention
    #[arg(long, env = "SLACK_NOTIFY_ON", default_value = "success,failure")]
    slack_notify_on: String,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run a backup (default when no subcommand is given)
    Backup,

    /// Restore a backup to the database
    Restore {
        /// Tier to restore from: hourly | 6h | daily
        #[arg(long, default_value = "hourly")]
        tier: String,

        /// Specific filename to restore (defaults to the latest in the tier)
        #[arg(long)]
        file: Option<String>,

        /// Target database name (defaults to PGDATABASE)
        #[arg(long, env = "RESTORE_TARGET_DB")]
        target_db: Option<String>,

        /// Path to the age private key file (required for encrypted backups)
        #[arg(long, env = "AGE_PRIVATE_KEY_PATH")]
        private_key: Option<PathBuf>,
    },

    /// List all available backups across tiers
    List,
}

// ─── Storage backend trait ───────────────────────────────────────────────────

trait Backend {
    fn name(&self) -> &str;
    fn ensure_dirs(&self) -> Result<()>;
    fn upload(&self, local_path: &Path, filename: &str) -> Result<()>;
    fn list_files_sorted(&self, tier: &str) -> Result<Vec<BackupFile>>;
    fn move_file(&self, filename: &str, from: &str, to: &str) -> Result<()>;
    fn delete_file(&self, filename: &str, tier: &str) -> Result<()>;
    /// Returns a shell fragment that streams the named file to stdout.
    fn stream_cmd(&self, tier: &str, filename: &str) -> Result<String>;
}

#[derive(Debug, Clone)]
struct BackupFile {
    pub name: String,
    pub size: Option<u64>, // bytes, if known
}

impl BackupFile {
    fn size_pretty(&self) -> String {
        match self.size {
            Some(b) => format!("{:.1} MB", b as f64 / 1_048_576.0),
            None    => "—".into(),
        }
    }
}

// ─── Local backend ───────────────────────────────────────────────────────────

struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    fn tier(&self, t: &str) -> PathBuf {
        self.root.join(t)
    }
}

impl Backend for LocalBackend {
    fn name(&self) -> &str { "local" }

    fn ensure_dirs(&self) -> Result<()> {
        for t in ["hourly", "6h", "daily"] {
            std::fs::create_dir_all(self.tier(t))
                .with_context(|| format!("Creating {}/{t}", self.root.display()))?;
        }
        info!("Storage dirs ready: {}/{{hourly,6h,daily}}", self.root.display());
        Ok(())
    }

    fn upload(&self, local_path: &Path, filename: &str) -> Result<()> {
        let dest = self.tier("hourly").join(filename);
        std::fs::copy(local_path, &dest)
            .with_context(|| format!("Copying dump to {}", dest.display()))?;
        Ok(())
    }

    fn list_files_sorted(&self, tier: &str) -> Result<Vec<BackupFile>> {
        let dir = self.tier(tier);
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut entries: Vec<(SystemTime, BackupFile)> = std::fs::read_dir(&dir)
            .with_context(|| format!("Reading {}", dir.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                let t = m.modified().ok()?;
                Some((t, BackupFile {
                    name: e.file_name().to_string_lossy().to_string(),
                    size: Some(m.len()),
                }))
            })
            .collect();
        entries.sort_by_key(|(t, _)| *t); // oldest first
        Ok(entries.into_iter().map(|(_, f)| f).collect())
    }

    fn move_file(&self, filename: &str, from: &str, to: &str) -> Result<()> {
        std::fs::rename(self.tier(from).join(filename), self.tier(to).join(filename))
            .with_context(|| format!("Moving {filename}: {from} → {to}"))
    }

    fn delete_file(&self, filename: &str, tier: &str) -> Result<()> {
        std::fs::remove_file(self.tier(tier).join(filename))
            .with_context(|| format!("Deleting {filename} from {tier}"))
    }

    fn stream_cmd(&self, tier: &str, filename: &str) -> Result<String> {
        Ok(format!("cat {}", self.tier(tier).join(filename).display()))
    }
}

// ─── Rsync backend ───────────────────────────────────────────────────────────

struct RsyncBackend {
    host: String,
    user: String,
    path: String,
    key:  PathBuf,
    port: u16,
}

impl RsyncBackend {
    fn from_cli(cli: &Cli) -> Result<Self> {
        Ok(Self {
            host: cli.rsync_host.clone().context("RSYNC_HOST required when STORAGE_BACKEND=rsync")?,
            user: cli.rsync_user.clone().context("RSYNC_USER required when STORAGE_BACKEND=rsync")?,
            path: cli.rsync_path.clone().context("RSYNC_PATH required when STORAGE_BACKEND=rsync")?,
            key:  cli.rsync_ssh_key.clone(),
            port: cli.rsync_port,
        })
    }

    fn ssh_e(&self) -> Result<String> {
        Ok(format!(
            "ssh -i {} -p {} -o StrictHostKeyChecking=no -o BatchMode=yes",
            self.key.to_str().context("SSH key path not UTF-8")?,
            self.port,
        ))
    }

    fn ssh(&self, cmd: &str) -> Result<String> {
        let out = Command::new("ssh")
            .args([
                "-i", self.key.to_str().context("SSH key path not UTF-8")?,
                "-p", &self.port.to_string(),
                "-o", "StrictHostKeyChecking=no",
                "-o", "BatchMode=yes",
                &format!("{}@{}", self.user, self.host),
                cmd,
            ])
            .output()
            .context("Failed to spawn SSH")?;

        if !out.status.success() {
            bail!("SSH failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8(out.stdout).context("SSH output not UTF-8")?)
    }
}

impl Backend for RsyncBackend {
    fn name(&self) -> &str { "rsync" }

    fn ensure_dirs(&self) -> Result<()> {
        self.ssh(&format!("mkdir -p {p}/hourly {p}/6h {p}/daily", p = self.path))?;
        info!("Remote dirs ready: {}/{{hourly,6h,daily}}", self.path);
        Ok(())
    }

    fn upload(&self, local_path: &Path, _filename: &str) -> Result<()> {
        let dest = format!("{}@{}:{}/hourly/", self.user, self.host, self.path);
        let status = Command::new("rsync")
            .args([
                "-az", "--no-perms",
                "-e", &self.ssh_e()?,
                local_path.to_str().context("Dump path not UTF-8")?,
                &dest,
            ])
            .status()
            .context("Failed to spawn rsync")?;
        if !status.success() {
            bail!("rsync upload failed with {status}");
        }
        Ok(())
    }

    fn list_files_sorted(&self, tier: &str) -> Result<Vec<BackupFile>> {
        let out = self.ssh(&format!("ls -1t {}/{}/", self.path, tier))
            .unwrap_or_default();
        let mut files: Vec<BackupFile> = out
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|n| BackupFile { name: n.to_string(), size: None })
            .collect();
        files.reverse(); // oldest first
        Ok(files)
    }

    fn move_file(&self, filename: &str, from: &str, to: &str) -> Result<()> {
        self.ssh(&format!(
            "mv {p}/{from}/{f} {p}/{to}/{f}",
            p = self.path, f = filename,
        ))
        .with_context(|| format!("Moving {filename}: {from} → {to}"))?;
        Ok(())
    }

    fn delete_file(&self, filename: &str, tier: &str) -> Result<()> {
        self.ssh(&format!("rm {}/{}/{}", self.path, tier, filename))
            .with_context(|| format!("Deleting {filename} from {tier}"))?;
        Ok(())
    }

    fn stream_cmd(&self, tier: &str, filename: &str) -> Result<String> {
        Ok(format!(
            "ssh -i {} -p {} -o StrictHostKeyChecking=no -o BatchMode=yes {}@{} \
             'cat {}/{}/{}'",
            self.key.display(), self.port, self.user, self.host,
            self.path, tier, filename,
        ))
    }
}

// ─── Slack ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SlackApiResponse { ok: bool, error: Option<String> }

#[derive(Debug, Default)]
struct NotifyOn {
    start: bool, auth: bool, success: bool, failure: bool, retention: bool,
}

impl NotifyOn {
    fn parse(s: &str) -> Self {
        let mut n = Self::default();
        for t in s.split(',').map(str::trim) {
            match t {
                "start"     => n.start     = true,
                "auth"      => n.auth      = true,
                "success"   => n.success   = true,
                "failure"   => n.failure   = true,
                "retention" => n.retention = true,
                other if !other.is_empty() => warn!("Unknown SLACK_NOTIFY_ON value: {other}"),
                _ => {}
            }
        }
        n
    }
}

struct Slack { client: Client, token: String, channel: String }

impl Slack {
    fn from_cli(cli: &Cli) -> Option<Self> {
        match (&cli.slack_token, &cli.slack_channel) {
            (Some(t), Some(c)) => Some(Self {
                client:  Client::new(),
                token:   t.clone(),
                channel: c.clone(),
            }),
            _ => None,
        }
    }

    async fn post(&self, payload: serde_json::Value) -> Result<()> {
        let r: SlackApiResponse = self.client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.token)
            .json(&payload)
            .send().await?
            .error_for_status()?
            .json().await?;
        if !r.ok { bail!("Slack: {}", r.error.unwrap_or_default()); }
        Ok(())
    }

    async fn notify_start(&self, db: &str, backend: &str) -> Result<()> {
        self.post(serde_json::json!({ "channel": self.channel, "attachments": [{
            "color": "#439fe0", "title": "postback starting \u{1f504}",
            "fields": [{"title":"Database","value":db,"short":true},
                       {"title":"Backend","value":backend,"short":true}]
        }]})).await
    }

    async fn notify_auth(&self, db: &str, target: &str) -> Result<()> {
        self.post(serde_json::json!({ "channel": self.channel, "attachments": [{
            "color": "#439fe0", "title": "postback connected \u{1f511}",
            "fields": [{"title":"Database","value":db,"short":true},
                       {"title":"Target","value":target,"short":true}]
        }]})).await
    }

    async fn notify_success(&self, db: &str, file: &str, size: &str) -> Result<()> {
        self.post(serde_json::json!({ "channel": self.channel, "attachments": [{
            "color": "#36a64f", "title": "postback succeeded \u{2705}",
            "fields": [{"title":"Database","value":db,"short":true},
                       {"title":"File","value":file,"short":true},
                       {"title":"Size","value":size,"short":true}]
        }]})).await
    }

    async fn notify_failure(&self, db: &str, err: &str) -> Result<()> {
        self.post(serde_json::json!({ "channel": self.channel, "attachments": [{
            "color": "#cc0000", "title": "postback FAILED \u{274c}",
            "fields": [{"title":"Database","value":db,"short":true},
                       {"title":"Error","value":err,"short":false}]
        }]})).await
    }

    async fn notify_retention(&self, db: &str, actions: &[String]) -> Result<()> {
        let text = actions.iter().map(|a| format!("\u{2022} {a}")).collect::<Vec<_>>().join("\n");
        self.post(serde_json::json!({ "channel": self.channel, "attachments": [{
            "color": "#ff9800", "title": "Retention actions \u{1f504}",
            "text": text,
            "fields": [{"title":"Database","value":db,"short":true}]
        }]})).await
    }

    async fn notify_restore(&self, db: &str, file: &str) -> Result<()> {
        self.post(serde_json::json!({ "channel": self.channel, "attachments": [{
            "color": "#9b59b6", "title": "postback restore complete \u{1f504}",
            "fields": [{"title":"Database","value":db,"short":true},
                       {"title":"Restored from","value":file,"short":true}]
        }]})).await
    }
}

// ─── pg_dump version detection ───────────────────────────────────────────────

fn resolve_pg_dump(cli: &Cli) -> Result<String> {
    let out = Command::new("psql")
        .args([
            "-h", &cli.db_host,
            "-p", &cli.db_port.to_string(),
            "-U", &cli.db_user,
            "-d", &cli.db_name,
            "-t", "-A",
            "-c", "SELECT current_setting('server_version_num')::int / 10000;",
        ])
        .output()
        .context("Failed to run psql for version detection")?;

    if !out.status.success() {
        bail!("Version query failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }

    let major: u16 = String::from_utf8(out.stdout)
        .context("psql output not UTF-8")?
        .trim().parse()
        .context("Parsing server major version")?;

    let bin = format!("/usr/lib/postgresql/{major}/bin/pg_dump");
    if !Path::new(&bin).exists() {
        bail!("pg_dump {major} not found at {bin}. Rebuild the image to add support for version {major}.");
    }

    info!("Server is PostgreSQL {major} — using {bin}");
    Ok(bin)
}

// ─── Backup ───────────────────────────────────────────────────────────────────

fn dump_database(cli: &Cli) -> Result<(PathBuf, String, String)> {
    std::fs::create_dir_all(&cli.tmp_dir).context("Creating tmp dir")?;

    let pg_dump    = resolve_pg_dump(cli)?;
    let timestamp  = Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();

    let (filename, encrypt_stage) = match &cli.encryption_key {
        Some(k) => (format!("hourly_{timestamp}.sql.gz.age"), format!("| age -e -r {k}")),
        None    => (format!("hourly_{timestamp}.sql.gz"),     String::new()),
    };

    let dump_path = cli.tmp_dir.join(&filename);

    info!(
        "Dumping `{}` → {} {}",
        cli.db_name, dump_path.display(),
        if cli.encryption_key.is_some() { "(encrypted)" } else { "" },
    );

    let cmd = format!(
        "set -o pipefail; {pg_dump} -h {h} -p {p} -U {u} -d {d} -Fp \
         | gzip -9 {encrypt_stage} > {out}",
        h   = cli.db_host,
        p   = cli.db_port,
        u   = cli.db_user,
        d   = cli.db_name,
        out = dump_path.display(),
    );

    let s = Command::new("bash").args(["-c", &cmd]).status()
        .context("Failed to spawn pg_dump")?;
    if !s.success() { bail!("pg_dump failed with {s}"); }

    let size = std::fs::metadata(&dump_path)
        .map(|m| format!("{:.1} MB", m.len() as f64 / 1_048_576.0))
        .unwrap_or_else(|_| "unknown".into());
    info!("Dump complete — {size}");

    Ok((dump_path, filename, size))
}

fn enforce_retention(
    backend: &dyn Backend,
    tier: &str,
    next_tier: Option<&str>,
    keep: usize,
) -> Result<Vec<String>> {
    let files = backend.list_files_sorted(tier)?;
    let count = files.len();
    info!("Tier `{tier}`: {count} files, keeping {keep}");

    let mut actions = Vec::new();
    if count <= keep { return Ok(actions); }

    for f in files.iter().take(count - keep) {
        match next_tier {
            Some(dst) => match backend.move_file(&f.name, tier, dst) {
                Ok(())  => { info!("Promoted {} → {dst}", f.name); actions.push(format!("Promoted `{}` ({tier} → {dst})", f.name)); }
                Err(e)  => warn!("Failed to promote {}: {e}", f.name),
            },
            None => match backend.delete_file(&f.name, tier) {
                Ok(())  => { info!("Deleted expired: {}", f.name); actions.push(format!("Deleted expired `{}` from {tier}", f.name)); }
                Err(e)  => warn!("Failed to delete {}: {e}", f.name),
            },
        }
    }
    Ok(actions)
}

fn run_backup(cli: &Cli, backend: &dyn Backend) -> Result<(String, String, Vec<String>)> {
    let (dump_path, filename, size) = dump_database(cli)?;

    info!("Storing {} via {} …", filename, backend.name());
    backend.upload(&dump_path, &filename)?;
    std::fs::remove_file(&dump_path).context("Removing local dump")?;

    let mut actions = Vec::new();
    actions.extend(enforce_retention(backend, "hourly", Some("6h"),    cli.keep_hourly)?);
    actions.extend(enforce_retention(backend, "6h",     Some("daily"), cli.keep_6h)?);
    actions.extend(enforce_retention(backend, "daily",  None,          cli.keep_daily)?);

    Ok((filename, size, actions))
}

// ─── Restore ─────────────────────────────────────────────────────────────────

fn run_restore(
    cli:         &Cli,
    backend:     &dyn Backend,
    tier:        &str,
    file:        Option<&str>,
    target_db:   &str,
    private_key: Option<&Path>,
) -> Result<String> {
    // Resolve which file to restore
    let filename: String = match file {
        Some(f) => f.to_string(),
        None => {
            let files = backend.list_files_sorted(tier)?;
            files.into_iter().last()
                .map(|f| f.name)
                .with_context(|| format!("No backups found in the {tier} tier"))?
        }
    };

    let encrypted = filename.ends_with(".age");

    if encrypted && private_key.is_none() {
        bail!(
            "`{filename}` is encrypted — pass --private-key /path/to/age_key.txt \
             or set AGE_PRIVATE_KEY_PATH"
        );
    }

    let stream  = backend.stream_cmd(tier, &filename)?;
    let decrypt = match (encrypted, private_key) {
        (true, Some(k)) => format!("| age -d -i {}", k.display()),
        _               => String::new(),
    };

    let cmd = format!(
        "set -o pipefail; {stream} {decrypt} | gunzip \
         | psql -h {h} -p {p} -U {u} -d {db}",
        h  = cli.db_host,
        p  = cli.db_port,
        u  = cli.db_user,
        db = target_db,
    );

    info!("Restoring {filename} → database `{target_db}`");

    let s = Command::new("bash").args(["-c", &cmd]).status()
        .context("Failed to spawn restore pipeline")?;
    if !s.success() { bail!("Restore failed with {s}"); }

    info!("Restore complete");
    Ok(filename)
}

// ─── List ─────────────────────────────────────────────────────────────────────

fn run_list(backend: &dyn Backend) -> Result<()> {
    let mut total = 0usize;

    for tier in ["hourly", "6h", "daily"] {
        let files = backend.list_files_sorted(tier)?;
        let count = files.len();
        total += count;

        println!("\n  {tier}/  ({count} files)");
        println!("  {}", "─".repeat(60));

        if files.is_empty() {
            println!("  (empty)");
        } else {
            // newest first for readability
            for f in files.iter().rev() {
                println!("  {:<50} {:>8}", f.name, f.size_pretty());
            }
        }
    }

    println!("\n  Total: {total} backup(s)");
    Ok(())
}

// ─── Build backend from CLI ──────────────────────────────────────────────────

fn build_backend(cli: &Cli) -> Result<Box<dyn Backend>> {
    match cli.storage_backend.as_str() {
        "local" => {
            info!("Backend: local → {}", cli.local_storage_path.display());
            Ok(Box::new(LocalBackend { root: cli.local_storage_path.clone() }))
        }
        "rsync" => {
            let r = RsyncBackend::from_cli(cli)?;
            info!("Backend: rsync → {}@{}:{}", r.user, r.host, r.path);
            Ok(Box::new(r))
        }
        other => bail!("Unknown STORAGE_BACKEND={other:?}. Use \"local\" or \"rsync\"."),
    }
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

    let cli = Cli::parse();
    let cmd = cli.command.as_ref().unwrap_or(&Cmd::Backup);

    info!("=== postback {} ===", match cmd {
        Cmd::Backup    => "backup",
        Cmd::Restore { .. } => "restore",
        Cmd::List      => "list",
    });
    info!("DB: {}@{}:{}/{}", cli.db_user, cli.db_host, cli.db_port, cli.db_name);

    let backend    = build_backend(&cli)?;
    let slack      = Slack::from_cli(&cli);
    let notify_on  = NotifyOn::parse(&cli.slack_notify_on);

    match cmd {
        // ── List ─────────────────────────────────────────────────────────────
        Cmd::List => {
            backend.ensure_dirs()?;
            run_list(backend.as_ref())?;
        }

        // ── Restore ──────────────────────────────────────────────────────────
        Cmd::Restore { tier, file, target_db, private_key } => {
            backend.ensure_dirs()?;

            let db = target_db.as_deref().unwrap_or(&cli.db_name);

            if cli.encryption_key.is_some() {
                info!("Encryption is configured; make sure --private-key points to the age private key");
            }

            let result = run_restore(
                &cli,
                backend.as_ref(),
                tier,
                file.as_deref(),
                db,
                private_key.as_deref(),
            );

            match &result {
                Ok(filename) => {
                    if let Some(ref s) = slack {
                        s.notify_restore(db, filename).await
                            .unwrap_or_else(|e| warn!("Slack: {e}"));
                    }
                }
                Err(err) => {
                    if let Some(ref s) = slack {
                        if notify_on.failure {
                            s.notify_failure(db, &format!("{err:#}")).await
                                .unwrap_or_else(|e| warn!("Slack: {e}"));
                        }
                    }
                }
            }

            result.map(|_| ())?;
        }

        // ── Backup (default) ─────────────────────────────────────────────────
        Cmd::Backup => {
            if cli.encryption_key.is_some() {
                info!("Encryption: enabled (age)");
            }

            if let Some(ref s) = slack {
                if notify_on.start {
                    s.notify_start(&cli.db_name, backend.name()).await
                        .unwrap_or_else(|e| warn!("Slack: {e}"));
                }
            }

            backend.ensure_dirs()?;

            if let Some(ref s) = slack {
                if notify_on.auth {
                    let target = match cli.storage_backend.as_str() {
                        "rsync" => format!(
                            "{}@{}",
                            cli.rsync_user.as_deref().unwrap_or("?"),
                            cli.rsync_host.as_deref().unwrap_or("?"),
                        ),
                        _ => cli.local_storage_path.display().to_string(),
                    };
                    s.notify_auth(&cli.db_name, &target).await
                        .unwrap_or_else(|e| warn!("Slack: {e}"));
                }
            }

            let result = run_backup(&cli, backend.as_ref());

            if let Some(ref s) = slack {
                match &result {
                    Ok((filename, size, actions)) => {
                        if notify_on.success {
                            s.notify_success(&cli.db_name, filename, size).await
                                .unwrap_or_else(|e| warn!("Slack: {e}"));
                        }
                        if notify_on.retention && !actions.is_empty() {
                            s.notify_retention(&cli.db_name, actions).await
                                .unwrap_or_else(|e| warn!("Slack: {e}"));
                        }
                    }
                    Err(err) => {
                        if notify_on.failure {
                            s.notify_failure(&cli.db_name, &format!("{err:#}")).await
                                .unwrap_or_else(|e| warn!("Slack: {e}"));
                        }
                    }
                }
            }

            result.map(|_| ())?;
        }
    }

    info!("=== postback done ===");
    Ok(())
}
