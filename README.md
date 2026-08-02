![tooler](docs/header.png)

# tooler

> Intelligent process management for **developers** and **DevOps** teams. Built with Rust — fast, portable, no runtime required.

## Table of Contents

- [Installation](#installation)
- [Output formats](#output-formats)
- [Commands](#commands)
  - [tooler info](#tooler-info)
  - [tooler env](#tooler-env)
  - [tooler http](#tooler-http)
  - [tooler check](#tooler-check)
  - [tooler json](#tooler-json)
  - [tooler run](#tooler-run)
  - [tooler play](#tooler-play)
  - [tooler git](#tooler-git)
  - [tooler scaffold](#tooler-scaffold)
  - [tooler config](#tooler-config)
  - [tooler completions](#tooler-completions)
  - [tooler doctor](#tooler-doctor)
  - [tooler report](#tooler-report)
  - [tooler db](#tooler-db)
  - [tooler gh](#tooler-gh)
  - [tooler systemd](#tooler-systemd)
  - [tooler cron](#tooler-cron)
  - [tooler logs](#tooler-logs)
  - [tooler ps](#tooler-ps)
  - [tooler fs](#tooler-fs)
  - [tooler deploy](#tooler-deploy)
  - [tooler fleet](#tooler-fleet)
  - [tooler group](#tooler-group)
  - [tooler stat](#tooler-stat)
  - [tooler mcp](#tooler-mcp)
- [Extending tooler](#extending-tooler)
- [Releasing a new version](#releasing-a-new-version)
- [Dependencies](#dependencies)

---

## Installation

### Option 1 — curl (no Rust required)

Downloads a prebuilt binary for your OS and architecture:

```sh
curl -fsSL https://raw.githubusercontent.com/venturaproject/tooler/master/install.sh | sh
```

Supports: macOS (Intel + Apple Silicon), Linux (x86\_64 + arm64).

To install a specific version:

```sh
TOOLER_VERSION=v1.0.0 curl -fsSL https://raw.githubusercontent.com/venturaproject/tooler/master/install.sh | sh
```

### Option 2 — cargo (requires Rust)

```sh
cargo install --git https://github.com/venturaproject/tooler
```

Or from a local clone:

```sh
git clone https://github.com/venturaproject/tooler
cd tooler
cargo install --path .
```

### Install Rust (only needed for Option 2)

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Requirements

| Method | Requirement |
|---|---|
| curl install | nothing — binary is self-contained |
| cargo install | Rust 1.70+ and Cargo |
| Build from source | Rust 1.70+ and Cargo |

Supported platforms: macOS, Linux, Windows (Windows via cargo only).

### Verify

```sh
tooler --version
```

If the command is not found, add the install directory to your PATH:

```sh
export PATH="$HOME/.local/bin:$PATH"   # curl install
export PATH="$HOME/.cargo/bin:$PATH"   # cargo install
```

### Uninstall

```sh
sudo rm /usr/local/bin/tooler   # curl install
cargo uninstall tooler           # cargo install
```

---

## Output formats

Every command accepts a global `--output` flag:

```sh
tooler <command> --output plain   # default: human-readable with colors
tooler <command> --output json    # machine-readable JSON
tooler <command> --output table   # aligned columns
```

You can set a default in your config so you never have to pass it:

```sh
tooler config set default.output json
```

---

## Commands

### tooler info

Show system information: working directory and environment variables.

```sh
tooler info              # show everything
tooler info --dir        # working directory only
tooler info --env        # environment variables only
tooler info --output json
```

---

### tooler env

Manage `.env` files — inspect, compare and validate them.

```sh
tooler env show .env                      # show vars (values masked)
tooler env show .env --reveal             # show real values
tooler env show .env --output json        # JSON output
tooler env list .env                      # list keys only
tooler env get DATABASE_URL .env          # get a single value
tooler env diff .env .env.example         # keys present in one but not the other
tooler env check .env.example .env        # verify .env has all keys from .env.example
```

---

### tooler http

Make HTTP requests with optional profile-based auth.

```sh
tooler http get https://api.example.com/users
tooler http get /users --profile staging          # uses profile base_url
tooler http get /health --token abc123            # Bearer auth (or set TOOLER_HTTP_TOKEN)
tooler http get /users --header "X-Key: value"
tooler http post /users --body '{"name":"test"}'
tooler http post /users --body '{"name":"test"}' --timeout 30
```

Set a profile base URL and token once, use everywhere:

```sh
tooler config set profile.staging.base_url https://staging.example.com
tooler config set profile.staging.token mytoken123
tooler http get /users --profile staging
```

Tokens are never written to `config.toml` in plaintext — they're stored encrypted in the OS credential store (Keychain on macOS, Credential Manager on Windows, Secret Service on Linux). `tooler config show` / `config profiles` never reveal the value, only whether a token is set (`[token]`). Remove one with `tooler config unset profile.staging.token`.

A profile's stored token is only ever attached automatically when the request's URL has the same scheme+host+port as that profile's `base_url` — a relative path (which is always resolved against `base_url`) or an absolute URL that happens to match it. A request to any other host (e.g. one an MCP tool caller supplies) is sent without it; pass `--token` explicitly if you really want to send a credential somewhere else.

#### OAuth2 profiles (refresh-token grant)

For APIs that require OAuth2 instead of a static bearer token (e.g. Exact Online/Exact Synergy, and most REST ERP APIs), configure a profile with a `token_url` instead of (or in addition to) a static `token`. Once `token_url` is set, `tooler http` automatically exchanges the stored `refresh_token` for a short-lived access token, caches it until shortly before it expires, and transparently refreshes it again as needed — no static token to keep in sync.

```sh
tooler config set profile.exact.base_url https://start.exactonline.nl/api/v1/<division>
tooler config set profile.exact.token_url https://start.exactonline.nl/api/oauth2/token
tooler config set profile.exact.client_id <client_id>
tooler config set profile.exact.client_secret <client_secret>
tooler config set profile.exact.refresh_token <initial_refresh_token>
tooler http get /crm/Accounts --profile exact
```

`tooler` does **not** perform the initial interactive OAuth2 login. Obtaining that first `client_id`/`client_secret`/`refresh_token` triple is a one-time step you do yourself against the provider (register an app, complete the authorization-code login in a browser) — from then on, `tooler` handles every refresh automatically, including providers like Exact that rotate the refresh token on every use (the newly issued one silently replaces the stored one).

Like `token`, `client_secret` and `refresh_token` are stored only in the OS credential store, never in `config.toml`. The cached access token and its expiry live there too. `tooler config profiles` marks OAuth2-managed profiles with `[oauth2]`.

---

### tooler check

Health-check URLs and TCP ports.

```sh
tooler check url https://api.example.com/health
tooler check url https://api.example.com/health --timeout 10
tooler check port localhost 5432          # PostgreSQL
tooler check port localhost 6379          # Redis
tooler check port redis.internal 6379 --timeout 5
```

Exit code is non-zero on failure — works well in scripts and CI.

---

### tooler json

Pretty-print and query JSON from a file or stdin.

```sh
tooler json file.json                     # pretty-print
tooler json file.json --key user.name     # extract nested field
tooler json file.json --compact           # compact output
cat file.json | tooler json               # read from stdin
cat file.json | tooler json --key items
```

---

### tooler run

Run named scripts defined in a `.tooler.toml` file at the project root.

**`.tooler.toml`**:

```toml
[scripts]
dev     = "npm run dev"
test    = "cargo test"
lint    = "cargo clippy -- -D warnings"
build   = "cargo build --release"
deploy  = "sh scripts/deploy.sh staging"
ci      = "cargo fmt --check && cargo clippy -- -D warnings && cargo test"
```

```sh
tooler run               # list available scripts
tooler run build         # execute a script
tooler run test --dry    # preview without running
tooler run test -- --nocapture   # pass extra args to the script
```

Tooler walks up the directory tree to find `.tooler.toml`, so it works from any subdirectory of the project.

---

### tooler play

Run YAML playbooks — define tasks, variables and health checks in a single file. Similar to Ansible but lightweight and self-contained.

**`playbook.yml`**:

```yaml
name: Deploy staging
description: Build, verify and deploy

vars:
  host: staging.example.com
  port: "8080"

tasks:
  - name: Check env file is complete
    env_check:
      reference: .env.example
      target: .env

  - name: Build release binary
    run: cargo build --release
    tags: [build]

  - name: Run tests
    run: cargo test
    tags: [test]
    ignore_errors: true

  - name: Health check
    check_url: http://{{host}}:{{port}}/health
    tags: [deploy]

  - name: Verify database port
    check_port:
      host: "{{host}}"
      port: 5432
      timeout: 3
    tags: [deploy]
```

```sh
tooler play --init                          # generate playbooks/playbook.yml
tooler play playbook.yml                    # run all tasks
tooler play playbook.yml --dry              # preview without executing
tooler play playbook.yml --tags build,test  # run only tagged tasks
tooler play playbook.yml --var host=prod.example.com   # override a variable
```

**Available task actions:**

| Action | Description |
|---|---|
| `run: <cmd>` | Execute a shell command |
| `check_url: <url>` | HTTP health check (expects 2xx) |
| `check_port: {host, port}` | TCP connectivity check |
| `env_check: {reference, target}` | Verify .env has all keys from reference |
| `ssh: {server, command, sudo}` | Run a command on one remote server profile over SSH |
| `fleet: {servers/group/all, command, sudo, parallel}` | Run a command on multiple server profiles (same targeting as [`tooler fleet`](#tooler-fleet)) |
| `include: <name-or-path>` | Run another whole playbook as a single task |
| `assert: "<condition>"` | Fail the task immediately (not skip) unless the condition holds |
| `block: [...]` | Run a list of tasks as a unit, with `rescue:`/`always:` |
| `debug: "<message>"` | Print a rendered message; no side effects |

```yaml
tasks:
  - name: Restart the app on the primary
    ssh:
      server: web1
      command: systemctl restart myapp
      sudo: true

  - name: Roll the whole web group, all at once
    fleet:
      group: web
      command: systemctl restart myapp
      sudo: true
      parallel: true

  - name: Run a shared pre-flight playbook first
    include: preflight.yml

  - name: Refuse to run without a target host
    assert: "{{host}} != ''"

  - name: Deploy with a fallback
    block:
      - name: pull latest
        run: git pull
      - name: build
        run: cargo build --release
    rescue:
      - name: roll back
        run: git checkout -- .
    always:
      - name: clear the lock file
        run: rm -f deploy.lock
```

`block:`/`rescue:`/`always:` run in that order — `rescue:` only if `block:` failed (and, if it succeeds, the block is considered recovered), `always:` unconditionally afterward regardless of outcome (and a failure there fails the block even after a successful rescue). Like `include:`, a `block:` counts as a single ok/failed task in the parent's recap — its own tasks print for visibility but aren't flattened into the parent's totals. Nested tasks get the full `when:`/`loop:`/`retries:`/`register:` support, and can themselves contain another `block:`.

`ssh:`/`fleet:` are the native equivalent of `run: tooler ssh exec ...`/`run: tooler fleet exec ...` — same underlying SSH plumbing, but with structured per-server results and no shelling back into `tooler` itself. A `fleet:` task fails (and, without `ignore_errors: true`, stops the playbook) if any targeted server failed; `parallel: true` runs all targeted servers concurrently instead of one at a time (same flag as `tooler fleet exec/check --parallel`, see [`tooler fleet`](#tooler-fleet)). `ssh:`'s `server:` and `fleet:`'s `servers:`/`group:` are all rendered through `{{var}}` like any other field, so the target can be chosen at invocation time — `fleet: {group: "{{target}}"}` plus `tooler play deploy --var target=web-canary` — instead of hardcoded in the YAML.

`include:` resolves a bare name against `playbooks/` (same lookup as the top-level command) or a path relative to *this playbook's own directory*; the included playbook shares the same live variables (so it can read what the parent has set/registered, and anything it registers is visible back in the parent afterward), runs its own tasks unfiltered by the parent's `--tags`, and counts as a single ok/failed task in the parent's recap — its own tasks aren't flattened into the parent's totals. Include cycles are rejected with a clear error rather than hanging.

**Per-task modifiers**, usable with any action above:

- `when: "{{env}} == prod"` — skip the task unless the condition (evaluated once against the playbook's vars, after `{{var}}` substitution) holds. Supports `==`, `!=`, or a bare truthy check — not a full expression language.
- `loop: [a, b, c]` — run the task once per item, with `{{item}}` available to the action (e.g. `run: systemctl restart {{item}}`). The first failing iteration fails the task; remaining items aren't attempted. Items can also be maps — `loop: [{name: a, port: "1"}, {name: b, port: "2"}]` exposes `{{item.name}}`/`{{item.port}}` per iteration instead of a single `{{item}}`.
- `register: <name>` — capture the task's output into a variable, usable by any later task via `{{name}}`. Supported on `run:`/`ssh:`/`fleet:` only (an upfront error otherwise). `run:` normally streams its subprocess's output live; it only switches to capturing (needed to register it) when `register:` is actually set on that task, so every other `run:` task is unaffected. Inside a `loop:`, only the last iteration's value persists.
- `retries: N` / `delay: S` — retry a failing task up to N extra times, waiting `delay` seconds (default 1) between attempts, before giving up. Applies per `loop:` iteration if combined with `loop:`; ignored entirely in `--dry`.
- `notify: [handler, ...]` / `changed_when: "<condition>"` — trigger one or more `handlers:` (a playbook-level list of tasks, matched by name) when this task succeeds. Each notified handler runs **at most once**, after every regular task has succeeded, deduplicated across however many tasks notified it. Without `changed_when:`, a successful task always counts as "changed"; with it, only when the condition holds (typically checking a `register:`ed value). Notifying a handler name with no matching `handlers:` entry is rejected upfront, before any task runs — not silently ignored.
- `timeout: N` — kill the task if it's still running after N seconds. Only supported on `run:` for now — `ssh:`/`fleet:` route through a shared SSH helper with no process handle to actually kill, so they reject `timeout:` upfront rather than silently not honoring it.

```yaml
handlers:
  - name: restart nginx
    ssh:
      server: web1
      command: systemctl restart nginx
      sudo: true

tasks:
  - name: Deploy
    run: ./deploy.sh
    register: deploy_output

  - name: Only notify if the deploy actually changed something
    when: "{{deploy_output}} != no-op"
    run: ./notify.sh

  - name: Update nginx config
    run: cp nginx.conf /etc/nginx/nginx.conf
    changed_when: "{{deploy_output}} != no-op"
    notify: [restart nginx]

  - name: Wait for the app to come back up
    check_url: http://{{host}}/health
    retries: 5
    delay: 3

  - name: Show what we deployed
    debug: "deployed output was: {{deploy_output}}"

  - name: Give the build a hard ceiling
    run: cargo build --release
    timeout: 300
```

**Templating** — `{{...}}` inside any string field resolves, in order: a playbook/`--var` variable, then `env.<NAME>` (the process environment, e.g. `{{env.HOME}}`), then `secret.<profile>.<key>` (the OS keychain, the same store `tooler config set profile.<name>.token` and OAuth2 profiles already use — e.g. `{{secret.exact.token}}`). Anything that doesn't resolve is left exactly as written, so a missing var/secret never crashes a playbook, it just doesn't get substituted. **Security note**: a rendered secret ends up in a `run:` task's shell command line, which — like any subprocess argv — is visible to other local processes via `ps`/`/proc` while it runs; `ssh:`/`fleet:` carry the same exposure over SSH, no different from how `sudo:` already works today.

Commands and file paths in tasks always resolve **relative to the playbook file's directory**, not where you run `tooler play` from.

#### The `playbooks/` directory

For projects with more than one playbook, keep them in a `playbooks/` directory (found by
walking up from cwd to the nearest `.tooler.toml`, same lookup as `tooler run`'s
`[scripts]` — falls back to the current directory if there's no `.tooler.toml`) and run
them **by name** instead of by path:

```sh
tooler play --init deploy-staging     # writes playbooks/deploy-staging.yml
tooler play --init smoke-test         # writes playbooks/smoke-test.yml
tooler play                           # lists everything in playbooks/
tooler play deploy-staging            # runs playbooks/deploy-staging.yml
```

A bare name (no `/`, no `.yml`/`.yaml`) is always looked up in `playbooks/`. Anything
that looks like a path — contains a `/` or already ends in `.yml`/`.yaml` — is still
opened literally at that path, exactly as before, so `tooler play ./one-off.yml` (or any
existing path-based invocation) keeps working unchanged. Note a `run:`/`env_check:` path
inside a `playbooks/`-based playbook still resolves relative to `playbooks/` itself, not
the project root.

#### Markdown runbooks

Drop a `playbooks/<name>.md` next to `playbooks/<name>.yml` and `tooler` picks it up
automatically — free-form context (why this playbook exists, what to check before
running it, what to do if it fails) for whoever is about to run it, human or agent:

```sh
tooler play deploy-prod            # runs the playbook; playbooks/deploy-prod.md, if
                                    # present, prints under a NOTES banner first
tooler play deploy-prod --notes    # prints just the notes and exits — no tasks run
tooler play                        # bare list marks entries that have notes: [notes]
```

`tooler` never executes this content — it's pure context, not a task type. `--notes` is
useful for an agent driving `tooler play` over MCP: it can read a playbook's notes before
deciding whether to actually run it, without any side effects. Notes are also included in
`--output json`'s summary (as `"notes"`), so a normal run's output carries them too, not
only the `--notes`-only introspection mode. There's no `--init` shortcut for the `.md`
file — writing one is a deliberate act (by you, or by an agent's own file tools), not a
default every playbook gets.

---

### tooler git

Git utilities for day-to-day team workflows.

```sh
tooler git summary                              # branch, tag, ahead/behind, status, recent commits
tooler git clean                                # preview merged branches to delete
tooler git clean --confirm                      # delete merged branches
tooler git clean --confirm --remote             # also delete from origin
tooler git clean --after 010126 --before 150726 # preview: branches with a DDMMYY date
                                                 # suffix (any-name-080726) in that range,
                                                 # regardless of merge status
tooler git changelog                            # commits since last tag, grouped by feat/fix/other
tooler git changelog --from v1.0.0              # changelog from a specific tag
```

`git clean --after`/`--before` targets branches by a trailing `DDMMYY` date suffix in their
name (e.g. `feature-payments-080726` → 8 July 2026) instead of merge status — useful when your
team's naming convention already encodes a retire-by date. Either flag alone gives an
open-ended range; both together bound it.

---

### tooler scaffold

Generate new projects from built-in templates.

```sh
tooler scaffold list                            # list available templates
tooler scaffold new rust-cli my-tool            # create ./my-tool/
tooler scaffold new node-api backend            # create ./backend/
tooler scaffold new python-cli my-script        # create ./my-script/
tooler scaffold new rust-cli my-tool --dir /custom/path
```

**Built-in templates:**

| Template | Description |
|---|---|
| `rust-cli` | Rust CLI with clap and anyhow |
| `node-api` | Node.js REST API with Express |
| `python-cli` | Python CLI with argparse |

Templates substitute `{{name}}`, `{{name_snake}}`, `{{author}}` and `{{year}}` automatically.

---

### tooler config

Manage tooler's configuration stored at `~/.tooler/config.toml`.

```sh
tooler config show                          # show full config
tooler config path                          # show config file location
tooler config get default.output            # read a value
tooler config set default.output json       # set a value (plain | json | table)
tooler config set default.color false
tooler config profiles                      # list configured profiles (with [token] markers)
tooler config set profile.staging.base_url https://staging.example.com
tooler config set profile.staging.token mytoken123   # stored in the OS keychain, not this file
tooler config unset profile.staging.token
```

**Config file structure** (`~/.tooler/config.toml`) — profile tokens are deliberately absent, see above:

```toml
[default]
output = "plain"
color = true

[profile.staging]
base_url = "https://staging.example.com"

[profile.prod]
base_url = "https://example.com"
```

---

### tooler completions

Generate shell completion scripts.

```sh
tooler completions zsh  >> ~/.zshrc
tooler completions bash >> ~/.bashrc
tooler completions fish  > ~/.config/fish/completions/tooler.fish
```

---

### tooler doctor

Run environment/health checks: git installed/configured, OS keychain accessible, SSH key files for configured server profiles, self-exe resolution, and a config summary. Exits non-zero if any check fails.

```sh
tooler doctor
tooler doctor --output json
```

---

### tooler report

Generate a PDF or Excel report from the JSON output of any other tooler command (or any JSON file shaped as an object/array). Array-of-objects fields become tables automatically, and any table with a numeric column gets an embedded bar chart — no manual layout work.

```sh
tooler doctor --output json > doctor.json
tooler report pdf -i doctor=doctor.json -o report.pdf --title "Health Check"
tooler report excel -i doctor=doctor.json -o report.xlsx
```

Pass `--in name=path` more than once to add multiple sections (PDF) / sheets (Excel):

```sh
tooler report pdf -i servers=servers.json -i drift=env-diff.json -o report.pdf --title "Weekly Ops"
```

Omit `--in` to read a single JSON document from stdin:

```sh
tooler env diff .env .env.production --output json | tooler report excel -o drift.xlsx
```

---

### tooler db

Run a read-only SQL query against a remote MySQL or PostgreSQL database and print the rows as JSON — pipe straight into `tooler report`. Rather than opening an SSH tunnel, `tooler db query` runs `psql`/`mysql` directly on the server profile over SSH: many shared hosts (serv00.com and similar) disable `AllowTcpForwarding`, which makes tunneling a dead end there. This means `psql` (Postgres) or `mysql` (MySQL/MariaDB) must already be installed on the *remote* server — nothing extra is required locally.

```sh
tooler db query myserver "SELECT id, email, active FROM users" \
  --env domains/example.com/public_html/backend/.env
```

`--env` points at a remote Laravel/dotenv-style file and reads `DB_CONNECTION` / `DB_HOST` / `DB_PORT` / `DB_DATABASE` / `DB_USERNAME` / `DB_PASSWORD` from it directly on the server, over the same SSH connection — the password never crosses back to your machine as a CLI argument. Without `--env`, pass credentials explicitly instead:

```sh
tooler db query myserver "SELECT * FROM orders LIMIT 20" \
  --engine postgres --host db.internal --database shop --user reporting \
  --password "$TOOLER_DB_PASSWORD"
```

Only `SELECT` / `SHOW` / `EXPLAIN` / `WITH` / `DESCRIBE` are accepted — `tooler db query` refuses anything else (including multiple statements), since results are meant for reporting, not for driving writes against a production database.

`backup`/`restore` dump and restore whole databases the same way, over the same SSH connection — no local `psql`/`mysql` install needed either, since the remote host runs the compression/decompression too:

```sh
tooler db backup myserver --out shop.sql.gz --env backend/.env
tooler db restore myserver --in shop.sql.gz --env backend/.env --confirm
```

`backup` pipes `pg_dump`/`mysqldump` through `gzip -c` by default (pass `--no-gzip` to skip it) and writes the raw bytes straight to `--out`. `restore` pipes the local file into `psql`/`mysql` on the remote host, auto-detecting gzip by magic bytes rather than trusting the filename — and, like `tooler git clean`, is **preview-only unless you pass `--confirm`**: without it, it just reports how many bytes would be sent and to which database.

**Full pipeline** — a real report from a live database in three commands:

```sh
tooler db query myserver "SELECT email, first_name, last_name FROM users" \
  --env backend/.env --output json > users.json
tooler db query myserver "SELECT role, COUNT(*) AS n FROM users GROUP BY role" \
  --env backend/.env --output json > roles.json
tooler report pdf -i usuarios=users.json -i roles=roles.json \
  -o report.pdf --title "Users & Roles"
```

---

### tooler gh

Lists pull requests (title, state, author, labels, dates) via the [`gh`](https://cli.github.com) CLI, optionally filtered to a created-date range — requires `gh` installed and authenticated (`gh auth login`). Labels come back as a single comma-separated string rather than nested JSON, so the result drops straight into `tooler report`.

```sh
tooler gh prs --repo owner/name --after 2026-01-01 --before 2026-07-01
```

Omit `--repo` to use the repo in the current directory (same inference `gh` itself uses). `--state` defaults to `all` (open + closed + merged); `--limit` caps how many PRs are fetched from GitHub before date filtering (default 500).

**PDF/Excel of a quarter's PRs** in two commands:

```sh
tooler gh prs --after 2026-01-01 --before 2026-03-31 --output json > prs.json
tooler report pdf -i pull_requests=prs.json -o q1-prs.pdf --title "Q1 Pull Requests"
```

---

### tooler systemd

Manage a systemd unit on a remote server profile over SSH — no manual `ssh` session required.

```sh
tooler systemd status myserver nginx
tooler systemd restart myserver nginx --sudo
tooler systemd logs myserver nginx --lines 200
```

`status` never fails just because the unit is stopped: it always prints `systemctl status`'s output, and in `--output json` mode returns `"active"` (`true`/`false`, based on the exit code) alongside the raw text. `restart` requires `--sudo` on most setups; pass `--sudo-pass` or set `TOOLER_SUDO_PASS` for non-interactive sudo, or omit both and rely on a `NOPASSWD` sudoers entry. `logs` also accepts `--sudo` since some systems restrict journal access to root.

---

### tooler cron

Inspect and edit a remote server's crontab over SSH.

```sh
tooler cron list myserver
tooler cron add myserver "0 3 * * * /path/to/backup.sh"
tooler cron remove myserver backup.sh
```

`list` parses standard 5-field cron lines into `schedule`/`command`, keeping comments and env-var assignments (e.g. `MAILTO=root`) as raw lines. `add` appends a full crontab line as-is. `remove` drops every line containing the given fixed substring (not a regex) and reports which lines were removed. A user with no crontab yet reads as an empty list rather than an error.

---

### tooler logs

Read a remote log file over SSH without opening a manual session.

```sh
tooler logs tail myserver /var/log/nginx/error.log --lines 200
tooler logs grep myserver /var/log/nginx/error.log "500" --max-lines 50
```

`grep` uses a fixed substring match (`grep -F`), not a regex, and caps how many matching lines come back (`--max-lines`, default 200) with a `truncated` flag in JSON output. For a systemd service's journal instead of a plain file, use [`tooler systemd logs`](#tooler-systemd) instead.

---

### tooler ps

List and signal processes on a remote server over SSH — the process-level equivalent of [`tooler systemd`](#tooler-systemd) for hosts that don't have `systemctl` at all (shared hosting, FreeBSD, containers running a bare init).

```sh
tooler ps list myserver
tooler ps list myserver --filter keepalive
tooler ps kill myserver 12345 --confirm
tooler ps kill myserver 12345 --signal 9 --sudo --confirm
```

`list` parses `ps aux` (the same 11-column layout on both Linux and BSD) and, with `--filter`, keeps only rows whose command line contains the substring or whose PID matches it exactly. `kill` sends a signal (`--signal`, default `TERM`) and is **preview-only unless you pass `--confirm`**, matching `tooler db restore`'s safety pattern — without it, it just reports what would be sent and to which PID.

---

### tooler fs

Read, write, and diff arbitrary files on a remote server over SSH — the general-purpose counterpart to [`tooler env`](#tooler-env) (which only diffs `.env` key sets) and [`tooler logs`](#tooler-logs) (which only tails/greps).

```sh
tooler fs cat myserver /etc/nginx/nginx.conf
tooler fs diff myserver /etc/nginx/nginx.conf --local ./nginx.conf
tooler fs write myserver /etc/nginx/nginx.conf --from-file ./nginx.conf --confirm
```

`cat` prints the remote file's raw content. `diff` fetches the remote file and runs it through the system `diff -u` against a local file, so you get a normal unified diff (or a plain "identical" when there's no drift). `write` overwrites the remote file with either `--from-file <local path>` (binary-safe) or `--content <text>` — exactly one is required — and is **preview-only unless you pass `--confirm`**, matching `tooler db restore`'s safety pattern.

---

### tooler deploy

Orchestrate a remote deploy over SSH — `git pull`, an optional build step, a restart command, and an HTTP health check — as one command instead of chaining several `tooler ssh exec`/`tooler systemd restart`/`tooler check url` calls by hand.

```sh
tooler deploy myserver --path /var/www/app \
  --pull --restart "systemctl restart myapp" \
  --health-url https://myapp.example.com/health --confirm
```

Pass only the steps you want — `--pull`, `--build <cmd>`, `--restart <cmd>`, `--health-url <url>` are all optional, but at least one is required. Steps run in that fixed order and stop at the first failure (there's no automatic rollback: a failed step leaves the server exactly where a hand-run shell script would). The health check retries up to `--health-retries` times (default 3) with `--health-delay` seconds between attempts (default 2). `--restart` runs via `--sudo`/`--sudo-pass` (or `TOOLER_SUDO_PASS`) the same way as [`tooler systemd restart`](#tooler-systemd). Like `tooler db restore`, `tooler deploy` is **preview-only unless you pass `--confirm`** — without it, it prints the ordered plan and touches nothing.

---

### tooler fleet

Run a command, or check SSH reachability, against *multiple* server profiles in one call — the batch counterpart to [`tooler ssh exec`](#tooler-ssh)/[`tooler ssh check`](#tooler-ssh) for anyone tired of looping over servers by hand.

```sh
tooler fleet exec --servers web1,web2,web3 -- "uptime"
tooler fleet exec --all "systemctl is-active myapp" --sudo
tooler fleet exec --group web "uptime"
tooler fleet exec --group web "systemctl restart myapp" --sudo --parallel
tooler fleet check --all
```

Target servers with `--servers a,b,c` (comma-separated profile names), `--all` (every configured profile), or `--group <name>` (a named group, see [`tooler group`](#tooler-group) below) — exactly one of the three is required. `exec` runs the command on each server and **continues past a failing server**, reporting per-server stdout/stderr/success rather than aborting the whole batch (this is a fan-out/observability primitive, not an ordered pipeline like `tooler deploy`); it exits non-zero if any server failed. `exec` has **no `--confirm` gate** — it's exactly as unguarded as `tooler ssh exec`, just run against several servers at once, so treat the command you pass it with the same care. `check` verifies full SSH connectivity (not just a TCP port) to each server and reports which ones are reachable. Both accept `--parallel` to run every targeted server concurrently instead of one at a time — output is identical either way, just faster for larger batches.

---

### tooler group

Named sets of server profiles ("inventory groups"), so `tooler fleet` and playbook `ssh:`/`fleet:` tasks (see [`tooler play`](#tooler-play)) can target a group by name instead of listing `--servers a,b,c` every time.

```sh
tooler group add web --members web1,web2,web3
tooler group list
tooler group show web
tooler group remove web
```

`add` is an upsert (re-running it replaces the member list) and validates every member already exists as a server profile — an unknown name is rejected immediately with a hint to `tooler server add` it first, rather than failing later at `fleet`/playbook run time.

---

### tooler stat

Resource snapshot — uptime/load average, memory, disk usage — for one remote server over SSH, complementing [`tooler ps list`](#tooler-ps)'s process view.

```sh
tooler stat myserver
```

Fetches all three in a single SSH round trip and prints them as-is (no fragile per-OS numeric parsing — `free -h` output differs across distros and doesn't exist on BSD/macOS-family hosts at all, so `stat` falls back to `vm_stat` or reports "unavailable" rather than guessing at a format). Read-only, no `--confirm` needed.

---

### tooler mcp

Run tooler as an [MCP](https://modelcontextprotocol.io) server over stdio, exposing every subcommand as a typed tool (`tooler_info`, `tooler_env_show`, `tooler_ssh_exec`, `tooler_git_clean`, `tooler_gh_prs`, `tooler_systemd_restart`, `tooler_cron_add`, `tooler_logs_grep`, `tooler_ps_kill`, `tooler_db_backup`, `tooler_db_restore`, `tooler_fs_write`, `tooler_deploy_run`, `tooler_fleet_exec`, `tooler_fleet_check`, `tooler_stat`,
`tooler_group_add`, `tooler_group_list`, ...) so Claude and other MCP clients can drive tooler directly instead of shelling out.

```sh
tooler mcp
```

**Claude Code** (project or user scope):

```sh
claude mcp add tooler -- tooler mcp
```

**Claude Desktop** — add to your MCP config:

```json
{
  "mcpServers": {
    "tooler": {
      "command": "tooler",
      "args": ["mcp"]
    }
  }
}
```

Most tools accept an optional `cwd` parameter so a single long-running server can target different project directories across a session.

Tools are annotated (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) so MCP clients can distinguish safe reads (`tooler_info`, `tooler_env_show`, `tooler_check_url`, `tooler_stat`, `tooler_fleet_check`, ...) from destructive operations (`tooler_ssh_exec`, `tooler_ssh_ssl`, `tooler_git_clean`, `tooler_fleet_exec`, ...).

`tooler_ssh_ssl`, `tooler_systemd_restart`, `tooler_ps_kill`, and `tooler_deploy_run` never accept `pfx_password`/`sudo_pass` as tool arguments, `tooler_http_get`/`tooler_http_post` never accept a bearer `token`, and `tooler_db_query`/`tooler_db_backup`/`tooler_db_restore` never accept a database `password` (they'd otherwise sit in plaintext in the conversation/tool-call history, and in `http`'s case be forwarded to whatever URL the caller supplied). Set `TOOLER_PFX_PASS` / `TOOLER_SUDO_PASS` / `TOOLER_HTTP_TOKEN` / `TOOLER_DB_PASSWORD` in the MCP server's own environment instead, e.g.:

```json
{
  "mcpServers": {
    "tooler": {
      "command": "tooler",
      "args": ["mcp"],
      "env": {
        "TOOLER_PFX_PASS": "...",
        "TOOLER_SUDO_PASS": "...",
        "TOOLER_HTTP_TOKEN": "...",
        "TOOLER_DB_PASSWORD": "..."
      }
    }
  }
}
```

`tooler_db_restore`, `tooler_ps_kill`, `tooler_fs_write`, and `tooler_deploy_run` additionally require `confirm: true` to actually apply their change — omit it and the call only previews what would happen, without touching the remote database, process, file, or deploy target.

Likewise, `tooler_config_get`/`tooler_config_set` refuse `profile.<name>.token` over MCP -- set or read it directly in a terminal (`tooler config set profile.staging.token ...`), where it's stored encrypted in the OS keychain instead of passing through the conversation. `tooler_config_unset` is exempt since it only removes a value.

Run `tooler doctor` (also exposed as the `tooler_doctor` tool) to self-check the environment an MCP server is running in: git installed/configured, OS keychain accessible, SSH key files for configured server profiles, and that the binary can resolve itself (a precondition for every tool call, since each one re-execs `tooler`).

**Agentic reporting**: `tooler_db_query` and `tooler_report_pdf`/`tooler_report_excel` are designed to chain. An MCP client can pull real rows from a remote database and turn them into a formatted PDF/Excel report in two tool calls, with no manual step in between — this is the same pipeline documented under [tooler db](#tooler-db) and [tooler report](#tooler-report), just driven by Claude instead of typed by hand. `tooler_db_query` is annotated `readOnlyHint: true` (the CLI itself enforces SELECT/SHOW/EXPLAIN/WITH/DESCRIBE only) and, per the password rule above, its `env` parameter — reading DB credentials from a remote dotenv file over SSH — is the preferred way to authenticate, since it never puts a password in the conversation at all.

**Resources** (read-only, referenceable with `@` in MCP clients): `tooler://config/profiles`, `tooler://config/servers`, `tooler://config/show` -- the same data as their equivalent tools, for use as ambient context.

**Prompts**: `deploy_check(profile)` walks through `tooler_env_diff` / `tooler_check_url` / `tooler_git_summary` for a profile; `env_parity(file_a, file_b)` diffs two `.env` files and summarizes drift.

**Audit log**: pass `--audit-log <path>` (or set `TOOLER_MCP_AUDIT_LOG`) to append one JSON line per tool call (timestamp, argv, cwd, success, duration) -- useful when Claude is driving SSH/git/server operations semi-autonomously.

```sh
tooler mcp --audit-log ~/.tooler/mcp-audit.jsonl
```

**HTTP transport**: `tooler mcp` is stdio-only by default (unchanged for Claude Code/Desktop). Pass `--http` to serve over [Streamable HTTP](https://modelcontextprotocol.io) instead, e.g. to reach it from claude.ai or a remote agent. `rmcp` has no built-in inbound authentication, so `tooler` requires a bearer token and **refuses to start** without one:

```sh
tooler mcp --http --token "$(openssl rand -hex 32)"   # binds 127.0.0.1:8642 by default
tooler mcp --http --bind 0.0.0.0:8642 --token ...      # expose beyond localhost -- do this deliberately
```

`--token` can also come from `TOOLER_MCP_TOKEN`. Clients must send `Authorization: Bearer <token>`; requests without it (or with the wrong token) get `401`.

**Pairing with Playwright MCP**: `tooler` deliberately doesn't wrap browser automation — for visually verifying what it just deployed or checked, pair it with the official [`@playwright/mcp`](https://github.com/microsoft/playwright-mcp) server instead:

```sh
claude mcp add tooler -- tooler mcp
claude mcp add playwright -- npx @playwright/mcp@latest
```

With both configured, an agent can call `tooler_deploy_run` (or `tooler_check_url`) and then use Playwright's own `browser_navigate`/`browser_snapshot` tools to open the URL in a real browser — catching a blank page or a JS error that an HTTP 200 wouldn't.

---

## Extending tooler

Adding a new command takes five steps:

**1.** Create `src/commands/my_command.rs`:

```rust
use anyhow::Result;
use clap::Args;
use crate::context::Context;

#[derive(Args)]
pub struct MyCommandArgs {
    pub input: String,
}

pub fn run(args: MyCommandArgs, _ctx: &Context) -> Result<()> {
    println!("input: {}", args.input);
    Ok(())
}
```

**2.** Register in `src/commands/mod.rs`:

```rust
pub mod my_command;
```

**3.** Add to `Commands` in `src/cli.rs`:

```rust
/// Description shown in --help
MyCommand(my_command::MyCommandArgs),
```

**4.** Handle in `src/main.rs`:

```rust
Commands::MyCommand(args) => commands::my_command::run(args, &ctx),
```

**5.** Reinstall:

```sh
cargo install --path .
```

---

## Releasing a new version

Tag a commit — GitHub Actions builds binaries for all platforms automatically:

```sh
git tag v1.1.0
git push origin v1.1.0
```

Builds for: `linux/x86_64`, `linux/aarch64`, `macos/x86_64`, `macos/aarch64`, `windows/x86_64`.

---

## Dependencies

| Crate | Purpose |
|---|---|
| `clap` | Argument parsing and subcommand structure |
| `anyhow` | Error handling |
| `colored` | Terminal color output |
| `serde` + `serde_json` | JSON serialization |
| `serde_yaml` | YAML playbook parsing |
| `toml` | Config file parsing |
| `reqwest` | HTTP client (http, check, play) |
| `dirs` | Home directory resolution |
| `chrono` | Date/year for scaffold templates |
| `clap_complete` | Shell completion generation |
| `rmcp` + `schemars` + `tokio` | MCP server (`tooler mcp`) |
| `axum` | HTTP transport for `tooler mcp --http` |
| `keyring` | Encrypted credential storage (OS Keychain / Credential Manager / Secret Service) |
| `printpdf` | PDF generation (`tooler report pdf`) |
| `rust_xlsxwriter` | Excel generation (`tooler report excel`) |
