-- tasq schema v3: воронка idea → backlog → todo/planned
--
-- SQLite не умеет менять CHECK: единственный способ расширить допустимый набор статусов —
-- пересобрать таблицу. Пересборка идёт по штатной процедуре (create/copy/drop/rename), поэтому:
--   * `PRAGMA foreign_keys` выключен РАНЬШЕ транзакции (в раннере): внутри транзакции он молчит,
--     а с включёнными ключами DROP TABLE nodes выполнил бы неявное DELETE и каскадом снёс
--     attempts/edges/comments/notes;
--   * `PRAGMA legacy_alter_table` включён на время переименования: без него SQLite разбирает
--     ссылки в представлениях, а они в этот момент указывают на уже удалённую таблицу.
--
-- Идентификаторы строк сохраняются (явный список колонок), поэтому рёбра, попытки и заметки
-- остаются привязанными к своим узлам; FTS-индекс перестраивается из новой таблицы.

PRAGMA legacy_alter_table = ON;

CREATE TABLE nodes_v3 (
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
                       CHECK (status IN ('idea','backlog','planned','todo','in_progress',
                                         'blocked','review','done','cancelled','deferred')),
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

INSERT INTO nodes_v3 (
  id, project_id, parent_id, depth, seq, path, kind, title, description, description_locked,
  status, priority, assigned_to, weight, estimated_hours, due_date, branch, source_ref, tags,
  metadata, content_hash, sort_order, archived_at, deleted_at, created_at, updated_at
)
SELECT
  id, project_id, parent_id, depth, seq, path, kind, title, description, description_locked,
  status, priority, assigned_to, weight, estimated_hours, due_date, branch, source_ref, tags,
  metadata, content_hash, sort_order, archived_at, deleted_at, created_at, updated_at
FROM nodes;

DROP TABLE nodes;

ALTER TABLE nodes_v3 RENAME TO nodes;

-- Индексы и триггеры уходят вместе со старой таблицей: их надо вернуть теми же именами.
CREATE INDEX idx_nodes_project_status ON nodes(project_id, status);
CREATE INDEX idx_nodes_parent         ON nodes(parent_id, sort_order);
CREATE INDEX idx_nodes_due            ON nodes(due_date) WHERE due_date IS NOT NULL;
CREATE INDEX idx_nodes_assignee       ON nodes(assigned_to) WHERE assigned_to IS NOT NULL;

CREATE TRIGGER IF NOT EXISTS nodes_fts_ai AFTER INSERT ON nodes BEGIN
  INSERT INTO nodes_fts(rowid, title, description)
  VALUES (new.id, new.title, COALESCE(new.description, ''));
END;
CREATE TRIGGER IF NOT EXISTS nodes_fts_ad AFTER DELETE ON nodes BEGIN
  INSERT INTO nodes_fts(nodes_fts, rowid, title, description)
  VALUES ('delete', old.id, old.title, COALESCE(old.description, ''));
END;
CREATE TRIGGER IF NOT EXISTS nodes_fts_au AFTER UPDATE OF title, description ON nodes BEGIN
  INSERT INTO nodes_fts(nodes_fts, rowid, title, description)
  VALUES ('delete', old.id, old.title, COALESCE(old.description, ''));
  INSERT INTO nodes_fts(rowid, title, description)
  VALUES (new.id, new.title, COALESCE(new.description, ''));
END;

-- Внешний контент FTS перечитывается из новой таблицы: индекс не должен зависеть от того,
-- что shadow-таблицы пережили DROP.
INSERT INTO nodes_fts(nodes_fts) VALUES ('rebuild');

PRAGMA legacy_alter_table = OFF;
