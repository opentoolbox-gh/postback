# postback

Automated PostgreSQL backups with rolling retention. Stores backups on a local volume or a remote server via rsync. Optional age encryption and Slack notifications. Runs headless in Docker.

**Retention tiers:**

```
hourly  (keep 6)  →  6h  (keep 4)  →  daily  (keep 28)
```

Each tier promotes its oldest file to the next when full. Daily files are permanently deleted after 28 days (4 weeks).

---

## PostgreSQL version compatibility

The image ships `pg_dump` for every active PostgreSQL major version (13–17). At runtime, postback uses `psql` to ask the server its own version and invokes the matching `/usr/lib/postgresql/<major>/bin/pg_dump` — no version mismatch possible for any supported server.

When PostgreSQL 18 (or later) is released, add one line to the Dockerfile and rebuild:

```dockerfile
postgresql-client-18 \
```

---

## Prerequisites

- Docker + Docker Compose
- A PostgreSQL database reachable from your server
- **Local backend:** a directory or Docker volume to mount into the container
- **Rsync backend:** a remote Linux server you can SSH into
- A Slack workspace (optional, for notifications)

---

## 1. Storage backend

### Option A — Local volume (default)

No extra infrastructure needed. postback writes directly to a directory inside the container; you mount a host path or Docker volume there.

```env
STORAGE_BACKEND=local
LOCAL_BACKUP_PATH=./backups   # host path mounted as /backup inside the container
```

The three tier directories (`hourly/`, `6h/`, `daily/`) are created automatically on first run.

To use a named Docker volume instead of a host path, edit `docker-compose.yml`:

```yaml
volumes:
  - postback_data:/backup
```

---

### Option B — Remote server via rsync

Set `STORAGE_BACKEND=rsync` and follow the steps below.

## 1b. Remote server setup

Postback rsyncs backups over SSH using a dedicated key pair. No password — keys only.

### 1.1 Create a dedicated user on the backup server

```bash
# On the backup server
sudo useradd -m -s /bin/bash backups
sudo mkdir -p /var/backups/postgres
sudo chown backups:backups /var/backups/postgres
```

### 1.2 Generate an SSH key pair

Run this on the machine where you'll run postback (or locally, then copy):

```bash
ssh-keygen -t ed25519 -f ~/.ssh/postback_key -C "postback" -N ""
```

This creates:
- `~/.ssh/postback_key` — private key (goes into postback's container)
- `~/.ssh/postback_key.pub` — public key (goes onto the backup server)

### 1.3 Authorize the key on the backup server

```bash
# On the backup server
sudo -u backups mkdir -p /home/backups/.ssh
sudo -u backups tee -a /home/backups/.ssh/authorized_keys < ~/.ssh/postback_key.pub
sudo chmod 700 /home/backups/.ssh
sudo chmod 600 /home/backups/.ssh/authorized_keys
```

### 1.4 Test the connection

```bash
ssh -i ~/.ssh/postback_key backups@your-backup-server.com echo ok
```

You should see `ok` with no password prompt.

### 1.5 Place the private key on the host running postback

```bash
sudo mkdir -p /etc/postback
sudo cp ~/.ssh/postback_key /etc/postback/rsync_key
sudo chmod 600 /etc/postback/rsync_key
```

---

## 2. Encryption (optional)

Backups can be encrypted before upload using [age](https://age-encryption.org). Asymmetric encryption means:
- The **public key** encrypts — safe to store anywhere, including the container config
- The **private key** decrypts — kept offline or in a vault, never touches the backup server
- A compromised backup server reveals nothing

### 2.1 Generate a key pair

Install `age` locally ([releases](https://github.com/FiloSottile/age/releases)) then:

```bash
age-keygen -o postback_age_key.txt
```

Output looks like:

```
# created: 2026-05-24T10:00:00Z
# public key: age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p
AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ
```

- Put the **public key** (`age1...`) in `ENCRYPTION_PUBLIC_KEY` in your `.env`
- Store `postback_age_key.txt` (the private key) somewhere safe and offline

### 2.2 Restoring an encrypted backup

```bash
# Decrypt and pipe directly into psql
age -d -i postback_age_key.txt hourly_2026-05-24T10-00-00Z.sql.gz.age \
  | gunzip \
  | psql -h db.example.com -U postgres -d mydb

# Or decrypt to a file first
age -d -i postback_age_key.txt hourly_2026-05-24T10-00-00Z.sql.gz.age \
  > hourly_2026-05-24T10-00-00Z.sql.gz
gunzip hourly_2026-05-24T10-00-00Z.sql.gz
psql -h db.example.com -U postgres -d mydb < hourly_2026-05-24T10-00-00Z.sql
```

Encrypted files are stored with a `.age` extension (`hourly_TIMESTAMP.sql.gz.age`). Unencrypted files keep the `.sql.gz` extension. Both work with the same retention logic.

---

## 3. Slack setup (optional)

Skip this section if you don't want Slack notifications.

### 2.1 Create a Slack app

1. Go to [api.slack.com/apps](https://api.slack.com/apps)
2. Click **Create New App → From scratch**
3. Name it (e.g. `postback`) and pick your workspace, then click **Create App**

### 2.2 Add permissions

1. In the left sidebar click **OAuth & Permissions**
2. Scroll down to **Scopes → Bot Token Scopes**
3. Click **Add an OAuth Scope** and add: `chat:write`
4. If you want to post to channels the bot hasn't been invited to, also add: `chat:write.public`

### 2.3 Install the app

1. Scroll back up on the **OAuth & Permissions** page
2. Click **Install to Workspace** and allow it
3. Copy the **Bot User OAuth Token** — it starts with `xoxb-`

### 2.4 Get your channel ID

- For a public channel: use the channel name directly, e.g. `#db-alerts`
- For a private channel: open it in Slack, click the channel name at the top, scroll to the bottom of the popup — you'll see the **Channel ID** (e.g. `C1234567890`)
- If using a private channel, invite the bot first: `/invite @postback`

---

## 4. Configuration

Copy the example env file and fill in your values:

```bash
cp .env.example .env
```

Edit `.env`:

```env
# PostgreSQL — your external database
PGHOST=db.example.com
PGPORT=5432
PGDATABASE=mydb
PGUSER=postgres
PGPASSWORD=your_password

# rsync destination
RSYNC_HOST=backups.example.com
RSYNC_USER=backups
RSYNC_PATH=/var/backups/postgres
RSYNC_PORT=22

# Absolute path to the SSH private key on your host machine
RSYNC_SSH_KEY_PATH=/etc/postback/rsync_key

# Slack (optional)
SLACK_BOT_TOKEN=xoxb-...
SLACK_CHANNEL=#db-alerts
SLACK_NOTIFY_ON=success,failure
```

### Full configuration reference

| Variable | Required | Default | Description |
|---|---|---|---|
| `PGHOST` | Yes | — | PostgreSQL host |
| `PGPORT` | No | `5432` | PostgreSQL port |
| `PGDATABASE` | Yes | — | Database name |
| `PGUSER` | Yes | — | Database user |
| `PGPASSWORD` | Yes | — | Database password |
| `RSYNC_HOST` | Yes | — | Backup server hostname or IP |
| `RSYNC_USER` | Yes | — | SSH user on the backup server |
| `RSYNC_PATH` | Yes | — | Remote path to store backups |
| `RSYNC_PORT` | No | `22` | SSH port on the backup server |
| `RSYNC_SSH_KEY_PATH` | Yes | — | Host path to the SSH private key |
| `ENCRYPTION_PUBLIC_KEY` | No | — | age public key (`age1...`). If set, backups are encrypted before upload |
| `KEEP_HOURLY` | No | `6` | Number of hourly backups to keep |
| `KEEP_6H` | No | `4` | Number of 6-hour backups to keep |
| `KEEP_DAILY` | No | `28` | Number of daily backups to keep |
| `SLACK_BOT_TOKEN` | No | — | Slack bot token (`xoxb-...`) |
| `SLACK_CHANNEL` | No | — | Slack channel name or ID |
| `SLACK_NOTIFY_ON` | No | `success,failure` | Comma-separated list of events to notify on |
| `RUST_LOG` | No | `info` | Log level (`info` or `debug`) |

**`SLACK_NOTIFY_ON` events:**

| Value | When it fires |
|---|---|
| `start` | When the backup run begins |
| `auth` | After SSH connection to backup server is established |
| `success` | After the dump is rsynced successfully |
| `failure` | If the dump or upload fails |
| `retention` | When files are promoted between tiers or deleted |

---

## 5. Running with Docker Compose

### Build and start

```bash
docker compose --env-file .env up -d --build
```

### View logs

```bash
docker compose logs -f postback
```

### Stop

```bash
docker compose down
```

### Rebuild after a code change

```bash
docker compose --env-file .env up -d --build --force-recreate
```

### Run a one-off backup immediately

```bash
docker compose --env-file .env run --rm postback \
  --db-host      "$PGHOST" \
  --db-port      "$PGPORT" \
  --db-name      "$PGDATABASE" \
  --db-user      "$PGUSER" \
  --rsync-host   "$RSYNC_HOST" \
  --rsync-user   "$RSYNC_USER" \
  --rsync-path   "$RSYNC_PATH"
```

---

## 6. Publishing a release image

Every GitHub release automatically builds a multi-platform image (`linux/amd64` + `linux/arm64`) and pushes it to the **GitHub Container Registry** (`ghcr.io`). No secrets to configure — uses the built-in `GITHUB_TOKEN`.

### 5.1 Create a release

```bash
git tag v1.0.0
git push origin v1.0.0
```

Then go to your GitHub repo → **Releases → Draft a new release**, choose the tag, and click **Publish release**.

The workflow produces three tags:

| Tag | Example |
|---|---|
| Full version | `ghcr.io/regisrex/postback:1.0.0` |
| Minor version | `ghcr.io/regisrex/postback:1.0` |
| Latest | `ghcr.io/regisrex/postback:latest` |

### 5.2 Run the published image

Replace the `build:` block in `docker-compose.yml` with the pre-built image:

```yaml
services:
  postback:
    image: ghcr.io/regisrex/postback:latest
    restart: unless-stopped
    # ... rest unchanged
```

Then on any server:

```bash
docker compose --env-file /etc/postback/production.env pull
docker compose --env-file /etc/postback/production.env up -d
```

---

## 7. Remote directory layout

Postback creates the tier directories on the remote server automatically:

```
/var/backups/postgres/
├── hourly/   ← new backups land here
├── 6h/       ← promoted from hourly
└── daily/    ← promoted from 6h, deleted after 28 days
```

Backup files are named by timestamp: `hourly_2026-05-24T10-00-00Z.sql.gz`

---

## 7. Restore and recovery

postback handles restore as a first-class subcommand — the same binary that backs up can restore.

### 7.1 List available backups

See what's available before deciding what to restore:

```bash
docker compose --env-file .env run --rm postback list
```

Output:

```
  hourly/  (6 files)
  ────────────────────────────────────────────────────────────
  hourly_2026-05-24T10-00-00Z.sql.gz          12.4 MB   ← newest
  hourly_2026-05-24T09-00-00Z.sql.gz          12.3 MB
  ...

  6h/  (4 files)
  ────────────────────────────────────────────────────────────
  hourly_2026-05-24T04-00-00Z.sql.gz          12.1 MB

  daily/  (3 files)
  ────────────────────────────────────────────────────────────
  hourly_2026-05-23T22-00-00Z.sql.gz          11.9 MB
```

---

### 7.2 Restore the latest backup

```bash
# Restore latest from hourly tier into the configured PGDATABASE
docker compose --env-file .env run --rm postback restore

# Restore latest from the daily tier
docker compose --env-file .env run --rm postback restore --tier daily

# Restore into a different database
docker compose --env-file .env run --rm postback restore --target-db mydb_recovered
```

---

### 7.3 Restore a specific version

Pick the exact filename from `postback list` and pass it with `--file`:

```bash
docker compose --env-file .env run --rm postback restore \
  --tier daily \
  --file hourly_2026-05-23T22-00-00Z.sql.gz \
  --target-db mydb_recovered
```

This is the full recovery workflow for a specific point in time:

```bash
# 1. See what's available
docker compose --env-file .env run --rm postback list

# 2. Create a fresh target database
docker compose --env-file .env run --rm postback restore \
  --tier daily \
  --file hourly_2026-05-23T10-00-00Z.sql.gz \
  --target-db mydb_2026_05_23
```

---

### 7.4 Restoring encrypted backups

Mount your age private key into the container and pass it with `--private-key`:

```bash
docker compose --env-file .env run --rm \
  -v /path/to/age_private_key.txt:/secrets/age_key.txt:ro \
  postback restore \
  --tier hourly \
  --private-key /secrets/age_key.txt

# Restore a specific encrypted version
docker compose --env-file .env run --rm \
  -v /path/to/age_private_key.txt:/secrets/age_key.txt:ro \
  postback restore \
  --tier daily \
  --file hourly_2026-05-23T22-00-00Z.sql.gz.age \
  --private-key /secrets/age_key.txt \
  --target-db mydb_recovered
```

Or set `AGE_PRIVATE_KEY_PATH` in your `.env` so you don't have to pass it every time.

---

### 7.5 Quick reference

| Goal | Command |
|---|---|
| See all backups | `postback list` |
| Restore latest hourly | `postback restore` |
| Restore latest daily | `postback restore --tier daily` |
| Restore specific file | `postback restore --tier daily --file <name>` |
| Restore to different DB | `postback restore --target-db mydb_recovered` |
| Restore encrypted backup | `postback restore --private-key /secrets/age_key.txt` |

---

## 9. Verifying it works

After the first run, check the logs:

```bash
docker compose logs postback
```

Expected output:

```
=== postback starting ===
DB: postgres@db.example.com:5432/mydb
Remote: backups@backups.example.com:/var/backups/postgres
Remote dirs ready: /var/backups/postgres/{hourly,6h,daily}
Server is PostgreSQL 17 — using /usr/lib/postgresql/17/bin/pg_dump
Dumping database `mydb` → /tmp/postback/hourly_2026-05-24T10-00-00Z.sql.gz
Dump complete — 12.3 MB
Uploading hourly_2026-05-24T10-00-00Z.sql.gz to /var/backups/postgres/hourly/ …
=== postback complete ===
```

SSH onto the backup server and confirm:

```bash
ls -lh /var/backups/postgres/hourly/
```

---

## 10. Deploying on a server (production)

Use `docker-compose.prod.yml` on your server — it pulls the pre-built image from `ghcr.io` instead of building from source.

### 10.1 Install Docker

```bash
curl -fsSL https://get.docker.com | sh
sudo apt-get install docker-compose-plugin
```

### 10.2 Create a working directory and configure

```bash
mkdir -p /opt/postback && cd /opt/postback

# Download the production compose file
curl -fsSL https://raw.githubusercontent.com/regisrex/postback/main/docker-compose.prod.yml \
  -o docker-compose.prod.yml

# Create your env file
curl -fsSL https://raw.githubusercontent.com/regisrex/postback/main/.env.example -o .env
# edit .env with your values
```

### 10.3 Set up storage

**Local backend** — create the backup directory:

```bash
sudo mkdir -p /var/backups/postback
```

Set in `.env`:

```env
STORAGE_BACKEND=local
LOCAL_BACKUP_PATH=/var/backups/postback
```

**Rsync backend** — place your SSH private key:

```bash
sudo mkdir -p /etc/postback
sudo cp /path/to/rsync_key /etc/postback/rsync_key
sudo chmod 600 /etc/postback/rsync_key
```

Set in `.env`:

```env
STORAGE_BACKEND=rsync
RSYNC_SSH_KEY_PATH=/etc/postback/rsync_key
```

### 10.4 Pull and start

```bash
docker compose -f docker-compose.prod.yml --env-file .env pull
docker compose -f docker-compose.prod.yml --env-file .env up -d
```

### 10.5 View logs

```bash
docker compose -f docker-compose.prod.yml --env-file .env logs -f
```

### 10.6 Run one-off commands

```bash
# List all available backups
docker compose -f docker-compose.prod.yml --env-file .env run --rm postback list

# Restore latest hourly backup
docker compose -f docker-compose.prod.yml --env-file .env run --rm postback restore

# Restore latest daily backup
docker compose -f docker-compose.prod.yml --env-file .env run --rm postback restore --tier daily

# Restore a specific file into a recovery database
docker compose -f docker-compose.prod.yml --env-file .env run --rm postback restore \
  --tier daily \
  --file hourly_2026-05-23T22-00-00Z.sql.gz \
  --target-db mydb_recovered
```

### 10.7 Update to a newer image

```bash
docker compose -f docker-compose.prod.yml --env-file .env pull
docker compose -f docker-compose.prod.yml --env-file .env up -d
```

`restart: unless-stopped` ensures the container comes back up after a reboot automatically.
