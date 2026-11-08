-- tasq schema v1
-- Источник истины — README.md §5. Одна таблица nodes на все уровни дерева.

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

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
);

CREATE TABLE nodes (
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
  status             TEXT NOT NULL
                       CHECK (status IN ('planned','todo','in_progress','blocked','review',
                                         'done','cancelled','deferred')),
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
);
CREATE INDEX idx_nodes_project_status ON nodes(project_id, status);
CREATE INDEX idx_nodes_parent         ON nodes(parent_id, sort_order);
CREATE INDEX idx_nodes_due            ON nodes(due_date) WHERE due_date IS NOT NULL;
CREATE INDEX idx_nodes_assignee       ON nodes(assigned_to) WHERE assigned_to IS NOT NULL;

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
  checkpoint  TEXT,
  UNIQUE (node_id, attempt_no)
);
-- одна живая попытка на узел (README §5.3, инвариант 6)
CREATE UNIQUE INDEX uq_attempts_live ON attempts(node_id) WHERE ended_at IS NULL;
CREATE INDEX idx_attempts_exp ON attempts(expires_at) WHERE ended_at IS NULL;

CREATE TABLE reservations (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  attempt_id  INTEGER NOT NULL REFERENCES attempts(id) ON DELETE CASCADE,
  node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL CHECK (kind IN ('path','glob','logical')),
  value       TEXT NOT NULL,
  mode        TEXT NOT NULL DEFAULT 'exclusive' CHECK (mode IN ('exclusive','shared')),
  created_at  TEXT NOT NULL,
  released_at TEXT
);
CREATE INDEX idx_reserv_active ON reservations(kind, value) WHERE released_at IS NULL;

CREATE TABLE edges (
  from_node  INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  to_node    INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL
               CHECK (kind IN ('blocks','related','duplicates','discovered_from','split_from')),
  created_at TEXT NOT NULL,
  created_by TEXT,
  PRIMARY KEY (from_node, to_node, kind),
  CHECK (from_node <> to_node)
);
CREATE INDEX idx_edges_to ON edges(to_node, kind);

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
);
CREATE INDEX idx_comments_node ON comments(node_id, created_at);

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
);
CREATE INDEX idx_notes_entity ON notes(project_id, node_id);

CREATE TABLE templates (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id  INTEGER REFERENCES projects(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  description TEXT,
  tree        TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  UNIQUE (project_id, name)
);

CREATE TABLE activity_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  node_id     INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
  entity_type TEXT NOT NULL,
  entity_id   INTEGER NOT NULL,
  action      TEXT NOT NULL,
  field_name  TEXT,
  old_value   TEXT,
  new_value   TEXT,
  summary     TEXT,
  actor       TEXT,
  attempt_id  INTEGER,
  created_at  TEXT NOT NULL
);
CREATE INDEX idx_activity_project ON activity_log(project_id, created_at);
CREATE INDEX idx_activity_node    ON activity_log(node_id, created_at);

CREATE TABLE burn_snapshots (
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  day         TEXT NOT NULL,
  metric      TEXT NOT NULL CHECK (metric IN ('count','points','hours')),
  scope_total REAL NOT NULL,
  scope_done  REAL NOT NULL,
  scope_added REAL NOT NULL,
  computed_at TEXT NOT NULL,
  PRIMARY KEY (project_id, day, metric)
);

-- Представления (README §5.2). «Сейчас» — UTC; точный расчёт по часовому поясу проекта делает CLI.

CREATE VIEW v_node_effective AS
SELECT
  n.*,
  CASE
    WHEN n.status IN ('done','cancelled') THEN n.status
    WHEN EXISTS (
      SELECT 1 FROM attempts a
      WHERE a.node_id = n.id AND a.ended_at IS NULL
        AND a.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
    ) THEN 'in_progress'
    WHEN n.status = 'blocked' OR EXISTS (
      SELECT 1 FROM edges e
      JOIN nodes b ON b.id = e.to_node
      WHERE e.from_node = n.id AND e.kind = 'blocks'
        AND b.deleted_at IS NULL AND b.status NOT IN ('done','cancelled')
    ) THEN 'blocked'
    ELSE n.status
  END AS effective_status,
  (SELECT a.expires_at FROM attempts a
    WHERE a.node_id = n.id AND a.ended_at IS NULL
    ORDER BY a.id DESC LIMIT 1) AS lease_expires_at,
  (SELECT a.holder FROM attempts a
    WHERE a.node_id = n.id AND a.ended_at IS NULL
    ORDER BY a.id DESC LIMIT 1) AS lease_holder
FROM nodes n;

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
GROUP BY t.ancestor_id;

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
GROUP BY e.from_node;

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
  AND n.status NOT IN ('done','cancelled');

CREATE VIEW v_attempt_hours AS
SELECT
  node_id,
  SUM((julianday(COALESCE(ended_at, strftime('%Y-%m-%dT%H:%M:%fZ','now'))) - julianday(started_at))
      * 24.0) AS hours
FROM attempts
GROUP BY node_id;
