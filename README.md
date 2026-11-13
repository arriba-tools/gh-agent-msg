# gh-agent-msg

[![Build](https://github.com/arriba-tools/gh-agent-msg/actions/workflows/build.yml/badge.svg)](https://github.com/arriba-tools/gh-agent-msg/actions/workflows/build.yml)
[![Lint](https://github.com/arriba-tools/gh-agent-msg/actions/workflows/lint.yml/badge.svg)](https://github.com/arriba-tools/gh-agent-msg/actions/workflows/lint.yml)
[![codecov](https://codecov.io/gh/arriba-tools/gh-agent-msg/graph/badge.svg)](https://codecov.io/gh/arriba-tools/gh-agent-msg)

A GitHub CLI extension for coding agents on several machines. Agents send each other messages
through a Redis database, and keep notes in an Upstash Vector index, which they search by meaning
and by keyword together.

Each channel is a Redis stream, and each agent reads it through a consumer group of its own, so a
message waits for its reader until the reader acknowledges it.

Actions variables of a repository name the database and the index, and `gh` reads them with the
login a machine already has. A new machine needs the extension and nothing else.

## Contents

- [Install](#install)
- [Setup](#setup)
  - [Messages: a Redis database](#messages-a-redis-database)
    - [With Upstash Redis](#with-upstash-redis)
  - [Notes: an Upstash Vector index](#notes-an-upstash-vector-index)
- [Use with Claude](#use-with-claude)
- [Usage](#usage)
  - [Send](#send)
  - [Watch](#watch)
  - [Open asks and missing replies](#open-asks-and-missing-replies)
  - [Log](#log)
  - [Remember and recall](#remember-and-recall)
- [Messages](#messages)
- [Exit statuses](#exit-statuses)
- [Development](#development)
- [License](#license)

## Install

```
gh extension install arriba-tools/gh-agent-msg
```

Upgrade with `gh extension upgrade agent-msg`.

## Setup

The extension reads two Actions variables of a private repository that every machine can read, so
every command takes `-R owner/comms`.

| Variable | Names | Read by |
|----------|-------|---------|
| `AGENT_MSG_URL` | The Redis database that carries the messages | `send`, `watch`, `open`, `log` |
| `AGENT_VECTOR_URL` | The Upstash Vector index that keeps the notes | `remember`, `recall` |

Messages need only the database, and notes only the index, so leave `AGENT_VECTOR_URL` unset to go
without notes. Anyone who can read the repository can read the variables, so keep it private. An
environment variable of the same name overrides each one.

### Messages: a Redis database

Use any Redis server that every machine reaches, such as Upstash Redis, Redis Cloud, or a Redis or
Valkey server on your own network, and put its URL in `AGENT_MSG_URL`.

```
gh variable set AGENT_MSG_URL -R owner/comms --body 'rediss://default:<password>@<host>:6379'
```

#### With Upstash Redis

Upstash runs Redis as a hosted service with a free plan, so no machine has to stay on to host it.

1. Sign in to the [Upstash Console](https://console.upstash.com). On the **Redis** tab, click
   **+ Create Database**, give it a name, pick the region closest to your machines, and choose the
   free plan.
2. Open the database's **Details** tab, and copy its **Endpoint**, **Port** and **Token**. The
   token is also the database's password. Copy the token itself, not the read-only one, since a
   watch records what its name has read.
3. Store the URL in `AGENT_MSG_URL`. Write any `/`, `+` or `=` in the token as `%2F`, `%2B` or `%3D`.

   ```
   gh variable set AGENT_MSG_URL -R owner/comms --body 'rediss://default:<token>@<endpoint>:<port>'
   ```

4. Check that a machine reaches it. A new database has nothing open.

   ```
   gh agent-msg -R owner/comms open
   ```

The free plan's 500,000 commands a month cover about ten agents watching around the clock, since a
watch spends one command for each 59 seconds it waits, and a few for each message.

An Upstash Redis REST URL, `https://<endpoint>?_token=<token>`, works in `AGENT_MSG_URL` as well.
It is turned into the TLS URL of the same database on port 6379, with the token encoded.

### Notes: an Upstash Vector index

`remember` and `recall` need an Upstash Vector index that embeds text itself. Upstash Vector is a
separate product from Upstash Redis, with its own tab in the console, and its own URL and token.

1. In the [Upstash Console](https://console.upstash.com), on the **Vector** tab, click
   **Create Index**. Pick the same region as the database, and the type **Hybrid**, with a hosted
   embedding model for the dense part and **BM25** for the sparse part.
2. From the index's **Connect** section, copy `UPSTASH_VECTOR_REST_URL` and
   `UPSTASH_VECTOR_REST_TOKEN`. Take the token itself, not the read-only one, since `remember`
   writes.
3. Store them in `AGENT_VECTOR_URL` as one URL, with the token as `_token`. Unlike the Redis
   token, this one goes in as copied.

   ```
   gh variable set AGENT_VECTOR_URL -R owner/comms --body 'https://<endpoint>?_token=<token>'
   ```

4. Check that a machine reaches it. A new index has no notes.

   ```
   gh agent-msg -R owner/comms recall my-project test
   ```

## Use with Claude

Add a line like this to `~/.claude/CLAUDE.md`, naming your repository:

```
* Use `gh agent-msg -R owner/comms` to message the agents on other machines. Run `gh agent-msg --help` for the commands.
```

## Usage

```
gh agent-msg [-R OWNER/REPO] <command>
```

| Command | Description |
|---------|-------------|
| `send <NAME> <CHANNEL>` | Send a message `--to` names or `all`, with a `--subject` and a `--file` or `--text`, and print its ID |
| `watch <NAME> <CHANNEL>` | Wait for messages to a name and print them, or with `--re` for the replies to messages |
| `open` | List the asks that wait on someone and the messages with missing replies, or `--wait` on one |
| `log <CHANNEL>` | Print a channel's latest messages, newest first |
| `remember <NAME> <CHANNEL>` | Store a note from a `--file` or `--text` in the channel's index, and print its ID |
| `recall <CHANNEL> <QUERY>` | Print the channel's notes that best match, by meaning and by keyword |

A name is a session's name, such as `coordinator` or `win2`. A channel is usually named after the
project the agents work on.

### Send

```
gh agent-msg -R owner/comms send linux my-project \
  --to "coordinator, mac" --subject "#42 tests pass, merging next" --file message.md
```

It prints the new message's ID, which a reply names with `--re`. `--needs <name>` marks a message
that waits on a person, and `--ack-required "<names>"` one that each of those names must answer.

### Watch

Start the watch as a background command. It holds its connection open while it waits, and exits
when a message for the name arrives, so the exit wakes the agent that started it.

```
gh agent-msg -R owner/comms watch mac my-project
```

Messages for other names pass it by. Printing a message does not acknowledge it. Once the messages
are handled, restart with the `--ack` the command printed, so an agent that stops first receives
them again. The first watch under a name also delivers the last day's messages to that name.

To wait for the replies to a message instead, name it with `--re`:

```
gh agent-msg -R owner/comms watch mac my-project --re 1794536187224-0
```

### Open asks and missing replies

```
gh agent-msg -R owner/comms open
gh agent-msg -R owner/comms open --needs alex
gh agent-msg -R owner/comms open --wait my-project/1794536187224-0
```

An ask with `needs` stays open until its own sender replies to it, which the sender does once the
person has answered. A message with `ack-required` is open while one of its names has not replied.
`--wait` blocks until every one of them has.

### Log

```
gh agent-msg -R owner/comms log my-project --count 20
```

### Remember and recall

```
gh agent-msg -R owner/comms remember linux my-project --text "Fits cross at D 236.6"
gh agent-msg -R owner/comms recall my-project "where do the fits cross" --count 5
```

Each channel is a namespace of the index. A note's ID is the milliseconds since 1970, the name that
stored it, and the ID of the process that stored it. `recall` prints each note as a header, a blank line, then its text, best match
first. A hybrid index ranks by meaning and by the words themselves, so an exact figure such as
`D 236.6` still finds its note.

## Messages

A message is an entry in the stream `chan:<channel>`, with an ID such as `1794536187224-0`, the
milliseconds since 1970 and a sequence number. A watch prints it as a header, a blank line, then
the text.

```
--- 1794536187224-0
from: linux
to: coordinator, mac
re: 1794536129979-0
subject: #42 tests pass, merging next

Every test passes. I merge next unless you object.
```

| Field | Holds |
|-------|-------|
| `from` | The sender's name |
| `to` | The recipients, separated by commas, or `all` |
| `re` | The ID of the message it answers, in the same channel |
| `subject` | One line saying what it is about |
| `needs` | The person it waits on |
| `ack-required` | The names that must each reply |
| `body` | The text |

Each name reads a channel through the consumer group of that name, which starts a day back. A
channel keeps about its last 100,000 messages.

## Exit statuses

| Status | Meaning |
|--------|---------|
| 0 | It printed the messages, or `open` found nothing open |
| 1 | `open` found something open, or the database or the index refused a request |
| 2 | The command line was wrong |
| 3 | Nothing arrived before `--timeout` |
| 4 | The database or the index could not be reached, after `--offline` minutes of retries in a watch |

A watch rides out an outage with a pause that doubles from 15 seconds up to 5 minutes. A send does
not retry, so after a 4 check `log` before sending again, since the message may have landed with
only its answer lost. `remember` and `recall` do not retry either.

## Development

```
git clone https://github.com/arriba-tools/gh-agent-msg.git
cd gh-agent-msg
cargo build --release
cp target/release/gh-agent-msg .
gh extension install .
```

The copied binary is ignored by git. Rebuild and copy it again to pick up changes.

## License

MIT
