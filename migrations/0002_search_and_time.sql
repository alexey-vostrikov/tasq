-- tasq schema v2: полнотекстовый поиск и учёт времени
--
-- FTS5 включён в bundled-сборку SQLite (libsqlite3-sys собирает с -DSQLITE_ENABLE_FTS5),
-- поэтому отдельный крейт поиска не нужен. Индексы наполняются триггерами: расхождение
-- индекса и таблицы — единственный класс ошибок, который здесь реально возможен.

CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
  title,
  description,
  content='nodes',
  content_rowid='id',
  tokenize='unicode61 remove_diacritics 2'
);

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

CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
  title,
  content,
  content='notes',
  content_rowid='id',
  tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS notes_fts_ai AFTER INSERT ON notes BEGIN
  INSERT INTO notes_fts(rowid, title, content) VALUES (new.id, new.title, new.content);
END;

CREATE TRIGGER IF NOT EXISTS notes_fts_ad AFTER DELETE ON notes BEGIN
  INSERT INTO notes_fts(notes_fts, rowid, title, content)
  VALUES ('delete', old.id, old.title, old.content);
END;

CREATE TRIGGER IF NOT EXISTS notes_fts_au AFTER UPDATE OF title, content ON notes BEGIN
  INSERT INTO notes_fts(notes_fts, rowid, title, content)
  VALUES ('delete', old.id, old.title, old.content);
  INSERT INTO notes_fts(rowid, title, content) VALUES (new.id, new.title, new.content);
END;

-- Заполнение индексов уже существующими строками (для баз, созданных до v2).
INSERT INTO nodes_fts(rowid, title, description)
  SELECT id, title, COALESCE(description, '') FROM nodes
  WHERE id NOT IN (SELECT rowid FROM nodes_fts);

INSERT INTO notes_fts(rowid, title, content)
  SELECT id, title, content FROM notes
  WHERE id NOT IN (SELECT rowid FROM notes_fts);

-- Учёт времени: суммарные часы по попыткам, материализованные для дешёвого чтения.
CREATE VIEW IF NOT EXISTS v_node_hours AS
SELECT n.id AS node_id, n.path, n.estimated_hours,
       COALESCE(h.hours, 0.0) AS actual_hours
FROM nodes n
LEFT JOIN v_attempt_hours h ON h.node_id = n.id
WHERE n.deleted_at IS NULL;
