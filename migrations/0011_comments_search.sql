-- Схема v11: комментарии попадают в поиск.
--
-- Зачем. `find` искал по заголовкам и описаниям узлов и по заметкам, а комментарии — нет. При
-- этом именно в комментариях живут обоснования решений: на живой базе 250 комментариев против
-- нуля заметок. То есть поиск не находил самое ценное содержимое базы — и отвечал «ничего не
-- найдено» на вопрос, ответ на который в базе есть.
--
-- Индексируется только текст: автор и время ответом поиска не являются, а удалённый комментарий
-- (мягкое удаление, `is_deleted = 1`) остаётся в индексе намеренно — он часть истории, и его
-- находят так же, как находят архивный узел.
--
-- `content='comments'` (внешнее содержимое) — как у остальных индексов: текст не дублируется,
-- сравнение идёт с живой таблицей, а пересборка делается одной командой `rebuild`.

CREATE VIRTUAL TABLE IF NOT EXISTS comments_fts USING fts5(
  content,
  content='comments',
  content_rowid='id',
  tokenize='unicode61 remove_diacritics 2'
);

-- Триггеры на все три пути записи: вставка, удаление строки и правка текста. Пропущенный путь
-- делает индекс тихо неполным — ровно так и вышло с `nodes_fts` при импорте проекта, пока его
-- не начали перестраивать явно.
CREATE TRIGGER IF NOT EXISTS comments_fts_ai AFTER INSERT ON comments BEGIN
  INSERT INTO comments_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS comments_fts_ad AFTER DELETE ON comments BEGIN
  INSERT INTO comments_fts(comments_fts, rowid, content)
  VALUES ('delete', old.id, old.content);
END;

CREATE TRIGGER IF NOT EXISTS comments_fts_au AFTER UPDATE OF content ON comments BEGIN
  INSERT INTO comments_fts(comments_fts, rowid, content)
  VALUES ('delete', old.id, old.content);
  INSERT INTO comments_fts(rowid, content) VALUES (new.id, new.content);
END;

-- Индекс строится по уже существующим комментариям: без этого поиск находил бы только то, что
-- написали после обновления, и выглядел бы «работающим наполовину».
INSERT INTO comments_fts(comments_fts) VALUES ('rebuild');
