# postback

Automated PostgreSQL backups to Google Drive with rolling retention and optional Slack notifications. Runs headless in Docker — no browser, no manual OAuth.

**Retention tiers:**

```
hourly  (keep 6)  →  6h  (keep 4)  →  daily  (keep 28)
```

Each tier promotes its oldest file to the next tier when full. Daily files are permanently deleted after 28 days (4 weeks).

---

## Prerequisites

- Docker + Docker Compose
- A PostgreSQL database reachable from your server
- A Google Cloud project (free tier is fine)
- A Slack workspace (optional, for notifications)

---

## 1. Google Drive setup

Postback authenticates with Google Drive using a **service account** — no browser login required.

### 1.1 Create a Google Cloud project

1. Go to [console.cloud.google.com](https://console.cloud.google.com)
2. Click the project dropdown at the top → **New Project**
3. Give it a name (e.g. `postback`) and click **Create**

### 1.2 Enable the Google Drive API

1. In your project, go to **APIs & Services → Library**
2. Search for **Google Drive API** and click it
3. Click **Enable**

### 1.3 Create a service account

1. Go to **APIs & Services → Credentials**
2. Click **Create Credentials → Service Account**
3. Fill in a name (e.g. `postback-sa`) and click **Create and Continue**
4. Skip the optional role and user access steps — click **Done**
5. You'll see your new service account in the list. Click its email address to open it
6. Go to the **Keys** tab → **Add Key → Create new key**
7. Choose **JSON** and click **Create**
8. A `.json` file downloads automatically — this is your service account key. Keep it safe

### 1.4 Create the backup folder on Google Drive

1. Go to [drive.google.com](https://drive.google.com)
2. Create a new folder (e.g. `postback-backups`)
3. Right-click the folder → **Share**
4. Paste the service account email (found in the JSON file as `client_email`, looks like `postback-sa@your-project.iam.gserviceaccount.com`)
5. Set the role to **Editor** and click **Send**
6. Open the folder. Copy the folder ID from the URL:
   ```
   https://drive.google.com/drive/folders/1AbCdEfGhIjKlMnOpQrStUvWxYz
                                          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                                          this is your GDRIVE_FOLDER_ID
   ```

---

## 2. Slack setup (optional)

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

- For a public channel: you can use the channel name directly, e.g. `#db-alerts`
- For a private channel or to be precise: open the channel in Slack, click the channel name at the top, scroll to the bottom of the popup — you'll see the **Channel ID** (e.g. `C1234567890`)
- If using a private channel, invite the bot first: `/invite @postback`

---

## 3. Configuration

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

# Google Drive
GDRIVE_FOLDER_ID=1AbCdEfGhIjKlMnOpQrStUvWxYz

# Absolute path to the service account JSON key on your host machine
SA_KEY_PATH=/etc/postback/sa_key.json

# Slack (optional)
SLACK_BOT_TOKEN=xoxb-...
SLACK_CHANNEL=#db-alerts
SLACK_NOTIFY_ON=success,failure
```

Place your service account JSON key at the path you set in `SA_KEY_PATH`:

```bash
sudo mkdir -p /etc/postback
sudo cp ~/Downloads/your-service-account-key.json /etc/postback/sa_key.json
sudo chmod 600 /etc/postback/sa_key.json
```

### Full configuration reference

| Variable | Required | Default | Description |
|---|---|---|---|
| `PGHOST` | Yes | — | PostgreSQL host |
| `PGPORT` | No | `5432` | PostgreSQL port |
| `PGDATABASE` | Yes | — | Database name |
| `PGUSER` | Yes | — | Database user |
| `PGPASSWORD` | Yes | — | Database password |
| `GDRIVE_FOLDER_ID` | Yes | — | Google Drive folder ID |
| `SA_KEY_PATH` | Yes | — | Host path to service account JSON key |
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
| `auth` | After the service account authenticates |
| `success` | After the dump uploads to Drive successfully |
| `failure` | If the dump or upload fails |
| `retention` | When files are promoted between tiers or deleted |

---

## 4. Running with Docker Compose

### Build and start

```bash
docker compose up -d --build
```

To use an env file at a non-default path, pass `--env-file`:

```bash
docker compose --env-file /etc/postback/production.env up -d --build
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
docker compose up -d --build --force-recreate
```

### Run a one-off backup immediately (without waiting for the hourly loop)

```bash
docker compose run --rm postback \
  --db-host "$PGHOST" \
  --db-port "$PGPORT" \
  --db-name "$PGDATABASE" \
  --db-user "$PGUSER" \
  --sa-key /secrets/sa_key.json \
  --gdrive-folder-id "$GDRIVE_FOLDER_ID"
```

---

## 5. Drive folder layout

Postback creates three subfolders under your backup folder automatically:

```
postback-backups/
├── hourly/   ← new backups land here
├── 6h/       ← promoted from hourly
└── daily/    ← promoted from 6h, deleted after 28 days
```

Backup files are named by timestamp: `hourly_2026-05-24T10-00-00Z.sql.gz`

---

## 6. Verifying it works

1. After the first run, check the logs:
   ```bash
   docker compose logs postback
   ```
   You should see lines like:
   ```
   === postback starting ===
   Authenticating as postback-sa@your-project.iam.gserviceaccount.com
   Dumping database `mydb` → /tmp/postback/hourly_2026-05-24T10-00-00Z.sql.gz
   Dump complete — 12.3 MB
   Uploading to Drive/hourly …
   Uploaded: hourly_2026-05-24T10-00-00Z.sql.gz
   === postback complete ===
   ```

2. Check your Google Drive folder — you should see the file in the `hourly/` subfolder

3. If Slack is configured, you should receive a notification in your channel

---

## 7. Running on a server (without Docker Desktop)

On a Linux server with only the Docker CLI:

```bash
# Install Docker Engine (if not already installed)
curl -fsSL https://get.docker.com | sh

# Install the Compose plugin
sudo apt-get install docker-compose-plugin

# Clone and configure
git clone https://github.com/regisrex/postback.git
cd postback
cp .env.example .env
# edit .env with your values

# Place your service account key
sudo mkdir -p /etc/postback
sudo cp /path/to/sa_key.json /etc/postback/sa_key.json

# Start
docker compose up -d --build
```

To have it survive reboots, Docker's `restart: unless-stopped` in the compose file handles that automatically once the container is running.
