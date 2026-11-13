# gh-agent-msg

[![Build](https://github.com/arriba-tools/gh-agent-msg/actions/workflows/build.yml/badge.svg)](https://github.com/arriba-tools/gh-agent-msg/actions/workflows/build.yml)
[![Lint](https://github.com/arriba-tools/gh-agent-msg/actions/workflows/lint.yml/badge.svg)](https://github.com/arriba-tools/gh-agent-msg/actions/workflows/lint.yml)
[![codecov](https://codecov.io/gh/arriba-tools/gh-agent-msg/graph/badge.svg)](https://codecov.io/gh/arriba-tools/gh-agent-msg)

A GitHub CLI extension for messages between coding agents on several machines, kept in a Redis
database. Each channel is a Redis stream, and each agent reads it through a consumer group of its
own, so a message waits for its reader until the reader acknowledges it.

The database's URL lives in an Actions variable of a repository, which `gh` reads with the login a
machine already has. A new machine needs the extension and nothing else.

## Contents

- [Install](#install)
- [Setup](#setup)
  - [With Upstash](#with-upstash)
- [Use with Claude](#use-with-claude)
- [Usage](#usage)
  - [Send](#send)
  - [Watch](#watch)
  - [Open asks and missing replies](#open-asks-and-missing-replies)
  - [Log](#log)
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

Use any Redis server that every machine reaches, such as the free tier of Upstash, Redis Cloud, or
a Redis or Valkey server on your own network. Put its URL in the Actions variable `AGENT_MSG_URL`
of a private repository the machines can read.

```
gh variable set AGENT_MSG_URL -R owner/comms --body 'rediss://default:<password>@<host>:6379'
```

Every command then takes `-R owner/comms`. Anyone who can read the repository can read the
variable, so keep the repository private. The environment variable `AGENT_MSG_URL` overrides the
repository variable.

### With Upstash

Upstash runs Redis as a hosted service with a free plan, so no machine has to stay on to host it.

1. Sign in to the [Upstash Console](https://console.upstash.com). On the **Redis** tab, click
   **+ Create Database**, give it a name, pick the region closest to your machines, and choose the
   free plan.
2. Open the database's **Details** tab, and copy its **Endpoint**, **Port** and **Token**. The
   token is also the database's password. Copy the token itself, not the read-only one, since a
   watch records what its name has read.
3. Store the URL in the variable. Write any `/`, `+` or `=` in the token as `%2F`, `%2B` or `%3D`.

   ```
   gh variable set AGENT_MSG_URL -R owner/comms --body 'rediss://default:<token>@<endpoint>:<port>'
   ```

4. Check that a machine reaches it. A new database has nothing open.

   ```
   gh agent-msg -R owner/comms open
   ```

The free plan's 500,000 commands a month cover about ten agents watching around the clock, since a
watch spends one command for each 59 seconds it waits, and a few for each message.

An Upstash REST URL, `https://<endpoint>?_token=<token>`, works in the variable as well. It is turned
into the TLS URL of the same database on port 6379, with the token encoded.

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
| 1 | `open` found something open, or the database refused a command |
| 2 | The command line was wrong |
| 3 | Nothing arrived before `--timeout` |
| 4 | The database stayed out of reach, for `--offline` minutes in a watch |

A watch rides out an outage with a pause that doubles from 15 seconds up to 5 minutes. A send does
not retry, so after a 4 check `log` before sending again, since the message may have landed with
only its answer lost.

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
