# Справочник CLI

Файл сгенерирован командой `tasq docs`; править его руками бессмысленно — гейт `tasq docs --check` в CI падает, если содержимое разошлось с бинарём.

```
tasq 0.4.0
Local-first task tracker for AI agents
```

## Глобальные флаги

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--db` | `PATH` | — | Path to the database; overrides the upward search for .tasq/tasq.db |
| `--json` | — | — | Machine-readable output |
| `--project` | `KEY` | — | Project key to work in; required when the database holds more than one project |
| `--flush` | `SECS` | `3` | Max seconds data may stay unwritten on the mcp server (0 disables the flush) |


## Команды

## `init`

Create .tasq/tasq.db in the current directory and register a project

```
tasq init [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--key` | `KEY` | `TASQ` | Project key used in dot-notation IDs ([A-Z0-9]{2,8}) |
| `--name` | `NAME` | — | Project name |

## `doctor`

Check database integrity, schema version and lease hygiene

```
tasq doctor [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--check` | — | — | Only check, never modify (currently the default behaviour) |
| `--gc` | — | — | Close expired attempts and release their reservations |

## `add`

Create a node; without --parent it becomes a root epic

```
tasq add [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--parent` | `REF` | — | Parent node: dot-ID, bare path or #id |
| `--kind` | `KIND` | — | Node kind: epic \| task \| subtask \| micro (default: from depth) |
| `--desc` | `DESC` | — | Description (the node's specification) |
| `--priority` | `PRIORITY` | — | Priority: low \| medium \| high \| critical |
| `--due` | `DUE` | — | Due date, YYYY-MM-DD |
| `--weight` | `WEIGHT` | — | Weight for burn-up, default 1 |
| `--estimate` | `ESTIMATE` | — | Estimated hours |
| `--source` | `SOURCE` | — | Reference to the code this work touches |
| `--branch` | `BRANCH` | — | Git branch this node belongs to |

## `show`

Show a node, optionally with its subtree

```
tasq show [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--tree` | — | — | Include the whole subtree |

## `ls`

List nodes in tree order

```
tasq ls [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--parent` | `REF` | — | Only this subtree |
| `--status` | `STATUS` | — | Filter by effective status |
| `--assignee` | `ASSIGNEE` | — | Filter by assignee |
| `--overdue` | — | — | Only overdue nodes |
| `--archived` | — | — | Include archived nodes |
| `-n`, `--no-done` | — | — | Hide nodes closed as done |
| `--except-project` | `KEY` | — | Project to leave out of the listing (repeatable); unknown keys are refused |

## `set`

Change node fields; only the given fields are touched

```
tasq set [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--title` | `TITLE` | — |  |
| `--desc` | `DESC` | — |  |
| `--status` | `STATUS` | — | Target status |
| `--priority` | `PRIORITY` | — |  |
| `--due` | `DUE` | — |  |
| `--assignee` | `ASSIGNEE` | — |  |
| `--weight` | `WEIGHT` | — |  |
| `--force` | — | — | Allow guarded transitions (reopen a closed node, finish with open children) |
| `--lock-desc` | — | — | Protect the description from rewrites |
| `--unlock-desc` | — | — | Allow the description to change again |

## `mv`

Move a node under another parent; descendant paths follow

```
tasq mv [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--parent` | `REF` | — | New parent; omit to move to the project root |
| `--renumber` | — | — | Take the next free number among the new siblings |

## `rm`

Soft-delete a todo node

```
tasq rm [ОПЦИИ]
```

## `restore`

Bring back a soft-deleted node

```
tasq restore [ОПЦИИ]
```

## `archive`

Archive (or unarchive) a subtree

```
tasq archive [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--undo` | — | — | Undo the archive |

## `log`

Show recent activity

```
tasq log [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--limit` | `LIMIT` | `20` | How many records to show |

## `claim`

Take a lease on a node and print its token

```
tasq claim [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--holder` | `HOLDER` | — | Who takes the lease (agent name); defaults to $TASQ_HOLDER or `cli` |
| `--ttl` | `TTL` | `900` | Lease time to live in seconds |
| `--steal` | — | — | Take over a live lease (the previous attempt is recorded as superseded) |
| `--force` | — | — | Claim despite unfinished blockers |
| `--checkpoint` | `CHECKPOINT` | — | Checkpoint to store for the next holder |
| `--reserve` | `PATH` | — | Take a file or directory (repeatable): --reserve src/api |
| `--reserve-glob` | `GLOB` | — | Take a glob pattern (repeatable): --reserve-glob 'src/**/*.rs' |
| `--reserve-logical` | `KEY` | — | Take a logical key (repeatable): --reserve-logical port:8080 |
| `--shared` | — | — | Share instead of locking exclusively (applies to the reservations of this call) |

## `reserve`

Add reservations to a live lease

```
tasq reserve [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--path` | `PATH` | — | File or directory (repeatable) |
| `--glob` | `GLOB` | — | Glob pattern (repeatable) |
| `--logical` | `KEY` | — | Logical key (repeatable) |
| `--shared` | — | — | Shared reservation instead of exclusive |

## `unreserve`

Drop reservations of a live lease (all of them without arguments)

```
tasq unreserve [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--path` | `PATH` | — | File or directory (repeatable) |
| `--glob` | `GLOB` | — | Glob pattern (repeatable) |
| `--logical` | `KEY` | — | Logical key (repeatable) |

## `renew`

Extend a lease before it expires

```
tasq renew [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--ttl` | `TTL` | `900` | New time to live in seconds |

## `release`

Close a lease: released | failed | completed

```
tasq release [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--outcome` | `OUTCOME` | `released` | What happened to the work |
| `--checkpoint` | `CHECKPOINT` | — | Checkpoint for whoever picks the node up next |
| `--force` | — | — | Finish despite unfinished children or blockers |

## `holders`

Live leases: who holds what and until when

```
tasq holders [ОПЦИИ]
```

## `dep`

Dependencies between nodes: add, remove, list, graph

```
tasq dep [ОПЦИИ]
```

### `add`

Add a link: `tasq dep add A B` means A waits for B

```
tasq add [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--kind` | `KIND` | `blocks` | Relation kind: blocks \| related \| duplicates \| discovered_from \| split_from |

### `rm`

Remove a link

```
tasq rm [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--kind` | `KIND` | `blocks` |  |

### `ls`

List links of the project

```
tasq ls [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--kind` | `KIND` | — | Only this kind |

### `graph`

Neighbourhood of a node: what it waits for, what it unblocks, Mermaid

```
tasq graph [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--depth` | `DEPTH` | `3` | How deep to walk the blockers chain |
| `--mermaid` | — | — | Print a Mermaid diagram instead of a table |

## `next`

What to work on next, with the reason for the choice

```
tasq next [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--limit` | `LIMIT` | `3` | How many candidates to show |

## `note`

Notes: decisions, context, meetings, blockers, releases

```
tasq note [ОПЦИИ]
```

### `save`

Save a note: without --id it creates, with --id it updates

```
tasq save [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--title` | `TITLE` | — | Note title |
| `--content` | `CONTENT` | — | Note body |
| `--type` | `NOTE_TYPE` | — | Type: general \| decision \| context \| meeting \| technical \| blocker \| progress \| release |
| `--node` | `REF` | — | Attach to a node |
| `--id` | `ID` | — | Update this note instead of creating one |

### `ls`

List notes, newest first

```
tasq ls [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--type` | `NOTE_TYPE` | — |  |
| `--node` | `REF` | — |  |
| `--limit` | `LIMIT` | `50` |  |

### `search`

Search notes by keyword

```
tasq search [ОПЦИИ]
```

### `rm`

Delete a note

```
tasq rm [ОПЦИИ]
```

## `comment`

Comments on a node: a chronological thread

```
tasq comment [ОПЦИИ]
```

### `add`

Add a comment to a node

```
tasq add [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--author` | `AUTHOR` | — | Author name; defaults to $TASQ_HOLDER or `cli` |

### `ls`

List comments of a node

```
tasq ls [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--include-deleted` | — | — | Show soft-deleted comments with their reasons |

### `rm`

Soft-delete a comment; the row stays for the audit trail

```
tasq rm [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--reason` | `REASON` | — | Why it is removed |

### `restore`

Bring a soft-deleted comment back

```
tasq restore [ОПЦИИ]
```

## `template`

Task templates with {variable} placeholders

```
tasq template [ОПЦИИ]
```

### `save`

Save a template from a JSON file or inline JSON

```
tasq save [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--file` | `PATH` | — | Path to a JSON file with the template tree |
| `--tree` | `JSON` | — | Inline JSON (array of nodes or {"nodes": [...]}) |
| `--description` | `DESCRIPTION` | — |  |

### `ls`

List templates with their variables

```
tasq ls [ОПЦИИ]
```

### `show`

Show one template with its tree

```
tasq show [ОПЦИИ]
```

### `apply`

Create nodes from a template

```
tasq apply [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--parent` | `REF` | — | Parent node; without it the roots land at the project top level |
| `--var` | `NAME=VALUE` | — | Variable value as name=value (repeatable) |

### `rm`

Delete a template

```
tasq rm [ОПЦИИ]
```

## `find`

Full-text search across nodes and notes

```
tasq find [ОПЦИИ]
```

## `ctx`

Context chain from the root epic down to the node

```
tasq ctx [ОПЦИИ]
```

## `board`

Board: nodes grouped by status, with overdue and live leases

```
tasq board [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--all` | — | — | One section per project instead of a single project |

## `time`

Hours: estimate against the time actually spent in attempts

```
tasq time [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--parent` | `REF` | — | Only this subtree |

## `bulk`

Batch operations

```
tasq bulk [ОПЦИИ]
```

### `set`

Move several nodes to one status; failures are reported per node

```
tasq set [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--status` | `STATUS` | — | Target status |
| `--force` | — | — |  |

## `burnup`

Burn-up: scope against completed work, with a forecast

```
tasq burnup [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--metric` | `METRIC` | `count` | Metric: count \| points \| hours |
| `--since` | `SINCE` | — | Start day, YYYY-MM-DD |
| `--until` | `UNTIL` | — | End day, YYYY-MM-DD |
| `--ascii` | — | — | Draw an ASCII chart |
| `--snapshot` | — | — | Write snapshots for the whole series |
| `--width` | `WIDTH` | `70` | Chart width in columns |

## `risk`

Nodes that fall behind their own due date

```
tasq risk [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--threshold` | `THRESHOLD` | `0.2` | Allowed lag between elapsed time and completed work, 0..1 |

## `export`

Export the project as JSON with a checksum

```
tasq export [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--out` | `PATH` | — | Write to a file instead of stdout |
| `--include-deleted` | — | — | Include soft-deleted nodes |
| `--all` | — | — | Dump the whole database: every project and every table, with the schema version |
| `--format` | `FORMAT` | — | Document format: json (default) or yaml; defaults to the format of `--out` by extension |

## `import`

Import a project from JSON

```
tasq import [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--mode` | `MODE` | `merge` | merge (add missing nodes) or replace (wipe and load) |
| `--from-saga` | — | — | Read a saga-mcp tracker_export file |
| `--dry-run` | — | — | Validate and report without writing |

## `load`

Load a whole database from a dump (JSON or YAML by extension)

Load a whole database from a dump (JSON or YAML by extension)

Отдельное имя от `restore`: `restore` уже занято возвратом мягко удалённого узла, и один глагол на два разных действия («верни строку» и «залей базу») — источник дорогих опечаток.

```
tasq load [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--force` | — | — | Load into a database that already has nodes |

## `mcp`

Run the MCP server over stdio

```
tasq mcp [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--profile` | `PROFILE` | `full` | Tool profile: full \| agent \| read-only \| migration |

## `create`

Create something: a project

```
tasq create [ОПЦИИ]
```

### `project`

Create a project in the current database

```
tasq project [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--name` | `NAME` | — | Human-readable name (default: the key) |

## `list`

List things: projects

```
tasq list [ОПЦИИ]
```

### `projects`

Projects of the current database, with live node counts

```
tasq projects [ОПЦИИ]
```

## `update`

Change something: a project

```
tasq update [ОПЦИИ]
```

### `project`

Rename or re-status a project

```
tasq project [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--name` | `NAME` | — | New human-readable name |
| `--description` | `DESCRIPTION` | — | New description |
| `--status` | `STATUS` | — | New status: active, on_hold, completed, archived |
| `--new-key` | `KEY` | — | New key; every dot-ID of the project changes with it |

## `delete`

Delete something: a project, with all of its content

```
tasq delete [ОПЦИИ]
```

### `project`

Delete a project with all of its nodes, notes and history

```
tasq project [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--force` | — | — | Delete even when the project still has live nodes or is the last one |

## `docs`

Generate the reference documentation from the binary itself

```
tasq docs [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--out` | `DIR` | `docs` | Where to write (default: docs) |
| `--check` | — | — | Only compare with what is on disk; exit 3 on drift |

## `tokens`

Measure the MCP tool schemas in tokens against the budget

```
tasq tokens [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--profile` | `PROFILE` | — | Only this profile |
| `--max` | `N` | — | Fail (exit 1) if a profile exceeds this many tokens |
| `--per-tool` | — | — | Show the cost of each tool |

## `connect`

Register the MCP server in a client's config

```
tasq connect [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--scope` | `SCOPE` | — | Where to write: user \| project |
| `--url` | `URL` | — | HTTP endpoint instead of stdio (loopback only) |
| `--dry-run` | — | — | Only show what would change |
| `--remove` | — | — | Remove the entry instead of adding it |
| `--check` | — | — | Check the recorded config and exit 1 if it is wrong |

## `completions`

Print a shell completion script for `tasq`

```
tasq completions [ОПЦИИ]
```

| флаг | значение | по умолчанию | описание |
| --- | --- | --- | --- |
| `--out` | `PATH` | — | Write the script to a file instead of stdout |

