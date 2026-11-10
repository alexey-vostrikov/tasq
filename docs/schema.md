# Схема базы

Файл сгенерирован командой `tasq docs` из пустой базы, созданной текущими миграциями: это фактические `CREATE TABLE` и `CREATE VIEW`, а не их пересказ.

Версия схемы: **v6**.

## Таблицы

### `activity_log`

```sql
CREATE TABLE "activity_log" (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  node_id     INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
  entity_type TEXT NOT NULL
                CHECK (entity_type IN ('node','comment','note','edge','project','template',
                                       'batch','import')),
  entity_id   INTEGER NOT NULL,
  action      TEXT NOT NULL,
  field_name  TEXT,
  old_value   TEXT,
  new_value   TEXT,
  summary     TEXT,
  actor       TEXT,
  attempt_id  INTEGER,
  created_at  TEXT NOT NULL
)
```

### `attempts`

```sql
CREATE TABLE attempts (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  attempt_no  INTEGER NOT NULL,
  holder      TEXT NOT NULL,
  token       TEXT NOT NULL UNIQUE,
  ttl_seconds INTEGER NOT NULL,
  started_at  TEXT NOT NULL,
  expires_at  TEXT NOT NULL,
  renewed_at  TEXT,
  ended_at    TEXT,
  end_reason  TEXT CHECK (end_reason IN ('released','expired','failed','completed','superseded')),
  checkpoint  TEXT, session_id TEXT REFERENCES sessions(id),
  UNIQUE (node_id, attempt_no)
)
```

### `burn_snapshots`

```sql
CREATE TABLE burn_snapshots (
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  day         TEXT NOT NULL,
  metric      TEXT NOT NULL CHECK (metric IN ('count','points','hours')),
  scope_total REAL NOT NULL,
  scope_done  REAL NOT NULL,
  scope_added REAL NOT NULL,
  computed_at TEXT NOT NULL,
  PRIMARY KEY (project_id, day, metric)
)
```

### `comments`

```sql
CREATE TABLE comments (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id       INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  author        TEXT,
  content       TEXT NOT NULL,
  is_deleted    INTEGER NOT NULL DEFAULT 0,
  deleted_at    TEXT,
  deleted_by    TEXT,
  delete_reason TEXT,
  created_at    TEXT NOT NULL
)
```

### `edges`

```sql
CREATE TABLE edges (
  from_node  INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  to_node    INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL
               CHECK (kind IN ('blocks','related','duplicates','discovered_from','split_from')),
  created_at TEXT NOT NULL,
  created_by TEXT,
  PRIMARY KEY (from_node, to_node, kind),
  CHECK (from_node <> to_node)
)
```

### `meta`

```sql
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
)
```

### `node_status_history`

```sql
CREATE TABLE "node_status_history" (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  from_status TEXT REFERENCES statuses(name),
  to_status   TEXT NOT NULL REFERENCES statuses(name),
  cause       TEXT NOT NULL CHECK (cause IN ('manual','lease_claimed','lease_completed',
                                             'lease_released','lease_expired','unblock',
                                             'gc','import','restore')),
  at          TEXT NOT NULL,
  actor       TEXT
)
```

### `nodes`

```sql
CREATE TABLE "nodes" (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id         INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  parent_id          INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
  depth              INTEGER NOT NULL,
  seq                INTEGER NOT NULL,
  path               TEXT NOT NULL,
  kind               TEXT NOT NULL
                       CHECK (kind IN ('epic','task','subtask','micro')),
  title              TEXT NOT NULL,
  description        TEXT,
  description_locked INTEGER NOT NULL DEFAULT 0,
  -- Единственное изменение против v3: вместо списка в CHECK — ссылка на справочник.
  status             TEXT NOT NULL REFERENCES statuses(name),
  priority           TEXT NOT NULL DEFAULT 'medium'
                       CHECK (priority IN ('low','medium','high','critical')),
  assigned_to        TEXT,
  weight             REAL NOT NULL DEFAULT 1,
  estimated_hours    REAL,
  due_date           TEXT,
  branch             TEXT,
  source_ref         TEXT,
  tags               TEXT NOT NULL DEFAULT '[]',
  metadata           TEXT NOT NULL DEFAULT '{}',
  content_hash       TEXT,
  sort_order         INTEGER NOT NULL DEFAULT 0,
  archived_at        TEXT,
  deleted_at         TEXT,
  created_at         TEXT NOT NULL,
  updated_at         TEXT NOT NULL,
  UNIQUE (project_id, path)
)
```

### `nodes_fts`

```sql
CREATE VIRTUAL TABLE nodes_fts USING fts5(
  title,
  description,
  content='nodes',
  content_rowid='id',
  tokenize='unicode61 remove_diacritics 2'
)
```

### `notes`

```sql
CREATE TABLE notes (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  node_id     INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
  title       TEXT NOT NULL,
  content     TEXT NOT NULL,
  note_type   TEXT NOT NULL DEFAULT 'general'
                CHECK (note_type IN ('general','decision','context','meeting',
                                     'technical','blocker','progress','release')),
  tags        TEXT NOT NULL DEFAULT '[]',
  metadata    TEXT NOT NULL DEFAULT '{}',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
)
```

### `notes_fts`

```sql
CREATE VIRTUAL TABLE notes_fts USING fts5(
  title,
  content,
  content='notes',
  content_rowid='id',
  tokenize='unicode61 remove_diacritics 2'
)
```

### `projects`

```sql
CREATE TABLE projects (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  key         TEXT NOT NULL UNIQUE,
  name        TEXT NOT NULL,
  description TEXT,
  status      TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active','on_hold','completed','archived')),
  timezone    TEXT NOT NULL DEFAULT 'UTC',
  tags        TEXT NOT NULL DEFAULT '[]',
  metadata    TEXT NOT NULL DEFAULT '{}',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
)
```

### `reservations`

```sql
CREATE TABLE reservations (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  attempt_id  INTEGER NOT NULL REFERENCES attempts(id) ON DELETE CASCADE,
  node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL CHECK (kind IN ('path','glob','logical')),
  value       TEXT NOT NULL,
  mode        TEXT NOT NULL DEFAULT 'exclusive' CHECK (mode IN ('exclusive','shared')),
  created_at  TEXT NOT NULL,
  released_at TEXT
)
```

### `sessions`

```sql
CREATE TABLE sessions (
  id           TEXT PRIMARY KEY,
  holder       TEXT NOT NULL,
  project_key  TEXT REFERENCES projects(key) ON DELETE SET NULL,
  created_at   TEXT NOT NULL,
  last_seen_at TEXT NOT NULL
)
```

### `statuses`

```sql
CREATE TABLE statuses (
  name     TEXT PRIMARY KEY,
  rank     INTEGER NOT NULL UNIQUE,
  terminal INTEGER NOT NULL DEFAULT 0 CHECK (terminal IN (0, 1))
)
```

### `templates`

```sql
CREATE TABLE templates (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id  INTEGER REFERENCES projects(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  description TEXT,
  tree        TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  UNIQUE (project_id, name)
)
```

## Представления

### `v_attempt_hours`

```sql
CREATE VIEW v_attempt_hours AS
SELECT
  node_id,
  SUM((julianday(COALESCE(ended_at, strftime('%Y-%m-%dT%H:%M:%fZ','now'))) - julianday(started_at))
      * 24.0) AS hours
FROM attempts
GROUP BY node_id
```

### `v_blockers`

```sql
CREATE VIEW v_blockers AS
SELECT
  e.from_node                    AS node_id,
  COUNT(*)                       AS blocker_count,
  group_concat(b.path, ',')      AS blocker_paths
FROM edges e
JOIN nodes b ON b.id = e.to_node
WHERE e.kind = 'blocks'
  AND b.deleted_at IS NULL
  AND b.status NOT IN ('done','cancelled')
GROUP BY e.from_node
```

### `v_node_effective`

```sql
CREATE VIEW v_node_effective AS
SELECT
  n.*,
  CASE
    WHEN n.status IN ('done','cancelled') THEN n.status
    -- Живая аренда: работа идёт прямо сейчас.
    WHEN EXISTS (
      SELECT 1 FROM attempts a
      WHERE a.node_id = n.id AND a.ended_at IS NULL
        AND a.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
    ) THEN 'in_progress'
    -- Аренда истекла, но попытку никто не закрыл: работа не идёт. Узел возвращается в очередь
    -- в тот статус, который допустим для его вида.
    WHEN n.status = 'in_progress' THEN
      CASE WHEN n.kind = 'epic' THEN 'planned' ELSE 'todo' END
    WHEN n.status = 'blocked' OR EXISTS (
      SELECT 1 FROM edges e
      JOIN nodes b ON b.id = e.to_node
      WHERE e.from_node = n.id AND e.kind = 'blocks'
        AND b.deleted_at IS NULL AND b.status NOT IN ('done','cancelled')
    ) THEN 'blocked'
    ELSE n.status
  END AS effective_status,
  -- Только живая попытка: истёкшая аренда для читателя не существует.
  (SELECT a.expires_at FROM attempts a
    WHERE a.node_id = n.id AND a.ended_at IS NULL
      AND a.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
    ORDER BY a.id DESC LIMIT 1) AS lease_expires_at,
  (SELECT a.holder FROM attempts a
    WHERE a.node_id = n.id AND a.ended_at IS NULL
      AND a.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
    ORDER BY a.id DESC LIMIT 1) AS lease_holder
FROM nodes n
```

### `v_node_hours`

```sql
CREATE VIEW v_node_hours AS
SELECT n.id AS node_id, n.path, n.estimated_hours,
       COALESCE(h.hours, 0.0) AS actual_hours
FROM nodes n
LEFT JOIN v_attempt_hours h ON h.node_id = n.id
WHERE n.deleted_at IS NULL
```

### `v_node_progress`

```sql
CREATE VIEW v_node_progress AS
WITH RECURSIVE tree(ancestor_id, node_id) AS (
  SELECT id, id FROM nodes
  UNION ALL
  SELECT t.ancestor_id, n.id FROM tree t JOIN nodes n ON n.parent_id = t.node_id
)
SELECT
  t.ancestor_id                                                       AS node_id,
  COUNT(*)                                                            AS total_count,
  SUM(CASE WHEN n.status = 'done' THEN 1 ELSE 0 END)                  AS done_count,
  SUM(n.weight)                                                       AS total_weight,
  SUM(CASE WHEN n.status = 'done' THEN n.weight ELSE 0 END)           AS done_weight
FROM tree t
JOIN nodes n ON n.id = t.node_id AND n.deleted_at IS NULL
GROUP BY t.ancestor_id
```

### `v_overdue`

```sql
CREATE VIEW v_overdue AS
SELECT
  n.id,
  n.project_id,
  n.path,
  n.due_date,
  CAST(julianday(date('now')) - julianday(n.due_date) AS INTEGER) AS days_overdue
FROM nodes n
WHERE n.due_date IS NOT NULL
  AND n.deleted_at IS NULL
  AND n.due_date < date('now')
  AND n.status NOT IN ('done','cancelled')
```

## Индексы

### `idx_activity_attempt`

```sql
CREATE INDEX idx_activity_attempt ON activity_log(attempt_id) WHERE attempt_id IS NOT NULL
```

### `idx_activity_node`

```sql
CREATE INDEX idx_activity_node    ON activity_log(node_id, created_at)
```

### `idx_activity_project`

```sql
CREATE INDEX idx_activity_project ON activity_log(project_id, created_at)
```

### `idx_attempts_exp`

```sql
CREATE INDEX idx_attempts_exp ON attempts(expires_at) WHERE ended_at IS NULL
```

### `idx_attempts_session`

```sql
CREATE INDEX idx_attempts_session ON attempts(session_id) WHERE session_id IS NOT NULL
```

### `idx_comments_node`

```sql
CREATE INDEX idx_comments_node ON comments(node_id, created_at)
```

### `idx_edges_to`

```sql
CREATE INDEX idx_edges_to ON edges(to_node, kind)
```

### `idx_nodes_assignee`

```sql
CREATE INDEX idx_nodes_assignee       ON nodes(assigned_to) WHERE assigned_to IS NOT NULL
```

### `idx_nodes_due`

```sql
CREATE INDEX idx_nodes_due            ON nodes(due_date) WHERE due_date IS NOT NULL
```

### `idx_nodes_parent`

```sql
CREATE INDEX idx_nodes_parent         ON nodes(parent_id, sort_order)
```

### `idx_nodes_project_status`

```sql
CREATE INDEX idx_nodes_project_status ON nodes(project_id, status)
```

### `idx_notes_entity`

```sql
CREATE INDEX idx_notes_entity ON notes(project_id, node_id)
```

### `idx_reserv_active`

```sql
CREATE INDEX idx_reserv_active ON reservations(kind, value) WHERE released_at IS NULL
```

### `idx_sessions_project`

```sql
CREATE INDEX idx_sessions_project ON sessions(project_key, last_seen_at)
```

### `idx_status_history_node`

```sql
CREATE INDEX idx_status_history_node ON node_status_history(node_id, at)
```

### `idx_status_history_to`

```sql
CREATE INDEX idx_status_history_to ON node_status_history(to_status, at)
```

### `uq_attempts_live`

```sql
CREATE UNIQUE INDEX uq_attempts_live ON attempts(node_id) WHERE ended_at IS NULL
```

