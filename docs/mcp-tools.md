# Инструменты MCP

Файл сгенерирован командой `tasq docs` из каталога самого сервера: имя, описание и схема берутся из того же кода, что отвечает на `tools/list`.

Всего инструментов: 45.

## Профили

| профиль | инструментов | бюджет, токенов | факт |
| --- | --- | --- | --- |
| `full` | 45 | 6000 | 5319 |
| `agent` | 29 | 4000 | 3582 |
| `read-only` | 16 | 2000 | 1667 |
| `migration` | 6 | 800 | 603 |

## Каталог

| инструмент | профили | токенов | обязательные аргументы | описание |
| --- | --- | --- | --- | --- |
| `tasq_init` | full, agent, migration | 102 | — | Initialise the tracker in the current directory and register a project |
| `project_create` | full, migration | 113 | `key`, `name` | Create another project in the same database |
| `project_list` | full, agent, read-only, migration | 60 | — | List projects with their node counts |
| `project_update` | full, agent | 146 | — | Rename a project or change its description and status |
| `project_delete` | full | 107 | — | Delete a project with all of its nodes, notes and history |
| `project_select` | full, agent | 84 | `key` | Switch this session to another project; every later call uses it |
| `node_create` | full, agent | 213 | `title` | Create a node; without parent it becomes a root epic |
| `node_get` | full, agent, read-only, migration | 102 | `ref` | One node with children, comments and the parent chain |
| `node_update` | full, agent | 198 | `ref` | Change node fields; only the given fields are touched |
| `node_list` | full, agent, read-only | 161 | — | List nodes in tree order with light fields |
| `node_move` | full | 132 | `ref` | Move a node under another parent; descendant dot-IDs change |
| `node_reorder` | full | 105 | `refs` | Set the order of siblings |
| `node_archive` | full, agent | 111 | `ref` | Archive or unarchive a subtree; nothing is deleted |
| `node_delete` | full | 99 | `ref` | Soft-delete a todo node; history stays in the database |
| `node_restore` | full | 94 | `ref` | Bring back a soft-deleted node |
| `tasq_context` | full, agent, read-only | 105 | `ref` | Context chain from the root epic down to the node: the why and the what |
| `tasq_next` | full, agent, read-only | 97 | — | What to work on next, with the reason for the choice |
| `tasq_claim` | full, agent | 202 | `ref` | Take an expiring lease on a node; returns the token for renew and release |
| `tasq_renew` | full, agent | 111 | `token` | Extend a lease before it expires |
| `tasq_release` | full, agent | 144 | `token` | Close a lease: released, failed or completed |
| `tasq_reserve` | full, agent | 160 | `token` | Add or drop resources on a live lease: paths, globs and logical keys |
| `tasq_holdings` | full, agent, read-only | 81 | — | Live leases and the resources they hold |
| `tasq_link` | full, agent | 141 | `from`, `to` | Link two nodes; blocks means from waits for to |
| `tasq_unlink` | full, agent | 126 | `from`, `to` | Remove a link between two nodes |
| `tasq_graph` | full, agent, read-only | 137 | `ref` | Neighbourhood of a node: waits for, unblocks, transitive blockers, Mermaid |
| `comment_add` | full, agent | 125 | `ref`, `content` | Add a comment to a node: a thread that survives sessions |
| `comment_list` | full, agent, read-only | 108 | `ref` | Comments of a node in chronological order |
| `comment_delete` | full | 126 | `id` | Soft-delete a comment, or restore it with restore=true |
| `note_save` | full, agent | 164 | — | Create or update a note: decision, context, meeting, blocker, release |
| `note_list` | full, agent, read-only | 116 | — | Notes of the project, newest first |
| `note_search` | full, agent, read-only | 90 | `query` | Search notes by keyword |
| `note_delete` | full | 90 | `id` | Delete a note |
| `template_save` | full | 136 | `name`, `tree` | Create or replace a template: a node tree with {variable} placeholders |
| `template_list` | full | 75 | — | List templates with their variables |
| `template_apply` | full | 124 | `name` | Create nodes from a template under a parent |
| `template_delete` | full | 90 | `name` | Delete a template |
| `activity_log` | full, agent, read-only | 90 | — | Recent changes: what happened and when |
| `session_diff` | full, agent, read-only | 102 | `since` | What changed since a timestamp: counts by action plus recent records |
| `node_batch_update` | full | 136 | `refs`, `status` | Move several nodes to one status; failures are reported per node |
| `tasq_search` | full, agent, read-only | 100 | `query` | Full-text search across nodes and notes |
| `tasq_dashboard` | full, agent, read-only | 87 | — | One-call overview: board, live leases, next candidates, at-risk nodes |
| `tasq_burnup` | full, agent, read-only | 143 | — | Burn-up series and forecast by count, points or hours |
| `tasq_risk` | full, read-only | 99 | — | Nodes that fall behind their own due date |
| `tasq_export` | full, migration | 94 | — | Export the project as JSON with a checksum |
| `tasq_import` | full, migration | 133 | `document` | Import an export document, from tasq or from saga-mcp |
