# claude-job-runner

A small daemon that turns the chat application's `chat_messages` table into
a queue of tasks for [Claude Code](https://code.claude.com). Every *agentic
user message* (`sender = 'user'`, `is_agentic = 1`) is run as a prompt in a
fresh, sandboxed, headless Claude Code session; the answer is written back
to the same chat as an `ai` message.

```
  ┌───────────────┐  user + agentic + status NULL  ┌────────┐  claude -p   ┌──────────────────┐
  │ chat_messages │ ─────────────────────────────▶ │ worker │ ───────────▶ │ workspaces/job-N │
  │   (MySQL)     │ ◀───────────────────────────── │        │ ◀─────────── │  (sandboxed cwd) │
  └───────────────┘  done + new 'ai' row / failed  └────────┘  JSON report └──────────────────┘
          ▲                                                                   deleted afterwards
          │ POST /jobs
   ┌──────┴─────┐
   │ Hyper API  │
   └────────────┘
```

Lifecycle of a user row: `status = NULL` → `pending` (claimed) → `done` or
`failed`. The claim is an atomic `UPDATE ... WHERE status IS NULL`, so a row
is never handed to two workers. Rows that are not agentic user messages
(`ai` replies, plain chat messages) are never touched and keep
`status = NULL` forever.

On `done`, the answer is inserted as a new row in the same chat with
`sender = 'ai'`, `is_agentic = 1` and `payload = {"in_reply_to": <user id>}`,
and the user row gets `payload.reply_id` pointing at it. On `failed`, the
user row gets `payload.error` (and `payload.result` if Claude produced a
partial answer) and no `ai` row is written. Existing keys in `payload` are
preserved.

Up to `MAX_CONCURRENT_JOBS` jobs run at once, each in its own `claude`
process. A row is claimed only when a slot is free for it, so `pending`
always means "running right now", never "queued inside the daemon".

## Safety model

Every job runs `claude --print` with the job's own directory as `cwd` and a
CLI-level `--settings` payload that:

* enables Claude Code's OS sandbox (Seatbelt on macOS, bubblewrap on Linux)
  with `allowUnsandboxedCommands: false` and `failIfUnavailable: true`, so
  shell commands and their children can **read the whole machine but only
  write inside the workspace**;
* uses the `acceptEdits` permission mode, so the `Edit`/`Write` tools are
  auto-approved under the workspace and denied everywhere else (print mode
  cannot prompt, so a prompt is a denial);
* adds a `Read(//**)` allow rule so the `Read` tool can open any file.

Every job's only data source is the **Granite Manager API**, through the
API's own MCP server (`POST $GRANITE_API_URL/v1/mcp`). Jobs get no database
login of any kind. Before a job starts, the runner trades `AGENT_RUNNER_KEY`
for a token bound to that one question
(`POST /v1/ai/agentic-questions/{id}/agent-token`). The token acts as the
person who asked, in the company they asked in, with their own permissions:

* it sees exactly what that user sees in the app, and nothing of any other
  company;
* it only reads (plus call transcripts), never as a superuser;
* it dies when the question is answered or after 30 minutes.

The MCP server sends every tool call through the API's normal routes with
that token, so the API's own permission checks are the only gate; nothing
depends on the model obeying a prompt. The token reaches Claude Code through
the job's environment (`GRANITE_AGENT_TOKEN`, named as `${GRANITE_AGENT_TOKEN}`
in the MCP config), never on a command line. A question the API will not
issue a token for (already answered, older than a week, or its asker lost
access) is marked `failed` without running Claude.

MCP servers run in Claude Code itself, outside the Bash sandbox, so shell
commands still cannot reach the network (or the database) at all.

The system prompt names the person asking (the chat's `user_id` and
`company_id`) so "show my deals" needs no introduction; what they may see is
decided by the API.

Verified on macOS: a job that reads `/Users/.../README.md`, writes
`notes.md` in its workspace, and tries to write `~/should_not_exist.txt`
reports the first two as successful and the third as blocked.

Things the sandbox does **not** do for you:

* Network access from shell commands is closed by default. Allow specific
  hosts with `CLAUDE_ALLOWED_DOMAINS` (for example `github.com`) if a job
  needs `git push` or `npm install`.
* `CLAUDE_PERMISSION_MODE=bypassPermissions` disables the permission system.
  The Bash sandbox still holds, but `Edit`/`Write` may then leave the
  workspace. Prefer the default.
* Reads of credentials such as `~/.ssh` are allowed by default, as in any
  Claude Code session. Add `denyRead` entries via `CLAUDE_CONFIG_DIR`
  settings if that matters to you.
* The job's environment is the runner's, minus `DATABASE_URL`,
  `AGENT_RUNNER_KEY` and older database and QuickBooks variables. Since jobs
  run as the same OS user and can read files, keep `DATABASE_URL` and
  `AGENT_RUNNER_KEY` out of `.env` and pass them to the daemon through a
  root-only file (see *Deploying*).

Because the workspace is deleted after each job, a job that should change
one of your projects needs a way to get its work out. The intended pattern
is to tell it to `git clone`/`git worktree add` the project *into the
workspace*, do the work there, and push a branch (with the domain
allow-listed), or to return a diff as its answer.

## Setup

1. Make sure the database has the `chats`/`chat_messages` tables and the
   `status` column. Both come from the granite-webhooks migrations
   (`20260920153120_log-chats.sql` and
   `20260920170000_alter-chat-messages-add-status.sql`); run them with
   `sqlx migrate run` in that repository. `schema/mysql.sql` here is a
   reference copy (without the foreign keys to `users`/`company`) that the
   tests load into a throw-away database.

   The runner writes only `chat_messages`: it reads `id`, `chat_id`,
   `content`, `status`, `payload` of agentic user rows, updates `status` and
   `payload`, and inserts `ai` rows. From `chats` it only reads `user_id`
   and `company_id`, to tell each job who is asking.

2. Copy `.env.example` to `.env` and set at least `DATABASE_URL`,
   `GRANITE_API_URL` and `AGENT_RUNNER_KEY` (the same value as on the API).

3. Make sure `claude` is installed and logged in for the user that will run
   the daemon (`claude --version`).

4. Build and run:

   ```sh
   cargo build --release --workspace
   target/release/claude-job-runner
   ```

The daemon exits cleanly on Ctrl-C / SIGTERM. A job that is still running at
that moment is killed and recorded as `failed`.

## Deploying (Ubuntu + systemd)

The production layout this repository's `Makefile` assumes:

* a dedicated user `runner` with rustup and Claude Code installed, and the
  repository cloned to `/home/runner/claude-job-runner`;
* `.env` in that directory (owned by `runner`, mode `600`) with everything
  **except** the writable `DATABASE_URL`;
* `/etc/claude-job-runner/secrets.env` (root, mode `600`) with the values
  jobs must never see: the writable `DATABASE_URL` and
  `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`). The systemd unit
  loads it with `EnvironmentFile=` and runs the binary as `runner`.

Two Ubuntu 24.04 specifics:

* Claude Code's Linux sandbox needs `bubblewrap` and `socat`, and 24.04
  restricts unprivileged user namespaces, so `bwrap` fails with
  `RTM_NEWADDR: Operation not permitted` until it gets an AppArmor profile:

  ```
  # /etc/apparmor.d/bwrap
  abi <abi/4.0>,
  include <tunables/global>
  profile bwrap /usr/bin/bwrap flags=(unconfined) {
    userns,
  }
  ```

  then `apparmor_parser -r /etc/apparmor.d/bwrap`.
* `cargo build --release` needs more than 2 GB of RAM; on a 2 GB instance
  add a 2 GB swap file first.

`make help` lists the day-to-day targets (`make env`, `make secrets`,
`make deploy`, `make logs`, ...). Server targets run on the host, or forward
themselves over SSH when invoked from a laptop.

## Enqueuing work

Normally the chat application inserts the user message itself:

```sql
INSERT INTO chat_messages (chat_id, sender, content, is_agentic)
VALUES (12, 'user', 'Summarise /Users/me/projects/foo/README.md in three bullets.', 1);
```

and the runner picks it up on its next poll. For experiments, use `jobctl`
(see *Tools*).

The HTTP API itself:

| Method | Path         | Body                                 | Response                         |
|--------|--------------|--------------------------------------|----------------------------------|
| GET    | `/health`    |                                      | `{"status":"ok"}`                |
| POST   | `/jobs`      | `{"chat_id": 12, "content": "..."}`  | `201` with the created job       |
| GET    | `/jobs/{id}` |                                      | the job or `404`                 |

A job is `{"id","chat_id","content","status","reply_id","error","reply"}`;
`reply` is the text of the `ai` message once `status` is `done`. Ids of
rows that are not agentic user messages answer `404`.

```sh
curl -X POST localhost:8080/jobs -H 'content-type: application/json' \
     -d '{"chat_id":12,"content":"List the three largest files under /Users/me/projects/foo."}'
curl localhost:8080/jobs/1
```

## Tools

The workspace builds two separate binaries into `target/release/`:

| Binary              | Crate            | What it is                                                   |
|---------------------|------------------|--------------------------------------------------------------|
| `claude-job-runner` | `.` (root)       | The daemon. Does not link `jobctl`.                          |
| `jobctl`            | `crates/jobctl`  | CLI for people: read chats, queue questions, talk to the daemon. |

```sh
jobctl retrieve 12                               # whole chat 12, oldest first (--json for raw rows)
jobctl enqueue "Summarise README.md."            # new chat for user 1, insert, wait for the answer
jobctl enqueue --chat 12 "Follow-up"             # existing chat
jobctl enqueue --no-wait "Long task"             # print ids and exit
jobctl enqueue --show 345                        # watch an existing row
jobctl ask --chat 12 "Say hello"                 # through the HTTP API; starts a runner if none answers
jobctl ask --stop                                # stop the runner `ask` started
```

`retrieve` and `enqueue` use the queue the way the chat application does
(plain SQL on `chats`/`chat_messages`, no HTTP). Credentials: `--env-file
PATH` (with `DB_HOST`/`DB_USER`/`DB_PASSWORD`, database `--database`,
default `main`) if given, else `DATABASE_URL` if set, else the Granite
Manager admin login in `~/general_datebase/.env`. `ask` reads `HTTP_ADDR`
from the environment or `.env` and starts `claude-job-runner` from the same
directory as `jobctl`, logging to `runner.log` in the current directory
(`--no-start` to only talk to a running one).

QuickBooks needs nothing here: the API's `/v1/qbo/*` routes answer jobs with
the asker's own QuickBooks access.

## Configuration

All settings come from the environment (a `.env` file is loaded if present).

| Variable                   | Default            | Meaning                                              |
|----------------------------|--------------------|------------------------------------------------------|
| `DATABASE_URL`             | required           | `mysql://…` (the database holding `chat_messages`)   |
| `DB_MAX_CONNECTIONS`       | `5`                | Pool size                                            |
| `GRANITE_API_URL`          | required           | The Granite Manager API; jobs use its MCP server `/v1/mcp` |
| `AGENT_RUNNER_KEY`         | required           | Traded for each question's agent token; never reaches a job. Secret |
| `WORKSPACE_ROOT`           | `./workspaces`     | Parent of per-job directories; `job-*` dirs are swept on start |
| `POLL_INTERVAL_SECS`       | `5`                | Sleep between polls when the queue is empty          |
| `MAX_CONCURRENT_JOBS`      | `1`                | Jobs (and `claude` processes) running at the same time |
| `HTTP_ADDR`                | `127.0.0.1:8080`   | Bind address of the API                              |
| `REQUEUE_PENDING_ON_START` | `true`             | Reset orphaned `pending` rows; set `false` with several workers |
| `CLAUDE_BIN`               | `claude`           | Executable to run                                    |
| `CLAUDE_MODEL`             | `opus`             | `--model`                                            |
| `CLAUDE_MAX_TURNS`         | `50`               | `--max-turns`                                        |
| `CLAUDE_MAX_BUDGET_USD`    | (none)             | `--max-budget-usd`                                   |
| `CLAUDE_TIMEOUT_SECS`      | `1800`             | Wall-clock limit per job; the process is killed      |
| `CLAUDE_PERMISSION_MODE`   | `acceptEdits`      | or `bypassPermissions` (see above)                   |
| `CLAUDE_EFFORT`            | `low`              | `--effort`: `low`, `medium`, `high`, `xhigh` or `max` |
| `CLAUDE_ALLOWED_DOMAINS`   | (none)             | Comma-separated hosts sandboxed shell commands may reach |
| `CLAUDE_PERSIST_SESSIONS`  | `false`            | Keep transcripts for `claude --resume <session_id>`  |
| `CLAUDE_CONFIG_DIR`        | (inherit)          | Separate Claude config dir for jobs                  |
| `CLAUDE_MCP_CONFIG`        | (none)             | Extra MCP servers JSON file for jobs (the API's is always there) |
| `RUST_LOG`                 | `info`             | Log filter                                           |

`CLAUDE_CONFIG_DIR` is worth setting if your personal `~/.claude` carries
a `CLAUDE.md` or hooks you do not want every job to inherit. MCP servers are
never inherited: jobs run with `--strict-mcp-config` and only the API's
server plus the servers in `CLAUDE_MCP_CONFIG`, if any.

## Resource usage

The daemon itself is a few megabytes, and runs on a single-threaded tokio
runtime to stay that way. Each running job is a `claude`
process, which is a bundled JavaScript runtime: roughly 150–250 MB resident
for a short job, more on long ones with big transcripts, plus whatever the
job's shell commands spawn (`npm install`, test suites, compilers). Size
memory as `MAX_CONCURRENT_JOBS × ~0.3 GB` plus head-room for those
commands. CPU is mostly idle: a session spends nearly all of its time
waiting on the API.

Jobs are started with `DISABLE_AUTOUPDATER=1` and
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`. The API's MCP server is
remote (HTTP), so it adds no process.

## Development

```sh
cargo test --workspace            # unit + integration tests (MySQL, fake claude binary)
cargo clippy --workspace --all-targets
```

The integration tests never call the real CLI: `tests/common/mod.rs`
provides a scripted `fake-claude.sh`. Database tests are
`#[sqlx::test]`: for each test sqlx creates a fresh database on the server
named by `DATABASE_URL` (from `.env`), the test loads `schema/mysql.sql`
into it, and the database is dropped afterwards. Point `DATABASE_URL` at
the test server, never at production.

## Layout

```
src/
  config.rs     environment → Config
  job.rs        MessageId, ChatId, JobStatus, Job, JobOutcome
  store.rs      JobStore: insert / get / reply / claim_next / finish / requeue_pending
  workspace.rs  WorkspaceRoot (sweep) and Workspace (create / remove)
  claude.rs     ClaudeConfig, CliRunner (spawn + parse), ClaudeRunner trait
  worker.rs     polling loop with a semaphore bounding concurrent jobs
  granite.rs    GraniteApi: per-question agent tokens from the Granite API
  http.rs       Hyper API
  main.rs       wiring and graceful shutdown
crates/
  jobctl/       library (db, chats, api) + the `jobctl` CLI
schema/         reference copy of the chat tables (source of truth: granite-webhooks)
tests/          integration tests
```
