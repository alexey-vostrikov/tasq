-- tasq schema v4: справочник статусов, история переходов, сессии, строгий entity_type
--
-- Что меняется и почему (docs/storage-redesign.md, решения по трём открытым вопросам):
--
--   1. `nodes.status` больше не держится списком в CHECK, а ссылается на таблицу `statuses`.
--      CHECK неизменяем: каждая правка набора статусов — пересборка `nodes` (миграция v3 стоила
--      120 строк SQL и четырёх подводных камней). Новый статус теперь INSERT в справочник.
--      Матрица переходов при этом остаётся в КОДЕ (`status::can_transition`) — это правило
--      проверки, а не данные: данные не тестируются, они лежат. В справочнике живёт только то,
--      что читают запросы — порядок воронки (`rank`) и признак закрытости (`terminal`).
--
--   2. `node_status_history` — факт перехода статуса отдельной таблицей. До сих пор `burnup`
--      вынимал дату закрытия из `activity_log` по паре (field_name='status', new_value='done'),
--      и ряд держался на совпадении текста в ПЯТИ местах записи (node::update, lease::claim,
--      lease::release, edge::unblock, edge::gc). Шестой путь (import, restore) молча выпадал из
--      ряда — и выпал: импорт проекта обнуляет burn-up, это измерено.
--
--   3. `sessions` + `attempts.session_id` — идентичность сессии, которой не было. `attempts.holder`
--      это имя агента: два параллельных «hermes» склеивались в одну сущность. Без id сессии
--      нельзя ни привязать `next` к проекту, ни отличить свои аренды от чужих.
--
--   4. `activity_log.entity_type` получает CHECK: опечатка в коде («coment») раньше жила в базе
--      годами и не отбивалась.
--
-- Процедура пересборки таблиц — та же, что в v3, и по тем же причинам:
--   * `PRAGMA foreign_keys` выключен РАНЬШЕ транзакции (раннер): внутри транзакции он молчит,
--     а с включёнными ключами DROP TABLE nodes выполнил бы неявное DELETE и каскадом снёс
--     attempts, reservations, edges, comments, notes, activity_log;
--   * `PRAGMA legacy_alter_table` включён на время переименования: без него SQLite разбирает
--     ссылки в представлениях, а они в этот момент указывают на уже удалённую таблицу.

PRAGMA legacy_alter_table = ON;

-- ---------------------------------------------------------------------------
-- 1. Справочник статусов
-- ---------------------------------------------------------------------------
-- `rank` — порядок воронки, а не алфавит: по нему строится доска и сортировка в отчётах.
-- Порядок обязан совпадать с `status::Status::all()`; сверка — тестом, иначе справочник и код
-- разъедутся так же молча, как разъехались README и матрица переходов.
CREATE TABLE statuses (
  name     TEXT PRIMARY KEY,
  rank     INTEGER NOT NULL UNIQUE,
  terminal INTEGER NOT NULL DEFAULT 0 CHECK (terminal IN (0, 1))
);

INSERT INTO statuses (name, rank, terminal) VALUES
  ('idea',        1, 0),
  ('backlog',     2, 0),
  ('planned',     3, 0),
  ('todo',        4, 0),
  ('in_progress', 5, 0),
  ('blocked',     6, 0),
  ('review',      7, 0),
  ('deferred',    8, 0),
  ('done',        9, 1),
  ('cancelled',  10, 1);

-- ---------------------------------------------------------------------------
-- 2. Пересборка `nodes`: CHECK → ссылка на справочник
-- ---------------------------------------------------------------------------
CREATE TABLE nodes_v4 (
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
);

INSERT INTO nodes_v4 (
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

ALTER TABLE nodes_v4 RENAME TO nodes;

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

-- ---------------------------------------------------------------------------
-- 3. История статусов
-- ---------------------------------------------------------------------------
-- Журнал отвечает на вопрос «что произошло» и остаётся журналом; эта таблица отвечает на
-- вопрос «когда узел стал done» и ничего больше не значит. `cause` — не украшение: без него
-- нельзя отличить ручной переход от закрытия через аренду, а `burnup` и отчёты опираются
-- именно на это различие.
--
-- `from_status` допускает NULL: у первой записи узла предыдущего статуса нет.
CREATE TABLE node_status_history (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  from_status TEXT REFERENCES statuses(name),
  to_status   TEXT NOT NULL REFERENCES statuses(name),
  cause       TEXT NOT NULL CHECK (cause IN ('manual','lease_claimed','lease_completed',
                                             'lease_released','unblock','gc','import','restore')),
  at          TEXT NOT NULL,
  actor       TEXT
);
CREATE INDEX idx_status_history_node ON node_status_history(node_id, at);

-- Перенос истории из журнала: единственное место, откуда её можно взять задним числом.
-- Строки, чей статус не зарегистрирован в справочнике, отбрасываются — иначе ссылка на
-- несуществующий статус отбила бы всю миграцию на чужих данных.
--
-- Причина выводится из действия: журнал писал её в `action`, а не отдельным полем.
INSERT INTO node_status_history (node_id, from_status, to_status, cause, at, actor)
SELECT
  l.node_id,
  (SELECT s.name FROM statuses s WHERE s.name = l.old_value),
  l.new_value,
  CASE
    WHEN l.action = 'claim'                 THEN 'lease_claimed'
    WHEN l.action = 'completed'             THEN 'lease_completed'
    WHEN l.action IN ('released', 'expire') THEN 'lease_released'
    WHEN l.action = 'unblock' AND l.actor = 'gc' THEN 'gc'
    WHEN l.action = 'unblock'               THEN 'unblock'
    WHEN l.action = 'import'                THEN 'import'
    ELSE 'manual'
  END,
  l.created_at,
  l.actor
FROM activity_log l
WHERE l.field_name = 'status'
  AND l.node_id IS NOT NULL
  AND l.new_value IN (SELECT name FROM statuses)
ORDER BY l.created_at, l.id;

-- ---------------------------------------------------------------------------
-- 4. Сессии
-- ---------------------------------------------------------------------------
-- Работающая сессия: кто (holder) и в каком проекте она занята (`project_key` — привязка,
-- которую выставляет первый успешный claim). `last_seen_at` нужен, чтобы брошенные сессии
-- можно было отличить от живых, не заводя им TTL отдельным механизмом.
CREATE TABLE sessions (
  id           TEXT PRIMARY KEY,
  holder       TEXT NOT NULL,
  project_key  TEXT REFERENCES projects(key) ON DELETE SET NULL,
  created_at   TEXT NOT NULL,
  last_seen_at TEXT NOT NULL
);
CREATE INDEX idx_sessions_project ON sessions(project_key, last_seen_at);

-- Аренда помнит сессию: без этого нельзя ответить «что эта сессия взяла» и «какой проект
-- она заняла». NULL допустим: аренды, созданные до v4, сессии не имеют, и придумывать её
-- задним числом — значит выдать догадку за факт.
ALTER TABLE attempts ADD COLUMN session_id TEXT REFERENCES sessions(id);
CREATE INDEX idx_attempts_session ON attempts(session_id) WHERE session_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- 5. Строгий `entity_type` в журнале
-- ---------------------------------------------------------------------------
-- Пересборка нужна потому, что CHECK неизменяем: добавить ограничение к существующей таблице
-- SQLite не умеет. Список — ровно те значения, которые пишет код; «на будущее» он не
-- расширяется, иначе проверка перестаёт что-либо ловить.
CREATE TABLE activity_log_v4 (
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
);

INSERT INTO activity_log_v4 (
  id, project_id, node_id, entity_type, entity_id, action, field_name, old_value, new_value,
  summary, actor, attempt_id, created_at
)
SELECT
  id, project_id, node_id, entity_type, entity_id, action, field_name, old_value, new_value,
  summary, actor, attempt_id, created_at
FROM activity_log;

DROP TABLE activity_log;

ALTER TABLE activity_log_v4 RENAME TO activity_log;

CREATE INDEX idx_activity_project ON activity_log(project_id, created_at);
CREATE INDEX idx_activity_node    ON activity_log(node_id, created_at);
CREATE INDEX idx_activity_attempt ON activity_log(attempt_id) WHERE attempt_id IS NOT NULL;

PRAGMA legacy_alter_table = OFF;
