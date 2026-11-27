# tasq — нормативная часть

Это раздел спецификации: модель данных, правила статусов, аренды, MCP-поверхность, надёжность и
коды ошибок. Вход в проект — [`README.md`](../README.md); справочники (`cli.md`, `mcp-tools.md`,
`schema.md`) генерируются из бинаря.

Нумерация разделов сохранена от прежнего единого README: ссылки «§17.3» в коде и комментариях
указывают сюда.

---

## 5. Модель данных

Один файл на проект: `.tasq/tasq.db` (SQLite 3, WAL). Один файл может содержать несколько проектов —
как в saga; обнаружение идёт поиском `.tasq/` вверх от текущего каталога, переопределяется `--db` и
переменной `TASQ_DB`.

Принципы:

- **Хранится только факт, не вывод.** `in_progress` из аренды, прогресс из детей, `blocked` из рёбер,
  `actual_hours` из попыток — это представления, а не колонки, которые кто-то забыл обновить.
- **Мягкое удаление вместо `DELETE`** для узлов и комментариев: строка остаётся ради аудита, скрывается
  из выдач (`deleted_at IS NULL`), восстанавливается парным инструментом.
- **Один писатель.** Запись — только в `BEGIN IMMEDIATE`, `busy_timeout = 200` мс, три повтора на `SQLITE_BUSY`
  с разбросом; ждать блокировку дольше оказалось медленнее, чем отказаться и повторить (§17.3).
- **Время — UTC ISO-8601 с миллисекундами** (`2026-11-08T05:20:31.412Z`). Даты (`due_date`) — `YYYY-MM-DD`
  в часовом поясе проекта (`projects.timezone`, по умолчанию `UTC`).

### 5.1. Схема (v1)

**Версия схемы 2** (миграция `0002_search_and_time.sql`, M5) добавляет полнотекстовые индексы
`nodes_fts`/`notes_fts` (FTS5, внешнее содержимое, наполняются триггерами) и представление
`v_node_hours` для учёта времени. Базы v1 достраиваются автоматически при открытии.

Колонка `status` ссылается на справочник `statuses`; какие из статусов допустимы для конкретного
`kind` и какие переходы разрешены — решает код по матрице §7 и проверяется тестами. Справочник —
страховка от мусора, а не замена правилам.

**Версия схемы 3** (`0003_status_funnel.sql`) добавила воронку `idea → backlog → todo/planned`.
**Версия схемы 4** (`0004_status_registry.sql`) выносит набор статусов в справочник, добавляет
историю переходов и сессии — см. §5.1.1.

```sql
-- Служебное
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);   -- schema_version, created_by_version

CREATE TABLE projects (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  key         TEXT NOT NULL UNIQUE,          -- TASQ, API, INFRA: [A-Z0-9]{2,8}, неизменяем
  name        TEXT NOT NULL,
  description TEXT,
  status      TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active','on_hold','completed','archived')),
  timezone    TEXT NOT NULL DEFAULT 'UTC',
  tags        TEXT NOT NULL DEFAULT '[]',    -- JSON-массив
  metadata    TEXT NOT NULL DEFAULT '{}',    -- JSON-объект
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);

-- Дерево работы: одна таблица на все уровни
CREATE TABLE nodes (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id         INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  parent_id          INTEGER REFERENCES nodes(id) ON DELETE CASCADE,  -- NULL = корень проекта
  depth              INTEGER NOT NULL,       -- 1 = эпик, 2 = задача, 3+ = подзадача
  seq                INTEGER NOT NULL,       -- номер среди сиблингов, не переиспользуется
  path               TEXT NOT NULL,          -- "12.3.1"
  kind               TEXT NOT NULL           -- epic|task|subtask|micro (по умолчанию из depth)
                       CHECK (kind IN ('epic','task','subtask','micro')),
  title              TEXT NOT NULL,
  description        TEXT,
  description_locked INTEGER NOT NULL DEFAULT 0,
  -- С v4 статус — ссылка на справочник, а не список в CHECK: набор статусов меняется
  -- INSERT-ом, без пересборки таблицы (см. §5.1.1).
  status             TEXT NOT NULL REFERENCES statuses(name),
  priority           TEXT NOT NULL DEFAULT 'medium'
                       CHECK (priority IN ('low','medium','high','critical')),
  assigned_to        TEXT,
  weight             REAL NOT NULL DEFAULT 1,   -- очки для burn-up
  estimated_hours    REAL,
  due_date           TEXT,                      -- YYYY-MM-DD
  branch             TEXT,
  source_ref         TEXT,                      -- путь:строка / URL / коммит
  tags               TEXT NOT NULL DEFAULT '[]',
  metadata           TEXT NOT NULL DEFAULT '{}',
  content_hash       TEXT,                      -- SHA-256 нормализованного description
  sort_order         INTEGER NOT NULL DEFAULT 0,
  archived_at        TEXT,
  deleted_at         TEXT,
  created_at         TEXT NOT NULL,
  updated_at         TEXT NOT NULL,
  UNIQUE (project_id, path)
);
CREATE INDEX idx_nodes_project_status ON nodes(project_id, status);
CREATE INDEX idx_nodes_parent          ON nodes(parent_id, sort_order);
CREATE INDEX idx_nodes_due             ON nodes(due_date) WHERE due_date IS NOT NULL;
CREATE INDEX idx_nodes_assignee        ON nodes(assigned_to) WHERE assigned_to IS NOT NULL;

-- Попытки и аренды
CREATE TABLE attempts (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  attempt_no  INTEGER NOT NULL,              -- 1,2,3… на узел
  holder      TEXT NOT NULL,
  token       TEXT NOT NULL UNIQUE,          -- непрозрачный, для renew/release
  ttl_seconds INTEGER NOT NULL,
  started_at  TEXT NOT NULL,
  expires_at  TEXT NOT NULL,
  renewed_at  TEXT,
  ended_at    TEXT,                          -- NULL = попытка жива
  end_reason  TEXT CHECK (end_reason IN ('released','expired','failed','completed','superseded')),
  checkpoint  TEXT,                          -- чекпойнт предыдущей попытки на момент захвата
  UNIQUE (node_id, attempt_no)
);
CREATE INDEX idx_attempts_open ON attempts(node_id) WHERE ended_at IS NULL;
-- гарантия «одна живая попытка на узел» (инвариант 6):
CREATE UNIQUE INDEX uq_attempts_live ON attempts(node_id) WHERE ended_at IS NULL;
CREATE INDEX idx_attempts_exp  ON attempts(expires_at) WHERE ended_at IS NULL;

-- Ресурсы, зарезервированные попыткой
CREATE TABLE reservations (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  attempt_id INTEGER NOT NULL REFERENCES attempts(id) ON DELETE CASCADE,
  node_id    INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('path','glob','logical')),
  value      TEXT NOT NULL,                  -- "src/api/", "src/**/*.rs", "port:8080"
  mode       TEXT NOT NULL DEFAULT 'exclusive' CHECK (mode IN ('exclusive','shared')),
  created_at TEXT NOT NULL,
  released_at TEXT
);
CREATE INDEX idx_reserv_active ON reservations(kind, value) WHERE released_at IS NULL;

-- Зависимости сквозь границы вложенности
CREATE TABLE edges (
  from_node  INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  to_node    INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('blocks','related','duplicates','discovered_from','split_from')),
  created_at TEXT NOT NULL,
  created_by TEXT,
  PRIMARY KEY (from_node, to_node, kind),
  CHECK (from_node <> to_node)
);
CREATE INDEX idx_edges_to ON edges(to_node, kind);

-- Комментарии (мягкое удаление)
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
-- С v11 текст комментария попадает в поиск (`comments_fts`, миграция 0011): индексируется только
-- содержание — автор и время ответом поиска не являются. Мягко удалённый комментарий остаётся
-- находимым: он часть истории, как архивный узел.

-- Заметки: решения, контекст, встречи, блокеры, прогресс, релизы
CREATE TABLE notes (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id    INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  node_id       INTEGER REFERENCES nodes(id) ON DELETE CASCADE,   -- NULL = заметка проекта
  title         TEXT NOT NULL,
  content       TEXT NOT NULL,
  note_type     TEXT NOT NULL DEFAULT 'general'
                  CHECK (note_type IN ('general','decision','context','meeting',
                                       'technical','blocker','progress','release')),
  tags          TEXT NOT NULL DEFAULT '[]',
  metadata      TEXT NOT NULL DEFAULT '{}',
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL
);

-- Шаблоны: дерево задач с {переменными}
CREATE TABLE templates (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id  INTEGER REFERENCES projects(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  description TEXT,
  tree        TEXT NOT NULL,                 -- JSON-описание дерева узлов
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  UNIQUE (project_id, name)
);
```

**Готовые шаблоны лежат в репозитории, в каталоге `templates/`.** Это не примеры, а запись
процесса: каждый выведен из истории этого же трекера — из эпиков, которые были пройдены
(`measure-gate` из TASQDEV-18 и 5, `protocol-defect` из RRL «воспроизвести детерминированно, отказ
чередуется», `schema-migration` из TASQDEV-2 и 11). Файл содержит ровно то, что принимает
`template save --file`, имя берётся из имени файла, переменные — из `{скобок}`; `kind` в файлах не
указан, потому что вид выводится из глубины при применении, и один шаблон годится и в корень, и
под задачу; объявленный `kind` — подсказка о замысле, и расхождение с местом называется числом
«rekinded», а не отказом (проверка согласованности объявлений между собой живёт при сохранении). Что есть и откуда взято — `templates/README.md`; загрузить — `make templates
TEMPLATES_DB=…` (без `TEMPLATES_PROJECT` шаблоны становятся общими для всей базы).

Шаблон бывает общим (`project_id IS NULL`) и принадлежащим проекту. Общий доступен везде, поэтому
`tasq template ls` без `--project` показывает всё в базе и называет владельца каждого шаблона, а в
проекте видно свои и общие. Проект вправе переопределить общий шаблон одноимённым — переопределение
побеждает; удалить общий из проекта нельзя, для этого у него нет проекта.

Каталог и его описание сверяются тестом (`tests/templates.rs`): файл без строки в таблице, строка
без файла, переменная в документе не та, что в файле, и число узлов — всё это роняет прогон.

```sql
-- Лог активности: аудит и источник burn-up
CREATE TABLE activity_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  node_id     INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
  entity_type TEXT NOT NULL,                 -- project|node|comment|note|template|attempt|reservation
  entity_id   INTEGER NOT NULL,
  action      TEXT NOT NULL,                 -- create|update|transition|claim|renew|release|expire|...
  field_name  TEXT,
  old_value   TEXT,
  new_value   TEXT,
  summary     TEXT,
  actor       TEXT,                          -- holder для агентских действий, '-' для CLI
  attempt_id  INTEGER,
  created_at  TEXT NOT NULL
);
CREATE INDEX idx_activity_project ON activity_log(project_id, created_at);
CREATE INDEX idx_activity_node    ON activity_log(node_id, created_at);

-- Суточные снимки для burn-up
CREATE TABLE burn_snapshots (
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  day         TEXT NOT NULL,                 -- YYYY-MM-DD
  metric      TEXT NOT NULL CHECK (metric IN ('count','points','hours')),
  scope_total REAL NOT NULL,
  scope_done  REAL NOT NULL,
  scope_added REAL NOT NULL,
  computed_at TEXT NOT NULL,
  PRIMARY KEY (project_id, day, metric)
);
```

### 5.1.1. Справочник статусов, история переходов, сессии (v4)

Три изменения, у каждого — своя причина, и все три про одно: убрать из схемы места, где правило
живёт в двух экземплярах и расходится молча.

**`statuses` — справочник вместо `CHECK`.** `CHECK` неизменяем: добавление `idea`/`backlog`
потребовало пересборки `nodes` (120 строк SQL, выключение внешних ключей вне транзакции,
`legacy_alter_table`, перестройка FTS — четыре места, где ошибка рвёт данные молча). Теперь новый
статус — `INSERT` в справочник. Матрица переходов при этом **остаётся в коде**
(`status::can_transition`): это правило проверки, а данные не тестируются — они лежат. В
справочнике живёт только то, что читают запросы: порядок воронки `rank` и признак закрытости
`terminal`. Совпадение справочника с `Status::all()` проверяется тестом.

```sql
CREATE TABLE statuses (name TEXT PRIMARY KEY, rank INTEGER NOT NULL UNIQUE,
                       terminal INTEGER NOT NULL DEFAULT 0 CHECK (terminal IN (0,1)));
```

Закрытость в запросах берётся из справочника (`status::not_terminal_sql`), а не литералом
`IN ('done','cancelled')`: до этой правки такой литерал жил в девятнадцати запросах, и новый
статус пришлось бы вписывать в каждое место — одно из них забывается (так уже выпал `import`
из burn-up).

**`incomplete` в схеме нет — и это решение.** Недоведённость выводится из состояния детей
(§7), поэтому схемы она не касается: хранится только `done`, а вычисляется состояние в коде —
одним обходом поддерева на выборку (`node::unfinished_under`). Хранимая копия вывода
разошлась бы с фактом на первой же правке детей.

**`node_status_history` — факт перехода отдельно от журнала.** `burnup` вынимал дату закрытия из
`activity_log` по паре `(field_name='status', new_value='done')`, и ряд держался на совпадении
текста в пяти местах записи. Шестой путь (`import`) молча выпал — импорт проекта обнулял burn-up,
это измерено. `cause` отличает ручной переход от закрытия через аренду; история переносится
дампом как данные.

```sql
CREATE TABLE node_status_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  from_status TEXT REFERENCES statuses(name),        -- NULL у первой записи узла
  to_status   TEXT NOT NULL REFERENCES statuses(name),
  cause TEXT NOT NULL CHECK (cause IN ('manual','lease_claimed','lease_completed',
                                       'lease_released','lease_expired','unblock',
                                       'gc','import','restore')),
  at TEXT NOT NULL, actor TEXT
);
```

Пишут историю **все** пути смены статуса, и каждый — со своей причиной: `node::create`
(`created`), `node::update` (`manual`), `lease::claim` (`lease_claimed`), `lease::release`
(`lease_completed` или `lease_released`), `lease::release_expired` (`lease_expired`),
`edge::unlink` (`unblock`), `edge::clear_stale_blocked` (`gc`). Причина `lease_expired` отдельная
намеренно: истёкшая аренда — не решение исполнителя, и в отчётах «не успел» должно отличаться от
«сделал и отпустил». Создание записывается отдельной причиной, потому что это не переход, но без
него у нового узла истории нет вовсе — и он выглядит «неизвестным». Переход в тот же статус не
пишется: это не переход.

Из этого следует свойство, и оно проверяется тестом, а не намерением: у каждого узла, о котором
история что-то знает, первый факт — `created`. Проверка идёт по всей базе сразу после всех путей,
а не по одному узлу: правило живёт в одном месте, и его нарушение на любом пути обязано быть видно
в одном и том же месте. Узел, чья история начинается с середины, — это узел с обрезанной
историей, и по ней нельзя ответить, откуда он пришёл. Нарушение ловится и проверено негативным
контролем: база с историей, начатой с `manual`, свойство нарушает.

**Burn-up считает по истории, а не по журналу.** Дата закрытия берётся из первого перехода в
`done`; причина перехода не важна — важно, что он был (иначе рядом причин пришлось бы править
отчёт). Узел, у которого истории нет вовсе, попадает в `scope_unknown`, и отчёт говорит об этом
словами: `history: unknown for N of M`. Это не педантизм: «переходов не было» значит «работа не
закрыта», а «истории нет» значит «мы не знаем», и показывать второе как первое — ровно тот
молчаливый ноль, из-за которого импорт проекта обнулял ряд.

**`sessions` — идентичность сессии, которой не было.** `attempts.holder` — имя агента: два
параллельных процесса склеивались в одну сущность. `sessions.project_key` — привязка, которую
выставляет первый успешный `claim`; по ней `next` сначала предлагает свой проект. У аренд,
созданных до v4, `session_id` пуст: придумывать сессию задним числом — значит выдать догадку за
факт.

Идентификатор приходит СНАРУЖИ: MCP-процесс заводит свой при старте, CLI берёт из `TASQ_SESSION`.
Нет идентификатора — нет привязки, и `next` перечисляет проекты как есть; сессия — не
обязательное понятие, а дополнительное. Привязка ставится **взятием работы**, а не просмотром:
«работала в проекте» и «посмотрела на проект» — разные вещи, и привязка по просмотру тянула бы
сессию туда, куда она заглянула случайно.

Правило `next` с привязкой: пока в своём проекте есть что брать — предлагается только своё;
когда там пусто — остальные проекты, с явной пометкой (`nothing to pick up in your project;
showing the rest: …`). Явный `--project` сильнее привязки и её не меняет.

Кроме этого `activity_log.entity_type` получил `CHECK` на восемь значений, которые реально пишет
код (`node`, `comment`, `note`, `edge`, `project`, `template`, `batch`, `import`): опечатка вроде
`coment` раньше жила в базе годами.

### 5.2. Представления

- `v_node_effective` — узел + эффективный статус:
  `in_progress`, если есть незакрытая попытка с `expires_at > now`;
  иначе `blocked`, если есть незакрытые `blocks`-рёбра на незавершённые узлы или статус `blocked` выставлен руками;
  иначе хранимый статус.
- `v_node_progress` — доля завершённых детей (рекурсивно, `WITH RECURSIVE`), по штукам и по `weight`.
- `v_blockers` — для каждого узла список блокирующих dot-ID с путём от корня.
- `v_overdue` — узлы с `due_date < today(projects.timezone)` и эффективным статусом вне (`done`,`cancelled`),
  с числом дней просрочки.
- `v_attempt_hours` — длительность попыток (сумма `ended_at - started_at`), база для `actual_hours`.

### 5.3. Инварианты

1. `path` уникален в проекте; `seq` сиблингов монотонен и не переиспользуется (в т.ч. после удаления).
   Уникальность адреса держится и структурой — `UNIQUE(project_id, COALESCE(parent_id, 0), seq)`:
   испорченная строка `path` не сможет дать двум узлам один адрес.
2. `depth = parent.depth + 1`; предел — 16 уровней (`node::MAX_DEPTH`), глубже узел не создаётся и
   поддерево не переносится (`DEPTH_LIMIT`). Предел на длину пути отдельно не задаётся: при 16
   сегментах dot-ID укладывается в 255 байт с запасом, поэтому правило одно, а не два.
3. Узел в `done` не может быть захвачен; для повторного захвата нужен переход в `in_progress` (reopen).
4. `blocks`-ребро не может соединять предка и потомка (тупик по построению) — отказ `ANCESTOR_BLOCK_EDGE`.
5. Ребро `blocks` не создаёт цикл в графе `blocks` — отказ `CYCLE_DETECTED` до записи. Проверка
   транзитивная (поиск пути от цели к источнику), поэтому ловится и пара «A ждёт B, B ждёт A», и
   цепочка любой длины; в сообщении печатается весь круг, чтобы было видно, что размыкать.
   Проверка выполняется внутри `BEGIN IMMEDIATE`: две одновременные встречные заявки не могут
   обе пройти на графе, прочитанном до чужой записи.
6. Одна незакрытая попытка на узел (гарантируется частичным уникальным индексом + транзакцией).
7. Резерв живёт не дольше своей попытки: `released_at` либо при release, либо при истечении аренды.
8. Каждая мутация пишет строку в `activity_log` в той же транзакции.
9. `description_locked = 1` запрещает менять `description` (остальные поля — свободно), отказ `DESCRIPTION_LOCKED`.
10. Удалять (`node_delete`) можно только узел, ещё не начатый работой (`idea`, `backlog`, `todo`);
    дальше — есть история, её не затираем.

## 6. Dot-notation ID

### 6.1. Формат

```
TASQ-12          эпик №12 в проекте TASQ
TASQ-12.3        третья задача эпика 12
TASQ-12.3.1      первая подзадача задачи TASQ-12.3
TASQ-12.3.1.4    её четвёртая микро-задача
```

- `TASQ` — `projects.key`, `[A-Z0-9]{2,8}`, неизменяем (`key` менять запрещено: сменится адрес всего дерева).
- `12` — `seq` среди корневых узлов проекта; `3`, `1`, `4` — `seq` среди детей своего родителя.
- В БД хранится `path` без ключа (`12.3.1`), отображение собирается из ключа проекта.
- Внутренний `nodes.id` остаётся стабильным и используется в связях; dot-ID — представление.

### 6.2. Разрешение ссылки

Любой инструмент и команда CLI принимают любую из форм:

| Форма | Пример | Примечание |
| --- | --- | --- |
| dot-ID | `TASQ-12.3.1` | основной человекочитаемый |
| голый путь | `12.3.1` | в контексте проекта по умолчанию |
| внутренний id | `#4711` | для машинных ссылок и логов |

Ссылка с ключом **самодостаточна**: `DEV-1.1` ведёт в проект DEV из любой сессии — «работаю в ALPHA»
этому не мешает. Это то, что делает обзор по всем проектам рабочим: адреса, которые он выдаёт,
открываются без предварительного выбора проекта, а `set DEV-1.1`, `claim DEV-1.1` и `rm DEV-1.1`
правят именно узел DEV. Селектор `--project` при этом по-прежнему главный: ссылка без ключа живёт
в проекте сессии.

Неизвестный ключ в ссылке — `PROJECT_NOT_FOUND` с перечнем доступных (`NOPE-1` — это опечатка в
проекте, а не битый адрес). Ссылка без ключа в базе с несколькими проектами при отсутствии селектора
даёт `PROJECT_REQUIRED`: угадывать проект не из чего.

`#id` ключа не несёт: внутренний id однозначен только внутри проекта, поэтому такой ссылке селектор
или единственный проект в базе нужен по-прежнему.

### 6.3. Перемещение и перенос

- `node_move` меняет родителя, сохраняя `seq`: dot-ID узла и всех его потомков меняется — это
  документированное следствие, а не сбой. В `activity_log` пишется пара `old_path → new_path` для узла
  и для каждого потомка (одна запись на узел, `action='move'`).
- `--renumber` (необязательный) перенумеровывает сиблингов нового родителя по `sort_order`;
  без флага структура сохраняется.
- Старые dot-ID не резервируются: при создании `seq` никогда не выдаётся заново, но перенесённый узел
  может занять освободившийся путь (он приходит со своим `seq`), поэтому прежний адрес не гарантирует
  прежний узел. Для стабильных внешних ссылок — `#id` или `metadata.external_ref`.

**`mv --to-project KEY` — перенос поддерева в другой проект** (`node_move` с `to_project`). Адрес
перестраивается: `RRL-3.1` становится `<новый ключ>-<seq>`, номер сохраняется, если свободен в
целевом проекте, иначе берётся следующий свободный. `--parent` ищется в ЦЕЛЕВОМ проекте.

Что следует за узлом и что нет (решение владельца):

- история статусов, комментарии, аренды, резервы и рёбра переезжают сами: они ссылаются на
  `node_id`, а не на проект;
- **ссылки в текстах переписываются** во всех НЕархивных записях базы — заголовок, описание,
  `source_ref`, комментарии к таким узлам и заметки. Поиск идёт по токену, а не по подстроке:
  `RRL-3.1` не совпадает внутри `XRRL-3.1` и не задевает `RRL-3.10`, потомок `RRL-3.1.5`
  переписывается в `<новый>-5`, а точка в конце фразы остаётся знаком препинания;
- архив хранит прежний адрес намеренно: это застывшая история, и правка текста в нём задним
  числом переписала бы прошлое;
- в перенесённый узел добавляется комментарий от `tasq` с прежним и новым адресом — иначе запись
  с другим адресом в старом проекте выглядит ошибкой, а не историей;
- `updated_at` узлов, которых коснулась замена ссылок, не поднимается: дата последней работы над
  задачей — не дата переезда соседа, и она участвует в отчёте о застое.

**Вид узла подчиняется глубине — при любом переносе.** `epic` — это глубина 1, `task` — 2,
`subtask` — 3, глубже — `micro`, и после переноса вид пересчитывается по новой глубине, а не
остаётся тем, каким был при создании. Отсюда два следствия, оба нужны в работе:

- задачу можно поставить в корень проекта (своего или другого), и она станет эпиком; эпик можно
  опустить под задачу, и он станет подзадачей — и его дети сдвинутся вместе с ним;
- статус приводится к набору нового вида: эпику запрещён `todo`, остальным — `planned`, поэтому
  `todo` становится `planned` и наоборот. Обе пары нетерминальны: ни burn-up, ни отчёты о
  закрытии от такой замены не меняются, поэтому в историю статусов запись не идёт (она отвечает
  «когда узел стал `done`»), а в журнал — идёт.

Ответ команды — отчёт: прежний и новый адрес, сколько узлов переехало, сколько сменило вид или
статус (`rekinded`) и сколько записей получило новый адрес (`nodes`, `comments`, `notes`).

## 7. Статусы и переходы

Воронка одна для всех уровней: **idea → backlog → todo (epic: planned) → in_progress → review →
done**. Первые два состояния нужны, чтобы отделить «мысль» от «очереди»: в `idea` узел лежит, пока
формулировка не созрела, в `backlog` — принят в работу, но ещё не готов к захвату.

Из воронки есть два выхода: `done` — работа закрыта; `cancelled` — работу отменили, не начиная.

**`incomplete` — не статус, а состояние.** Хранится только решение человека: узел закрыт —
`done`. «Недоведённость» вычисляется из состояния детей: если под закрытым узлом остались
незакрытые потомки, читатель видит `incomplete`. Схема об этом не знает ничего, и записать
`incomplete` нельзя — его нет ни в справочнике статусов, ни в матрице переходов.

Почему вычисляется, а не хранится: хранимый вывод расходится с фактом. Закрыли эпик с четырьмя
открытыми задачами — записали бы `incomplete`; потом одну из них открыли заново, и запись
пришлось бы править руками. Доделали последнюю — и `incomplete` висел бы вечно. Вычисление даёт
ровно обратное: состояние меняется само.

Отсюда поведение:

- закрыт он, но не выполнен: из «что делать дальше» и из burn-up он уходит, в прогресс не идёт —
  эпик с открытыми задачами не показывает 100 %;
- из списков и с доски он **не** уходит: это единственный сигнал «здесь остались хвосты».
  `ls -n` (скрыть выполненное) прячет только `done`, а `incomplete` остаётся виден;
- `ls -a` показывает архив;
- вернуть его в работу — обычное реоткрытие: `set --status todo|planned`, и это не требует
  `--force`, потому что человек не отменяет чужое решение, а продолжает своё;
- закрытие с хвостами `--force` не требует: это не ложь, а факт, и он показан честно.
  `--force` остался только для предусловий (`UNMET_PREREQUISITES`).

`planned → done` разрешён: эпик, который так и не взяли в работу, всё равно закрывают — задачи
под ним могли сделать другие ветви, или работа оказалась не нужна.

Наборы по `kind` (совместимы с saga; при импорте `completed → done`, `cancelled → cancelled`,
`on_hold → planned`; `idea`/`backlog` — собственные состояния tasq и переносятся как есть):

| kind | допустимые хранимые статусы |
| --- | --- |
| epic | idea, backlog, planned, in_progress, blocked, review, done, cancelled, deferred, incomplete |
| task | idea, backlog, todo, in_progress, blocked, review, done, cancelled, deferred, incomplete |
| subtask, micro | idea, backlog, todo, in_progress, blocked, review, done, cancelled, deferred, incomplete |

Разделение `planned`/`todo` — по уровню: планируют эпик, задачи получают `todo`. `idea` и `backlog`
допустимы любому виду. Отказ `INVALID_TRANSITION` перечисляет допустимые статусы для текущего
состояния и вида узла (список строится из матрицы, а не пишется текстом): для человека это
экономит шаг, для MCP-агента — единственный способ узнать выход, потому что README он не читает.

Матрица переходов (`→` разрешено; всё прочее — `INVALID_TRANSITION`, без «мягкой» подмены):

| из | в |
| --- | --- |
| (создание) | idea (все виды) |
| idea | backlog, planned (epic), todo (остальные), in_progress, blocked, deferred, cancelled |
| backlog | idea (сырая мысль), planned (epic), todo (остальные), in_progress, blocked, deferred, cancelled |
| planned | idea, backlog, in_progress, blocked, deferred, done, cancelled |
| todo | idea, backlog, in_progress, blocked, deferred, cancelled |
| in_progress | backlog, todo (снять захват), blocked, review, deferred, done, cancelled |
| review | backlog, in_progress, blocked, done, cancelled |
| blocked | backlog, todo, in_progress (если блокеры сняты), cancelled |
| deferred | idea, backlog, planned (epic), todo, cancelled |
| done | todo, in_progress (reopen) — требует `force` |
| cancelled | todo (restore) — требует `force` |

`incomplete` в матрице нет: он не хранится и не является целью перехода (см. §7).

Понижение в идею — обычный переход, а не ошибка: `todo → idea` возвращает сырую мысль в начало
воронки, и это единственный способ держать «недодуманное» вне очереди.

Флаги, меняющие строгость (а не подменяющие статус), соответствуют saga:

- `force` — записать именно `done` при незакрытых детях (`CHILDREN_UNFINISHED`) или начать работу
  при невыполненных предусловиях (`UNMET_PREREQUISITES`); факт `force` пишется в лог. Без `force`
  закрытие узла с открытыми детьми даёт `incomplete`, а не отказ и не ложное `done`.
- `description_locked` — защита постановки от перезаписи (`task_lock_description` в saga).

Прочее:

- `in_progress` **не выставляется руками** при живой аренде: захват и есть переход. Матрица выше задаёт,
  какие переходы вообще допустимы, а право выполнить `todo → in_progress` даёт только живая аренда —
  `node_update` без валидного `token` отвергается `LEASE_REQUIRED` (в CLI — `tasq claim`, он и делает
  переход). Это единственное место, где матрица и права расходятся, и расхождение намеренное.
- **Обход дерева ограничен и по шагам, и по объёму.** Шаг ограничен пределом глубины (16) — это
  защита от цикла и от испорченного `parent_id`. Объём ограничен отдельно: обход порождает по
  строке на пару «предок — потомок», и на широком дереве пар в разы больше, чем узлов (замер:
  87 381 узел → 757 305 пар; граница — `узлы × (предел + 1)`). Перед тяжёлым чтением (`ls`,
  `board`, `burnup`) эта граница сравнивается с бюджетом `TASQ_SCAN_BUDGET` (по умолчанию
  20 млн пар), и при превышении чтение отказывает `TRAVERSAL_BUDGET` с числами: узлы, пары,
  предел. Молчаливое заполнение временного файла — не отказ, а авария: так уже уходило 194 ГБ за
  один зависший прогон. Направление обхода объём не меняет — обходы вверх и вниз перечисляют одно
  и то же множество пар (измерено), поэтому защита сторожит объём, а не переписывает запрос.
- Пересчёт прогресса вверх по дереву — вычисляемый (`v_node_progress`), кэша нет: дёшево на SQLite,
  зато нет рассинхрона.
- `archived_at` (архив эпика/ветви) скрывает поддерево из выдач, дашборда, поиска и рекомендаций,
  пока не передан `include_archived`. Ничего не удаляется.
- Массовый архив — по статусу, а не перебором ссылок: `tasq archive --status done` берёт САМЫЕ
  ВЕРХНИЕ узлы множества (у кого родитель тоже в нём — пропускается, его накрывает поддерево
  родителя), поэтому повторный запуск ничего не находит и не упирается в «уже в архиве».
  `--project all` проходит по всей базе, `--dry-run` показывает, что уйдёт, и ничего не пишет;
  откат — теми же аргументами с `--undo`. Поддерево уходит целиком, включая детей в других
  статусах: это то же поведение, что у архива одной ссылки.

## 8. Аренды с истечением

### 8.1. Параметры

| параметр | значение по умолчанию | границы |
| --- | --- | --- |
| `ttl_seconds` | 900 (15 мин) | 60 … 28 800 (8 ч) |
| продление | до `now + ttl`, сколько угодно раз, пока попытка жива | — |
| `max_attempts_per_node` | без ограничения (счётчик в `attempts.attempt_no`) | — |
| захват `done`/`cancelled` | запрещён | — |
| чекпойнт | ≤ 8 КиБ текста | — |

### 8.2. Захват (`tasq_claim`)

Одна транзакция `BEGIN IMMEDIATE`, порядок проверок фиксирован (первый отказ выигрывает, чтобы
сообщения были предсказуемы):

1. узел существует и виден (не `deleted_at`) → иначе `NODE_NOT_FOUND`;
2. узел не в `done`/`cancelled` → `NODE_CLOSED`;
3. нет незакрытой попытки → иначе `ACTIVE_ATTEMPT_EXISTS` (в сообщении: `holder`, `expires_at`,
   `attempt_no`, `renew_hint`);
4. нет невыполненных блокеров → `UNMET_PREREQUISITES` со списком dot-ID, если не `force`;
5. нет конфликта резервов, запрошенных вместе с захватом → `RESOURCE_RESERVATION_CONFLICT`
   (в сообщении: держатель, его аренда, `expires_at`);
6. создаётся `attempts` (`token` = 32 случайных байта base64url, `attempt_no = max+1`), узел
   переводится в `in_progress` (`action='claim'`), резервы записываются, в лог — одна строка на всё.

Ответ содержит `attempt_id`, `token`, `expires_at`, `attempt_no`, `dot_id`, `path`, чекпойнт
предыдущей попытки (если был), унаследованные резервы предыдущей попытки **нет** — они освобождаются
вместе с арендой; новый исполнитель заявляет свои.

### 8.3. Продление и завершение

- `tasq_renew(token, ttl)` — `expires_at = now + ttl`, `renewed_at = now`; на истёкшей аренде —
  `LEASE_EXPIRED` (и `expired_attempt_id` в теле ошибки, чтобы клиент не искал его сам).
- `tasq_release(token, outcome, checkpoint?)` — закрывает попытку (`released` / `failed` / `completed`),
  освобождает резервы, пишет чекпойнт; `outcome=completed` переводит узел в `done` (с проверкой детей),
  `failed` — в `todo` с причиной, `released` — в `todo`.
- **Истечение освобождает узел само.** Аренда живёт до `expires_at`; после этого срока узел
  считается свободным без участия человека и без фонового демона. Механизм из двух частей, и они
  решают разные задачи:
    1. **Чтение** (`v_node_effective`) считает истёкшую аренду недействительной: `lease_holder` и
       `lease_expires_at` заполняются только для живой попытки, а узел в `in_progress` без живой
       аренды показывается в очереди — `todo` для задачи, `planned` для эпика (у эпика `todo` не
       бывает). Поэтому `next` снова предлагает узел, а `ls` не показывает мёртвого держателя.
    2. **Хранилище** чистит `lease::release_expired`, которая вызывается на путях записи (первым
       делом в `claim`, исключая целевой узел) и из явного `doctor --gc`: закрывает попытку с
       `end_reason='expired'`, снимает резервы, возвращает узел в очередь. Без этого `time` считал
       бы часы по незакрытой аренде, а `uq_attempts_live` держал бы строку, которую никто не
       обслуживает.

  `doctor` считает живыми только действующие аренды (`expires_at > now`), а `doctor --gc` без
  селектора проекта убирает все проекты: проверка идёт по всей базе, и требовать выбор одного
  проекта ради уборки значило бы оставить мусор в остальных.

  Разделение важнее, чем кажется: чтение не должно писать (иначе каждая `ls` была бы транзакцией),
  а запись не должна зависеть от того, читали ли базу после истечения аренды. Раньше работала
  только уборка по команде, и узел с истёкшей арендой исчезал из `next` навсегда — на живой базе
  это выглядело как 22 «занятых» узла при одной реально живой аренде.

### 8.4. Перехват и разделение

- Просроченная аренда **не требует флага**: захват проходит сразу, прежняя попытка закрывается с
  `end_reason='superseded'`, её резервы освобождаются, чекпойнт наследуется новой попыткой.
- `--steal` нужен для другого случая: аренда ещё жива, но держатель явно завис (процесс есть, работы нет).
  Флаг разрешает вытеснить живую аренду; это осознанное действие, в лог пишется `claim` от нового
  исполнителя и `superseded` у прежнего. Профиль `agent` этого флага не получает — только `full`.
- `tasq holders` и `tasq_dashboard` показывают «живые аренды и сколько осталось», чтобы агент видел
  чужие попытки до того, как упрётся в отказ.

## 9. Резервирование ресурсов

### 9.1. Виды

| kind | значение | семантика пересечения |
| --- | --- | --- |
| `path` | `src/api/`, `src/api/handlers.rs` | префикс по границе сегмента: `src/api` задевает `src/api/x.rs`, но не `src/apix/` |
| `glob` | `src/**/*.rs`, `migrations/*.sql` | пересечение по «пересечению языков»: консервативно — если шаблоны пересекаются по общему префиксу и суффиксу; непересечение доказывается только для литеральных шаблонов без `**` |
| `logical` | `port:8080`, `deploy:staging`, `migration:2026-11` | точное равенство строки |

### 9.2. Правила

- Путь приводится к виду от корня проекта: убираются `.`, разрешается `..` без выхода за корень,
  символические ссылки **не** разыменовываются (иначе конфликт зависит от машины). Отказ при выходе
  за корень — `PATH_OUTSIDE_PROJECT`.
- Режимы: `exclusive` (по умолчанию) конфликтует с любым пересечением; `shared` конфликтует только с
  `exclusive`.
- Заявка атомарна с захватом: либо аренда + все резервы, либо ничего (`RESOURCE_RESERVATION_CONFLICT`
  называет виновника).
- `tasq_reserve(token, add, remove)` добавляет/снимает резервы живой попытки; снятие чужих — отказ
  `NOT_YOUR_ATTEMPT`.
- Освобождение: при `release`, при истечении аренды (`gc`), при удалении узла.
- Пределы: не более 64 резервов на попытку, длина значения ≤ 512 байт; больше — `TOO_MANY_RESERVATIONS`.
- `tasq_holdings` отдаёт единый реестр: живые попытки, их резервы, остаток TTL; фильтры по проекту,
  виду ресурса и значению (в том числе «кто держит `src/api/`»).

## 10. Зависимости сквозь границы вложенности

- Рёбра хранятся между **узлами любого уровня**, поэтому «подзадача в Backend зависит от подзадачи в
  Database» выражается напрямую (в saga зависимость возможна только внутри одного уровня).
- Виды: `blocks` (жёсткая последовательность), `related` (связь), `duplicates`, `discovered_from`,
  `split_from`. Блокирующим считается только `blocks` и его транзитивное замыкание.
- **Направление ребра: `from -> to` читается «from ждёт to»** — то есть `to` блокирует `from`.
  Так ребро читают `v_node_effective`, `node.rs` и `lease.rs`, и так же его формулирует CLI:
  `tasq dep add A B` = «A ждёт B».
- Валидация при создании ребра:
  1. оба узла существуют в одном проекте (`CROSS_PROJECT_EDGE` иначе);
  2. `from ≠ to` (`SELF_EDGE`);
  3. для `blocks`: не предок/потомок (`ANCESTOR_BLOCK_EDGE`);
  4. для `blocks`: цикл в графе `blocks` (`CYCLE_DETECTED`, в теле — найденный путь);
  5. дубликат `(from,to,kind)` — идемпотентно (без ошибки, без второй строки).
- Снятие: `tasq_unlink(from,to,kind)`; снятие блокера пересчитывает эффективный статус автоматически
  (он вычисляемый), но пишет `activity_log` `unblock`.
- `tasq_graph(dot_id, depth)` отдаёт окрестность: что блокирует узел (с путями), что блокирует он,
  транзитивные блокеры, критический путь по количеству зависимых. Формат — JSON и Mermaid.
- «Автоблок»: если все `blocks`-предусловия выполнены, узел в `blocked` возвращается в `todo`
  (при следующем чтении эффективного статуса; хранимый `blocked` снимается `gc` при изменении условий
  или явным `node_update`).

## 11. Дедлайны и просрочка

- `due_date` — дата (`YYYY-MM-DD`, часовой пояс проекта) у любого узла; у эпика — как правило,
  агрегированный дедлайн, у задачи — рабочий.
- Просрочка: `due_date < today` и эффективный статус вне (`done`, `cancelled`); `days_overdue` считается
  календарными днями, `blocked` **не** освобождает от просрочки.
- `tasq_dashboard` и `tasq_next` показывают просроченные первыми (`overdue` блок), затем ближайшие сроки
  (≤ 3 дня — `soon`).
- «Сгорание дедлайна»: для узла считается `elapsed_ratio = (today - created_at) / (due_date - created_at)`
  и `done_ratio = прогресс`; отставание = `elapsed_ratio - done_ratio`, выводится как `at_risk`
  при `> 0.2`. Это сигнал, а не отдельный статус.
- Смена дедлайна логируется (`field='due_date'`), чтобы burn-up видел сдвиг цели.
- Выходные и праздники не учитываются (v1); при необходимости — `projects.metadata.workdays`.

## 12. Burn-up

Три метрики: `count` (штуки), `points` (`weight`), `hours` (`estimated_hours`).

Источник — `activity_log` плюс `burn_snapshots`:

- `scope_total(t)` — сумма веса узлов проекта, созданных ≤ t, минус `cancelled`/удалённые до t
  (плюс `scope_added` — сколько добавили за период: «расползание объёма»);
- `scope_done(t)` — вес узлов, достигших `done` ≤ t (по первой записи `action='transition'` в `done`);
- снимок за сутки материализуется `tasq burnup --snapshot` или попутно командой, читающей график;
  отсутствующие дни восстанавливаются из лога (лог — первоисточник, снимок — кэш).
- Сглаживание — скользящее среднее по 7 дням (флаг `--window`).
- Прогноз: линейная экстраполяция по последним `window` дням; выдаётся как «ожидаемое завершение» с
  явной пометкой `forecast`, а не как обещание.

Выдача: `tasq_burnup(metric, since, until, bucket=day|week, format=table|json|ascii)`; CLI `tasq burnup`
рисует ASCII-график (две линии: объём и готово, плюс линия цели, если задан `due_date` эпика).
Данные всегда отдаются и в JSON, чтобы график мог рисовать кто угодно.

## 13. MCP-поверхность

### 13.1. Транспорт

- Основной — stdio: одна JSON-строка на сообщение, логи только в stderr, уведомления без `id` не
  получают ответа.
- Методы: `initialize`, `ping`, `tools/list`, `tools/call`; неизвестный метод → `-32601`.
- `tools/call` возвращает `{"content":[{"type":"text","text":...}],"isError":bool}`; ошибки
  предметного уровня — текстом в `content` с кодом из §19, не в `error`.
- Инструменты, меняющие состояние, помечены `readOnlyHint: false`; удаляющие — `destructiveHint: true`;
  все — `openWorldHint: false` (сеть не используется).
- `--profile` сужает каталог: `full` (по умолчанию), `agent`, `read-only`, `migration`. Профиль
  влияет только на то, что видно в `tools/list`; вызов скрытого инструмента — `TOOL_NOT_IN_PROFILE`.

### 13.2. Инструменты и бюджет токенов

Бюджет считается по `tools/list` (компактный JSON) в токенах `o200k_base`:

| профиль | инструментов | бюджет (цель) | факт (`tasq tokens`) |
| --- | --- | --- | --- |
| `full` | 47 | ≤ 6 000 | 5 785 |
| `agent` | 30 | ≤ 4 000 | 3 884 |
| `read-only` | 17 | ≤ 2 000 | 1 926 |
| `migration` | 6 | ≤ 800 | 603 |

Факт — вывод `tasq tokens`, и он же сверяется с этой таблицей: `tasq docs --check` падает, если
число инструментов или стоимость профиля разошлись с бинарём.

**Поверхность описана один раз.** Схема аргументов каждого инструмента, его описание и аннотации
живут в `src/surface.rs`; из этого объявления строятся `tools/list`, `docs/mcp-tools.md` и цифры
строки `full` выше. Команды CLI остаются в clap — он собирает разбор аргументов на этапе
компиляции, — но между двумя описаниями стоит сторож (`src/surface_checks.rs`): каждая команда CLI
обязана быть названа операцией либо попасть в `CLI_ONLY` с причиной, а каждый её флаг — объявлен
аргументом либо в `cli_only` операции, тоже с причиной. Раньше копий было три, и они разошлись:
`tags` и `metadata` появились в CLI на сутки раньше, чем в MCP.

Снимок `tests/fixtures/mcp-schema.json` держит провод неподвижным: изменение схемы видно в диффе
и требует обновления файла, а не выясняется у клиента. Бюджет — гейт: `make tokens` и шаг
«Tool schema budget» в CI падают при перерасходе, поэтому «плюс один инструмент» не проедет
молча. Самые дорогие инструменты — `node_create` (195), `tasq_claim` (184), `node_update` (180):
у них больше всего параметров, и это цена за то, что агент делает всё одним вызовом.

Счёт 46, а не 42 из первичного плана: в M6 добавился `tasq_risk` («узлы, отстающие от своего срока»),
и он вошёл в каталог отдельным инструментом, а не параметром `tasq_dashboard` — у него своя выдача
и свой порог; позже добавились `project_select` и `project_delete` — работа с проектами (см. §15.1), — а
`tasq_history` закрыл вопрос «когда и почему узел стал `done`» (см. §5.1.1).

Числа — целевые: первый реальный замер (`tasq tokens`) фиксирует факт, дальше бюджет только ужесточается
осознанным решением, а не «подгонкой» гейта. Состав профиля `migration` — ровно шесть инструментов:
`tasq_init`, `project_list`, `project_create`, `node_get`, `tasq_import`, `tasq_export`.

Правила, из которых складывается бюджет (нарушение — падение гейта, а не «предупреждение»):

- описание инструмента ≤ 200 символов, описание параметра ≤ 80;
- параметров ≤ 8 на инструмент; перечисления вместо свободного текста;
- никаких парных инструментов на уровень (`task_*` / `subtask_*` в saga) — один `node_*` с `kind`;
- «тонкие» ответы по умолчанию: списки отдают dot-ID, заголовок, статус, приоритет, срок; полный текст
  — по `detail: "full"` или отдельным `node_get`;
- `tasq tokens` печатает общий размер и разбивку по инструментам (по убыванию), `--max N` — код выхода 1
  при превышении; тот же расчёт в CI (`make tokens`, шаг «Tool schema budget»).

| группа | инструменты | профили |
| --- | --- | --- |
| инициализация и проекты (6) | `tasq_init`, `project_create`, `project_list`, `project_update`, `project_select`, `project_delete` | full, agent (без `tasq_init` и `project_delete`) / (init — migration) |
| узлы (9) | `node_create`, `node_get`, `node_update`, `node_list`, `node_move`, `node_reorder`, `node_archive`, `node_delete`, `node_restore` | full, agent (без move/delete/restore) |
| дерево и рекомендации (2) | `tasq_context`, `tasq_next` | full, agent, read-only |
| аренды (3) | `tasq_claim`, `tasq_renew`, `tasq_release` | full, agent |
| ресурсы (2) | `tasq_reserve`, `tasq_holdings` | full, agent (`holdings` — read-only) |
| зависимости (3) | `tasq_link`, `tasq_unlink`, `tasq_graph` | full, agent (`graph` — read-only) |
| комментарии (2) | `comment_add`, `comment_list` (+ удаление через `comment_delete`) | full, agent |
| комментарии (удаление) (1) | `comment_delete` (op: delete/restore) | full |
| заметки (4) | `note_save`, `note_list`, `note_search`, `note_delete` | full, agent (без delete) |
| шаблоны (4) | `template_save` (upsert), `template_list`, `template_apply`, `template_delete` | full |
| активность (3) | `activity_log`, `session_diff`, `node_batch_update` | full, agent (`batch` — full) |
| поиск (1) | `tasq_search` | full, agent, read-only |
| обзор и графики (2) | `tasq_dashboard`, `tasq_burnup` | full, agent |
| архив/перенос данных (2) | `tasq_export`, `tasq_import` | full (import — migration) |

Точные имена и схемы фиксируются в `docs/tools.md` при кодировании и сверяются гейтом `tasq tokens`;
расширять набор сверх этих 42 инструментов можно только с пересмотром бюджета.

**Скоупинг по ветке** (в saga — `branch="current"`): `node_create`, `node_list`, `epic`-фильтры и
`tasq_dashboard` принимают `branch`; значение `current` читается один раз через
`git rev-parse --abbrev-ref HEAD` в каталоге проекта. Если git недоступен или каталог не репозиторий —
отказ `BRANCH_UNAVAILABLE`, а не подстановка пустой строки.

Соответствие «инструмент saga → инструмент tasq» — таблица в `docs/saga-parity.md` (41 инструмент).

### 13.3. HTTP-транспорт (опция, только по явному флагу)

```
tasq serve --http 127.0.0.1:8787 [--profile agent|full|read-only|migration] [--project KEY]
```

Предназначен для клиентов, которые не умеют stdio. По умолчанию **не поднимается**: без флага
`tasq serve` отказывает и называет флаг. Профиль по умолчанию — `agent`: наружу отдаётся то, что
нужно для работы, а не весь каталог.

Ограничения заданы кодом, а не настройкой:

- **только loopback.** `0.0.0.0:8787` — отказ с причиной: это открыло бы порт в сеть.
- **TLS здесь не терминируется.** `https://…` — отказ с причиной: поднимать кустарный TLS хуже,
  чем не поднимать; если он нужен, его ставит прокси перед tasq.
- **`Host` и `Origin` проверяются на loopback.** Браузер на чужой странице умеет отправлять
  запросы на `127.0.0.1` (DNS rebinding), и без этой проверки локальный трекер стал бы доступен
  любому открытому сайту. `Origin: null` (песочница) не подтверждает ничего и тоже отвергается.
- **POST и только в корень.** `GET` — 405, тело больше 1 МиБ — 413, без `Content-Length` — 411
  (chunked не поддерживается, и притворяться, что поддерживается, нельзя).
- **без аутентификации** — как и stdio: доступ ограничен тем, что порт не выходит за loopback.
- **соединения обрабатываются по одному.** Обработчик не `Sync` (внутри соединение SQLite), и
  параллельная обработка означала бы мьютекс вокруг всей работы — ту же очередь, но с потоками.
  Замер очереди (TASQDEV-20.8): сто вызовов через один сервер — 51–66 мс, отказов нет, поэтому
  очередь здесь не узкое место.

Адрес печатается в stderr (`tasq mcp: listening on http://127.0.0.1:PORT`) — при `:0` это
единственный способ узнать порт. Уведомление без `id` получает `204 No Content`: HTTP-клиент ждёт
ответ, и молчание выглядело бы зависанием.

## 14. `tasq connect` — автоматическая прописка конфига

`tasq connect` находит клиентов, показывает найденные и вписывает сервер `tasq` в их конфиг:

| клиент | файл | формат / ключ |
| --- | --- | --- |
| `claude-code` | проект `.mcp.json`, пользовательский `~/.claude.json` | JSON, `mcpServers` |
| `claude-desktop` | `~/.config/Claude/claude_desktop_config.json` | JSON, `mcpServers` |
| `opencode` | `~/.config/opencode/opencode.json` | JSON, `mcp` |
| `zed` | `~/.config/zed/settings.json` | JSON, `agent_servers` (в старых версиях `context_servers` — ключ определяется по файлу) |
| `cursor` | `~/.cursor/mcp.json`, проект `.cursor/mcp.json` | JSON, `mcpServers` |
| `vscode` | `~/.config/Code/User/mcp.json`, проект `.vscode/mcp.json` | JSON, `servers` |
| `gemini` | `~/.gemini/settings.json` | JSON, `mcpServers` |
| `codex` | `~/.codex/config.toml` | TOML, `mcp_servers` |
| `goose` | `~/.config/goose/config.yaml` | YAML, `extensions` |
| `crush` | `~/.config/crush/crush.json` | JSON, `mcp` |
| `windsurf` | `~/.codeium/windsurf/mcp_config.json` | JSON, `mcpServers` |

Правила (жёсткие, иначе «прописка» ломает чужие конфиги):

1. `--dry-run` печатает итоговый фрагмент и путь, ничего не записывая.
2. Запись атомарна: временный файл + `rename`; перед записью — копия `<файл>.bak-<YYYYmmdd-HHMMSS>`.
3. Существующие ключи и порядок сохраняются; правка только ключа сервера `tasq` (или добавление).
4. Файл, который не парсится, **не трогается**: печатается готовый фрагмент и код выхода 3.
5. Повторный запуск идемпотентен; `--remove` убирает только запись `tasq`.
6. Транспорт: по умолчанию stdio — в конфиг пишется абсолютный путь к текущему бинарю и `args=["mcp"]`,
   чтобы запись не зависела от `PATH` клиента. `--url URL` пишет HTTP-адрес и принимает **только петлю**
   (`127.0.0.1`, `localhost`, `[::1]`): наружу MCP-сервер молча не отдаётся.
7. `--scope project|user`; по умолчанию — значение из таблицы клиента (`claude-code` и `vscode` — проект,
   остальные — пользовательский файл). Охват, которого у клиента нет, — ошибка, а не молчаливая подмена.
8. Без идентификатора клиента команда только печатает таблицу «клиент → путь → есть файл → прописан»
   и ничего не создаёт: иначе один запуск насоздавал бы конфиги всем одиннадцати.
9. `--check` перечитывает конфиг и возвращает `0`, если `tasq` прописан, и `1`, если нет; `--remove`
   убирает запись, а опустевший контейнер (например `mcpServers: {}`) удаляет целиком.
10. Формат сверен с фактическими файлами на машине, а не только с документацией клиентов. Два
    подводных камня, найденных при этом:
    - `~/.config/zed/settings.json` штатно содержит `//`-комментарии, поэтому strict-JSON его не
      разбирает. Такой файл не переписывается никогда: печатается YAML/JSON-фрагмент и код 3.
    - ключ Zed менялся между версиями: `context_servers` → `agent_servers`. Ключ выбирается по тому,
      что уже есть в файле, иначе запись уедет туда, куда редактор не смотрит.
11. YAML (`goose`) не правится автоматически: комментарии, якоря и порядок ключей слишком легко
    потерять. Печатается готовый фрагмент, код 3. TOML (`codex`) правится через `toml_edit` без
    переформатирования, с неявной таблицей `mcp_servers`, чтобы в файле не появлялось пустого заголовка.
12. Идентификатор инструментов различается по клиентам и это учтено: `claude-code`/`cursor`/`gemini`/
    `windsurf` — `command`+`args`; `opencode`/`crush` — `type: local` и одна команда со всеми
    аргументами; `zed` — вложенный `command: {path, args, env}`.

## 15. CLI

```
tasq init [--key TASQ] [--name "..."] [--here]     инициализация .tasq/tasq.db
tasq add <parent|проект> "заголовок" [--kind] …    создать узел
tasq show <ref> [--full] [--tree]                  карточка узла и поддерево
tasq ls [-n|--no-done] [--except-project KEY]…       список; без селектора — все проекты
tasq board [--all]                                  доска по статусам; --all — секция на проект
tasq claim <ref> [--holder] [--ttl] [--steal] [--checkpoint]  захват; печатает token
tasq renew <token> [--ttl]                         продление
tasq release <token> [--outcome released|failed|completed] [--checkpoint]
tasq holders                                       живые аренды и остаток TTL
tasq next [--explain]                              рекомендация + причина
tasq ctx <ref>                                     цепочка контекста от корня до узла
tasq dep add|rm|ls|graph <ref> …                   зависимости и их граф
tasq find <запрос>                                 поиск (FTS5); без --project — по всей базе
tasq time [--parent <ref>]                         часы: оценка против факта из попыток
tasq bulk set --status <s> <refs…>                 пакетная правка с отчётом по узлу
tasq burnup [--metric count|points|hours] [--since] [--until] [--ascii] [--snapshot]
tasq risk [--threshold 0.2]                         узлы, отстающие от своего срока
tasq note save|ls|search|rm                        заметки 8 типов
tasq comment add|ls|rm|restore <ref>               нить комментариев с мягким удалением
tasq template save|ls|show|apply|rm                шаблоны с {переменными}
tasq export [--out FILE] [--include-deleted]           экспорт с контрольной суммой
tasq import FILE [--mode merge|replace] [--from-saga] [--dry-run]
tasq create project KEY [--name "..."]             создать проект в текущей базе
tasq list projects                                 проекты базы с числом живых узлов
tasq update project [KEY] [--name] [--description] [--status] [--new-key]
                                                   переименовать проект, сменить статус или ключ
tasq delete project [KEY] [--force]                удалить проект со всем содержимым
tasq tokens [--max N] [--profile]                  бюджет схем MCP
tasq docs [--out DIR] [--check]                    справочник из бинаря; --check ловит расхождение
tasq connect [клиент] [--scope user|project] [--url URL] [--dry-run] [--remove] [--check]
tasq completions bash|zsh|fish [--out PATH]        скрипт автодополнения (алиас: completion)
tasq mcp [--profile]                               MCP-сервер stdio
tasq migrate [--json]                           обновить схему базы до поддерживаемой версии
tasq compact [--before DATE] [--dry-run]        вынести журнал и историю старше горизонта в архив
tasq serve --http ADDR [--profile P]            MCP по HTTP: loopback, только по флагу (§13.3)
tasq doctor [--check] [--gc] [--json]             целостность БД, истёкшие аренды, осиротевшие резервы
tasq --version | --help
```

Соглашения: машиночитаемый вывод — `--json` у любой команды; коды выхода — `0` успех, `1` ошибка
предметной области, `2` неверные аргументы, `3` отказ из-за внешнего файла (непарсимый конфиг).
Глобальные флаги (работают с любой командой): `--db PATH` / `TASQ_DB` — файл базы,
`TASQ_SESSION` — идентификатор сессии для привязки `next` к проекту (нет переменной — нет
привязки),
`--project KEY` / `TASQ_PROJECT` — проект, в котором идёт работа, `--flush SECS` /
`TASQ_FLUSH_SECONDS` — предел, сколько секунд данные могут лежать незаписанными у `tasq mcp`
(по умолчанию 3, `0` — выключить; см. §17.0.1).
Человекочитаемый вывод CLI — английский (как и описания инструментов): это поверхность, которую читают
и агент, и скрипты; README и документация — русские.

Автодополнение собирается из того же описания CLI (clap), поэтому разойтись с интерфейсом не может:
новая подкоманда или флаг появляются в скрипте сами. Базы команда не требует — дополнение настраивают
один раз на машине, в том числе до `tasq init`:

```
tasq completions bash > ~/.local/share/bash-completion/completions/tasq   # bash
tasq completions zsh  --out ~/.zfunc/_tasq                                # zsh (после fpath+=~/.zfunc)
tasq completions fish --out ~/.config/fish/completions/tasq.fish          # fish
```

`--out` создаёт каталог назначения сам: пути дополнений на чистой машине ещё не существует.

### 15.1. Несколько проектов в одной базе

База — не проект: в одном файле может быть сколько угодно проектов, и каждый узел принадлежит
ровно одному. Проект — это и есть dot-префикс (`TASQ-12.3`, `PRJ-4`), поэтому два проекта в одной
базе не мешают друг другу и не требуют второй базы.

```
tasq create project PRJ --name "Новый проект"   создать проект в текущей базе
tasq list projects                              ключ, статус, число живых узлов; активный помечен *
tasq --project PRJ add "Задача"                 работать в выбранном проекте
export TASQ_PROJECT=PRJ                         то же для серии команд
```

Правила, которые важно знать:

- **Проект — самостоятельная сущность иерархии, а не «текущий каталог работы».** Он выбирается
  по тому, над каким проектом идёт работа: `--project KEY`, `TASQ_PROJECT` или `project_select` в
  сессии MCP. Молчаливого «первого по порядку создания» нет — с ним запись уходит в чужие данные и
  ничем себя не выдаёт.
- **Один проект в базе — выбора нет, и это не дефолт.** Если проект в базе единственный, он
  однозначен и селектор не нужен: существующие однопроектные базы и скрипты работают как раньше.
- **Два и больше — отказ `PROJECT_REQUIRED` со списком ключей.** Команда не угадывает: `no project
  selected: this command works in one project (known: ALPHA, PRJ)`. Какой проект твой, знает только
  тот, кто ставит задачу.
- **Флаг важнее переменной окружения.** `--project` перекрывает `TASQ_PROJECT`; явное значение всегда
  сильнее окружения. В MCP аргумент `project` у вызова важнее проекта, выбранного в сессии, и не
  меняет его — можно читать чужой проект, не переключаясь.
- **Ключ нормализуется к верхнему регистру:** `tasq create project prj` создаёт `PRJ`. Ключ входит
  в dot-ID, а он всегда заглавный.
- **Неизвестный ключ — `PROJECT_NOT_FOUND` с перечнем доступных:** `project NOPE not found
  (known: ALPHA, PRJ)`. Молчаливого «взяли проект по умолчанию» нет.
- **Обзор по умолчанию — все проекты.** `tasq ls` без селектора в базе с несколькими проектами
  показывает узлы всех: адрес узла самодостаточен (начинается с ключа своего проекта), поэтому
  список остаётся однозначным. `--project all` — та же форма, сказанная явно; `--project KEY`
  сужает до одного проекта. `--except-project KEY` (повторяемый) убирает названные проекты и
  отвергает неизвестный ключ: опечатка иначе выглядела бы как «проект и так не показывался».
  `-n` / `--no-done` гасит закрытое словом `done`; отменённое остаётся, а явный `--status done`
  сильнее флага. `--parent` в режиме всех проектов отвергается (`INVALID_VALUE`): родитель живёт
  внутри проекта, и молчаливо проигнорировать ссылку значило бы выдать не тот список.
- **Доска и поиск тоже не требуют проекта.** `tasq board --all` печатает секцию на проект
  (`--json` — массив досок, не одна); `tasq find` без `--project` ищет по всей базе, а с
  `--project` — только в нём. Счётчики каждой секции — про свой проект: сложенные вместе числа
  не относятся ни к одному из них. Секция проекта без узлов не печатается — пустая секция это
  не сведения. В MCP то же самое: `tasq_dashboard` с `all: true` отдаёт массив секций, а
  `tasq_search` без `project` ищет по всей базе. Искать «в первом проекте по порядку» нельзя:
  ответ «не найдено» на существующую задачу хуже находки из чужого проекта — она хотя бы правдива.
- **Счётчики в `list projects`** считают живые узлы своего проекта, удалённые не считаются.
- **Переименование ключа меняет все dot-ID проекта разом.** `dot_id` собирается из ключа при
  чтении, поэтому `--new-key BETA` не правит строки узлов: адреса `PRJ-1.2` становятся `BETA-1.2`
  одним обновлением. Команда говорит об этом вслух («all dot-IDs of this project now start with
  BETA-»), потому что внешние ссылки в тексте задач и коммитах остались прежними.
- **Удаление защищено двумя отказами.** Непустой проект не удаляется без `--force` (`PROJECT_NOT_EMPTY`
  с числом узлов), последний проект в базе — тоже (`LAST_PROJECT`): пустая база делает все команды
  бесполезными. Оба отказа советуют выход: архивировать (`--status archived`) или удалить сам файл базы.
- **Удаление каскадное и необратимое:** узлы, рёбра, заметки, комментарии, попытки, резервы и лог —
  всё уходит вместе с проектом по `ON DELETE CASCADE` схемы. `doctor --check` после удаления чист.
- **В MCP контекст сессии начинается без проекта**, если их в базе несколько: `project_list`,
  `project_create` и `tasq_init` работают всегда, остальные инструменты отвечают `PROJECT_REQUIRED`.
  Проект выбирается `project_select` (сессия) или аргументом `project` у конкретного вызова —
  второй вариант не меняет сессию, поэтому один агент может читать несколько проектов подряд.
- `project_update` правит проект сессии (или указанный `key`) и, если ключ сменился, переводит
  контекст сессии на новый ключ. `project_delete` при удалении проекта сессии оставляет сессию без
  проекта; переезд бывает только когда проект в базе остался ровно один — тогда `switched_to`
  называет его, иначе `active_project: null` с подсказкой. `project_delete` входит только в
  профиль `full`.
- **В MCP то же самое, но с явным переключением сессии:** `project_create` создаёт проект и
  **не** переключает сессию (в ответе `switched: false` и подсказка), `project_select` переключает
  и возвращает `previous`/`active_project`. Молчаливый переход контекста недопустим: агент,
  создавший проект «на будущее», иначе писал бы узлы не туда. Сервер можно сразу запустить в нужном
  проекте: `tasq mcp --project PRJ`.

## 16. Импорт/экспорт и совместимость с saga

- `tasq export` — вложенный JSON: проект, дерево узлов (dot-ID + локальные id), рёбра, комментарии,
  заметки, шаблоны, снимки burn-up; `schema: "tasq.export.v1"`; в конце — `sha256` канонического JSON
  (ключи отсортированы, без отступов) для проверки целостности.
- **Форма документа**: `{schema, version, exported_at, sha256, data:{project, nodes, edges, comments,
  notes, templates, snapshots}}`. Сумма считается по `data`, а не по документу целиком: `exported_at`
  и версия бинаря меняются от запуска к запуску, и сумма обязана зависеть только от содержимого —
  иначе одинаковые данные давали бы разные суммы и проверка теряла смысл.
- `tasq import --mode merge|replace` — одна транзакция: сначала полная валидация (ссылки, циклы,
  дубликаты dot-ID), потом запись; при любой ошибке — откат целиком.
- Импорт из saga: `tasq import export.json --from-saga` — маппинг: `projects → projects`,
  `epics/tasks/subtasks → nodes` (эпик depth 1, задача depth 2, подзадача depth 3),
  `task.dependencies → edges(blocks)`, `comments/notes` — как есть, `activity_log` — только
  `summary`+`created_at` (актор неизвестен → `actor = "saga"`). Числовые id saga → `metadata.saga_id`,
  dot-ID строится заново. Поддерживаются `format_version` 1.0…1.3; шаблоны saga не экспортирует,
  поэтому их в импорте нет — это ограничение исходного формата, а не потеря.
- Статусы saga при импорте транслируются: `completed`/`done → done`, остальные совпадают по имени.
  Все значения перечислены явно: пропущенная ветка молча превратила бы готовую подзадачу в `todo`
  (такой дефект и был пойман импортом реальной фикстуры `1.3`).
- Экспорт попыток и аренд в файл не входит: это состояние живой машины. Поэтому документ
  одного проекта **не восстанавливает базу**: без `attempts`, `reservations`, `activity_log` и
  остальных проектов `burnup` и `time` на нём пусты, а история начинается с нуля.

### 16.0. Миграция схемы: отдельная команда, не побочный эффект открытия

Бинарь новее базы не трогает её сам: открытие читает версию и, если она отстала, отказывает
кодом `SCHEMA_OUTDATED` с подсказкой `tasq migrate`. Обновляет схему только эта команда:

```
tasq migrate [--db PATH] [--json]     v10 -> v11 (или «nothing to do»)
```

Почему не автоматически при открытии, хотя так было сделано в 0.9.0. Открытие вызывается
каждой командой, а миграция — это запись. Сто процессов, запущенных одной волной на файле
старой схемы, видят одну и ту же версию и все берутся обновлять: блокировку получает один,
остальные ждут её и падают на «database is locked», хотя собирались только читать. Замер на
десятитысячной базе: 60–67 успешных из 100 при чтении, и ни одной причины в сообщении — оно
говорит про блокировку, а не про схему. Теперь волна даёт 100 отказов `SCHEMA_OUTDATED` и не
меняет файл ни на байт.

Молчать при этом нельзя, и это вторая половина решения: команда на старой схеме прочитает то,
чего в базе ещё нет, и ответит неполно — так `find` по комментариям отвечал запасным путём
`LIKE`, пока индекса не существовало вовсе. Поэтому отказ, а не тихая работа: «работает, но
иначе» хуже видимой ошибки.

Миграция идёт в транзакции, с выключенными на время внешними ключами и проверкой нарушений до
коммита. Повторный запуск безопасен и печатает `nothing to do` — скрипт обновления запускают
дважды. Если блокировку держит кто-то посторонний и все три попытки истекли, ответ —
`MIGRATION_BUSY`, а не «база занята».

Где миграция остаётся частью операции: `tasq init` (создание и доведение базы — одна работа) и
восстановление из дампа (`tasq load`, `tasq import` — схема приводится к текущей, иначе заливать
некуда).

Дамп остаётся путём для переноса между машинами и для редизайна, требующего пересборки таблиц.

### 16.0.1. Политика хранения: архив вместо удаления

`activity_log` и `node_status_history` только дописываются и растут без предела. Замер на
фикстуре 10 000 узлов: история статусов — 0,78 МБ, её индексы — ещё 1,07 МБ, всего 43% файла в
4,3 МБ при 1,37 строки на узел. У живого узла переходов больше, поэтому «до 50 МБ на 100 000
узлов» из §17.2 — не потолок, а нижняя оценка.

```
tasq compact [--before YYYY-MM-DD] [--dry-run] [--json]
```

Строки старше горизонта (по умолчанию год) переезжают в файл рядом — `<имя>-archive.db` — и
**не удаляются**. Причины две, обе про молчаливую потерю данных:

- удаление истории сделало бы `burnup` ложным: узел без истории попадает в счётчик «неизвестно»,
  то есть выглядит несделанным. Отчёт был бы уверенным и неправильным;
- удаление журнала оборвало бы `log` и `session_diff` на дате усечения, и лента выглядела бы так,
  будто история началась вчера.

Поэтому архив подключается к каждому соединению (`ATTACH`), а читающие запросы — `history`,
`burnup`, `log`, дамп — видят объединение: основная база плюс архив. Проверка этого и есть
приёмка задачи: `board` и `burnup --since` после выноса отвечают **теми же числами**, а размер
основного файла падает.

Как это устроено:

| шаг | что происходит |
| --- | --- |
| `--dry-run` | считает строки и оценивает освобождение; ничего не меняет и файл архива не создаёт |
| перенос | одна транзакция: `INSERT OR IGNORE` в архив, затем `DELETE` из основной |
| усечение | `VACUUM` плюс checkpoint: без этого освободившиеся страницы остаются в списке свободных, и файл не уменьшается |
| отчёт | `doctor` называет архив отдельной строкой — «база стала меньше» без второй половины картины читается как «данные пропали» |

Повторный `compact` того же диапазона безопасен: `INSERT OR IGNORE` по `id` не задваивает строки,
а второй прогон находит ноль. Дамп (`tasq export`, `tasq dump`) читает историю и журнал через то же
объединение, поэтому вынесенные строки в него попадают — иначе переезд терял бы прошлый год молча.

### 16.1. Дамп всей базы

`tasq export --all` — отдельный вид документа: **все** проекты и **все** таблицы, включая аренды,
резервы и журнал. Нужен ровно затем, чтобы редизайн схемы не терял данные: дамп снимается до
пересборки и заливается после (путь апгрейда — `docs/storage-redesign.md`).

Форма: `{kind: "tasq-dump", format: 1, schema_version, version, exported_at, counts, tables, sha256}`.
В `tables` — по массиву строк на таблицу, колонки как в базе; строки читаются универсально
(`SELECT *` и типы значений), поэтому дамп переживает правку схемы, а не требует нового кода под
каждый столбец. FTS-индексы в дамп не идут: они перестраиваются из `nodes`/`notes`.

Сумма считается по `{kind, format, schema_version, tables}` — без `exported_at` и версии бинаря,
поэтому две выгрузки одного состояния дают одинаковый `sha256` (это и есть тест).
Селектор проекта не нужен: `--project` в режиме `--all` игнорируется, потому что берётся всё.

### 16.2. YAML

Тот же документ (и дамп, и экспорт проекта) доступен в YAML: `--format yaml` или расширение
`.yaml`/`.yml` у `--out`. Формат выбирает вызывающий — флаг важнее расширения, расширение важнее
JSON; догадки по содержимому файла нет, потому что дамп — состояние, а не конфиг. Неизвестный
`--format` — отказ до записи файла (`INVALID_VALUE`), а не молчаливый JSON.

Парсер — `serde-saphyr` (YAML 1.2, чистый Rust, `#![forbid(unsafe_code)]` у самого крейта): C-код
и `unsafe-libyaml` в дерево не тянутся. Комментарии и якоря при чтении поддерживаются — дамп
правят руками.

Проверка — свойствами, а не «файл похож на YAML»: YAML разбирается обратно в тот же документ,
`sha256` JSON и YAML одного состояния совпадает, `.yaml` без флага формата действительно YAML.

### 16.3. Восстановление из дампа

`tasq load <файл> [--db PATH] [--force]` — заливает дамп в базу. Команда названа `load`, а не
`restore`: `restore` уже занято возвратом мягко удалённого узла, и один глагол на два разных
действия — источник дорогих опечаток.

Дамп — **состояние, а не действие**: матрица переходов не применяется, аренды не проверяются,
журнал не пишется. Поэтому `load` восстанавливает то, что `import` восстановить не может —
аренды, резервы, историю, все проекты сразу.

Свойства:

- одна транзакция; строки вставляются в порядке `DUMP_TABLES` (родители раньше детей), внешние
  ключи отложены до конца (`defer_foreign_keys`) — иначе порядок строк в дампе решал бы, пройдёт
  вставка или нет: узел после `mv` вполне может иметь `parent_id` больше своего `id`, и такая
  база цела;
- колонки берутся из текущей схемы: поле, которого в ней больше нет, — потеря с названием поля;
  поле, которого не хватает, — потеря таблицы целиком, сообщённая один раз, а не строкой на
  каждую запись;
- потеря называется адресом: `RRL-3.1` для узла, `TASQDEV-1.2` для аренды или комментария на нём;
- отчёт строится до коммита. Есть потери — транзакция откатывается целиком, код выхода **3**
  (0 — всё перенесено, 1 — ошибка вызова). Половинчатая база хуже отказа: на неё потом смотрят
  как на рабочую;
- чужой файл, `format` из будущего и `schema_version` новее поддерживаемой отвергаются до записи;
  непустая база требует `--force`;
- без `--db` целевой файл — `.tasq/tasq.db` в текущем каталоге. Поиск базы вверх по дереву здесь
  намеренно не делается: восстановление не должно молча переписать чужую базу.

FTS-индексы не переносятся, а перестраиваются триггерами при вставке — поэтому `find` на
восстановленной базе работает сразу.

**Поиск идёт по трём поверхностям: узлы, заметки и комментарии.** Комментарии добавлены схемой v11
(`comments_fts`) и это не украшение: на живой базе их 250 против нуля заметок, и обоснования
решений живут в них. Ответ по комментарию называет узел в `dot_id`, а заголовком идёт первая строка
текста с автором. Мягко удалённый комментарий остаётся находимым — он часть истории, как архивный
узел, — а комментарии удалённого узла не находятся: адрес в ответе был бы битым.

**Поиск говорит, когда ответил не индексом.** Поиск идёт по трём поверхностям (узлы, заметки,
комментарии) и по каждой сначала пробует FTS5, а при отказе отступает к сравнению по словам. Это
отступление раньше было молчаливым, и человек видел другой ответ, не зная об этом; на живой базе
так отвечал даже обычный `find "миграция"`, потому что схема оставалась старой и индекса не было
вовсе (TASQDEV-19).

Теперь ответ несёт своё качество:

```json
{"hits": […], "complete": false,
 "degraded": [{"surface": "comments", "reason": "index_missing",
               "note": "search: comments answered without an index (it is missing); … run `tasq migrate`"}]}
```

| `reason` | что значит | что делать |
| --- | --- | --- |
| `query_not_understood` | запрос не выражение FTS5 (незакрытая кавычка, голый `OR`) | ничего: искали по словам, находки те же |
| `index_missing` | индекса нет или он повреждён | `tasq migrate` |

`complete` — отдельное поле, а не вывод потребителя: одно булево значение нельзя случайно не
заметить. В человеческом выводе `tasq find` предупреждение идёт в **stderr**, а находки — в
stdout, чтобы разбор вывода скриптом не сломался. Причина различается намеренно: советовать
`migrate` в ответ на двоеточие в запросе было бы так же неверно, как молчать об отсутствующем
индексе.

**Метки и свободные поля узла — не украшение схемы.** `nodes.tags` и `nodes.metadata` были с первой
версии и не заполнялись ничем: поле, обещающее возможность, хуже отсутствующего. Теперь метки
пишутся (`add --tag`, `set --tag|--untag|--tags`, `tags` в шаблоне и в MCP), читаются (`show`,
`--fields tags,metadata`) и отбираются (`ls --tag` — точное сравнение в SQL через `json_each`, не
подстрока). Метка — МНОЖЕСТВО: порядок и повторы нормализуются при записи, иначе перестановка
выглядела бы правкой и писала бы пустую строку в журнал. Свободные поля (`set --meta key=value`,
`--meta-remove key`) — для того, чему не нужна своя колонка.

**Граница между ними одна и она про назначение, а не про вкус.**

| | метка (`nodes.tags`) | свободное поле (`nodes.metadata`) |
| --- | --- | --- |
| отвечает на вопрос | «к чему относится» | «какое у него свойство» |
| по ней | ОТБИРАЮТ: `ls --tag`, шаблоны, фильтр в MCP | не отбирают |
| хранение | своя колонка, множество строк | объект строк, чужой формат |
| кто читает | код и человек | тот, кто записал; код — только зарезервированные ключи |

Сложить метки в свободные поля значило бы превратить отбор в разбор JSON: сегодня `ls --tag`
сравнивает точное значение в SQL и читает только совпавшие строки, а с метками внутри `metadata`
пришлось бы прочитать все узлы проекта и отбросить их в Rust — то есть отбора не осталось бы.
Поэтому колонка у меток своя, а граница объявлена кодом: реестр `src/meta.rs`.

**Зарезервированные ключи** — то единственное, что код читает или пишет в свободных полях. Каждый
назван в одном месте, с аксессором, и проверяется тестом на ЧТЕНИЕ:

| поле | ключ | смысл |
| --- | --- | --- |
| `nodes.metadata` | `saga_id` | происхождение узла в системе-предшественнике; пишется импортом, читается `Node::saga_id()` |
| `projects.metadata` | `reopen` | политика реоткрытия (`force_required`/`allow`), §7.1 |

Всё остальное в `metadata` tasq обязан переносить как есть и не толковать. Новый зарезервированный
ключ добавляется только вместе с аксессором и тестом: ключ, который читает код, — это скрытая
колонка, и без типа она ведёт себя как `saga_id`, пришедший числом: узел перестаёт читаться
целиком (`invalid type: integer, expected a string`), и ошибка не называет причину. Тип значения
приводится на границе (`node::normalise_metadata_json`): числа и флаги становятся текстом, потому
что хранимое поле — объект строк.

### 7.1. Строгость реоткрытия — политика проекта

Матрица переходов отвечает, КУДА можно. Отдельный вопрос — нужен ли `--force`, чтобы вернуть
закрытый узел (`done`, `cancelled`) в работу. Это настройка проекта, а не правило кода:
`projects.metadata.reopen`.

| значение | поведение |
| --- | --- |
| `force_required` (по умолчанию) | реоткрытие только с `--force` |
| `allow` | реоткрытие без флага |

Политика читается один раз при выборе проекта и едет вместе с ним, поэтому проверка перехода не
ходит в базу. Неизвестное значение в `metadata.reopen` читается как `force_required`, а не как
«разрешить»: опечатка в настройке не должна ослаблять правило. Политика меняет только строгость —
переход, которого нет в матрице, остаётся запрещённым (`INVALID_TRANSITION`), сколько бы
разрешающей ни была настройка.

## 17. Нефункциональные требования

### 17.0. Кодировка и сравнение текста

SQLite хранит текст в UTF-8, поэтому многобайтовые символы (кириллица, эмодзи) сохраняются и
возвращаются без потерь — отдельного «utf8mb4» режима, как в MySQL, здесь нет и не требуется.
Но у SQLite есть встроенное ограничение, аналогичное старому `utf8` против `utf8mb4`: **`lower()`,
`LIKE` и коллация `NOCASE` понимают только ASCII**. Без обхода `lower('Миграции')` возвращает строку
без изменений, и кириллический поиск молча становится регистрозависимым — при том что на ASCII
он «работает», и ошибка проявляется только на русском тексте.

Поэтому на каждом соединении регистрируются две функции:

- `uni_lower(text)` — приведение к нижнему регистру по правилам Unicode (Rust `str::to_lowercase`);
  используется во всех пословных/LIKE-выборках (`overview::search`, `note::search`);
- коллация `UNICODE_NOCASE` — сравнение и сортировка без учёта регистра для любого письма;
  для сортировки по названиям вместо `NOCASE`.

Полнотекстовый индекс FTS5 собирается токенизатором `unicode61 remove_diacritics 2`, который
регистр и диакритику разбирает сам; `uni_lower` нужен для путей, где FTS-выражение не применимо
(пунктуация в запросе, поиск по заметкам до индекса).

Проверяется тестами: `uni_lower('МИГРАЦИИ') = 'миграции'`, сравнение `'Миграции' = 'миграции'
COLLATE UNICODE_NOCASE`, поиск в верхнем и нижнем регистре в обоих путях.

### 17.0.1. Долговечность записи: сколько работы можно потерять

Требование: при отключении питания теряется не больше **нескольких секунд** работы. Это не
бесплатное свойство WAL, и вот почему.

`journal_mode = WAL` + `synchronous = NORMAL` означает: коммит **не** делает fsync, а только
дописывает кадры в журнал. Пока не случится checkpoint, данные лежат в кэше страниц ядра.
Замер на живой MCP-сессии (`strace -tt`, паузы 5 с между записями, 205 коммитов):

| наблюдение | значение |
| --- | --- |
| fsync за 205 коммитов | 10, все — на открытии, checkpoint и закрытии |
| первый fsync после пачки записей | через 25 с (и только потому, что сработал checkpoint по объёму) |
| прирост журнала на один `node_create` | 45 320 байт = 11 кадров по 4 КБ |
| checkpoint по умолчанию | `wal_autocheckpoint = 1000` страниц ≈ 4 МБ ≈ 90 коммитов |

Отсюда важное: окно потерь ограничено **объёмом**, а не временем. При паре записей в час данные
могли бы не попасть на диск сутками. Что при этом спасает: кэш страниц переживает падение
процесса (`kill -9`, OOM, падение сервера), поэтому теряются данные только при отказе машины —
но именно этот случай и нормируется.

Решения:

- **Периодический сброс по времени.** MCP-сервер держит поток `tasq-flush`, который каждые
  `--flush` секунд (по умолчанию 3, `TASQ_FLUSH_SECONDS`, `0` — выключить) делает
  `PRAGMA wal_checkpoint(PASSIVE)`: кадры копируются в базу и синхронизируются, читателя он
  не ждёт и не отказывает. Замер: при `--flush 3` журнал переливается на третьей секунде,
  при `--flush 0` остаётся непустым.
- **Усечение — отдельный, редкий шаг.** `PASSIVE` не уменьшает файл (замер: 6,8 МБ до и после,
  при любом `journal_size_limit`), а `TRUNCATE` усекает, но ждёт освобождения читателей до
  `busy_timeout` (замер: 5 000 мс при умолчании SQLite, 204 мс при нашем). Поэтому усечение
  выполняется только когда переливка прошла полностью И файл дорос до `JOURNAL_LIMIT` (8 МБ):
  тогда ожидание ограничено, а журнал не растёт безгранично.
- **Разовые команды CLI уже долговечны:** закрытие соединения делает checkpoint, поэтому
  `tasq add …` оставляет после себя пустой журнал. Поток нужен именно долгоживущему серверу.
- **`journal_size_limit = 8 МБ`** ставится только соединению сброса: он живёт в соединении, а не
  в файле (замер: в новом соединении снова `-1`), и усекает журнал именно фоновый поток.
- **Сброс может застрять, и это видно.** `PASSIVE` не переливает кадры, которые держит
  читательская транзакция (замер: 7 кадров в журнале, перелито 3). Тогда окно потерь растёт
  без предела, поэтому поток
  считает тики без успеха и после третьего пишет в stderr
  `journal is not being flushed: another connection holds a read transaction…`. Тихо это
  происходить не должно: иначе «предел 3 секунды» превращается в обещание без основания.
  Поток сброса открывает **своё** соединение без `PRAGMA journal_mode` и с коротким
  `busy_timeout = 200`: переключение режима требует исключительной блокировки, и с
  пятисекундным ожиданием поток зависал на самом открытии, то есть не сбрасывал именно тогда,
  когда сброс стал нужен.
- **Чего сознательно не делаем: `synchronous = FULL`.** Он даёт нулевое окно, но добавляет fsync
  на каждый коммит — на этом диске ≈19 мс на вызов (серия из 8 fsync заняла 150 мс), то есть
  импорт на тысячу узлов стал бы на двадцать секунд медленнее. Для трекера задач потеря трёх
  секунд приемлема, платить за это каждой командой — нет.
- **Резервная копия — не `cp`.** Свежие данные живут в `-wal`, который может быть больше самой
  базы; копирование `.db` в одиночку даёт устаревший снимок. Правильный способ — `VACUUM INTO`
  (см. §17.2).

### 17.1. Язык и сборка

- Rust, edition 2024; версия тулчейна пинится в `rust-toolchain.toml` (на машине `cargo 1.99.1`).
- `#![forbid(unsafe_code)]` в `lib.rs` и `main.rs`; `lib.rs` + тонкий `main.rs` (модули тестируются
  интеграционно).
- Зависимости — диапазоном версии (`clap = "4.6"`, `rusqlite = "0.41"`, `serde_json`, `time = "0.3"`,
  `thiserror = "2"`, `tracing = "0.1"`, `uuid = "1"`, `tiktoken-rs = "0.12"`, `unicode-width = "0.2"`).
  `*` и `==` не используются.
- SQLite — `rusqlite` с фичей `bundled`: Си-код SQLite компилируется в бинарь статически, системной
  библиотеки не требуется. Это осознанный компромисс: зрелость и `integrity_check` важнее «чистого»
  Rust; альтернативы на чистом Rust (turso/limbo) для v1 не берём.
- Целевой бинарь: ≤ 20 МБ, статический по отношению к sqlite/OpenSSL-зависимостям (TLS не используется).

### 17.2. Производительность

| операция | требование |
| --- | --- |
| старт CLI (`--version`, `next`) | ≤ 50 мс |
| `tasq_claim` на 10 000 узлов | p99 ≤ 5 мс |
| `tasq_dashboard` на 10 000 узлах / 500 эпиках | ≤ 50 мс |
| burnup за 365 дней | ≤ 100 мс (снимки) / ≤ 1 с (пересчёт из лога) |
| RSS | ≤ 60 МБ на 100 000 узлов |
| размер `.tasq/tasq.db` | ≤ 50 МБ на 100 000 узлов с логом |

### 17.3. Надёжность и корректность

- `PRAGMA synchronous=NORMAL; foreign_keys=ON; busy_timeout=200` (переменная `TASQ_BUSY_MS` для
  стендов) — эти прагмы живут в соединении и повторяются на каждом открытии. `journal_mode=WAL`
  — свойство файла, и ставится только когда режим ДРУГОЙ (создание базы, восстановление, перевод
  старого файла); `journal_size_limit` ставится соединению сброса. Команда, которая только читает,
  открывает базу без права записи; при «занято» на открытии или `BEGIN IMMEDIATE` — до трёх
  повторов с разбросом 5–25 мс. Отказ после повторов на миграции — отдельный код
  `MIGRATION_BUSY`, а не `STORAGE_ERROR`; сама миграция — только `tasq migrate` (§16.0).
- Любая мутация — одна транзакция с записью в лог; частичных состояний не бывает.
- Ошибки — типизированные коды (§19), а не строки; `--json` отдаёт `{"error":{"code":…,"message":…,"hint":…}}`.
- Fail-fast: неизвестный `kind`, статус, профиль, вид ресурса, конфликтная конфигурация — ошибка сразу,
  без «мягкой» подстановки значения по умолчанию.
- Конкурентность проверяется тестами: N процессов одновременно захватывают один узел, побеждает ровно
  один; `kill -9` держателя не оставляет узел в `in_progress` навсегда.
- **Очередь вызовов через MCP измерена отдельно от CLI** (`make queue`, `tests/mcp_queue.rs`).
  Сервер читает stdin построчно и обрабатывает по одному, поэтому в живом режиме агента нет ни
  файловых блокировок, ни повторов — есть очередь внутри процесса. Замер: сто вызовов `node_create`
  в один сервер — 51–66 мс (0,55–0,64 мс на вызов), ни одного отказа, все сто узлов созданы; сто
  ОТДЕЛЬНЫХ серверов на одну базу — 358–364 мс, p95 227 мс, отказов тоже нет. Чтение писателя не
  ждёт: пока сервер создаёт сотню узлов, доска отвечает за 5,7 мс в худшем случае.
  Отсюда вывод для транспорта: очередь записи не узкое место, и HTTP ради неё не нужен (§13.3).

### 17.4. Безопасность

- Ядро не открывает сетевых соединений; `connect` только читает/пишет локальные файлы конфигов.
- База выбирается тремя способами: флаг `--db PATH`, переменная `TASQ_DB` (удобно прописать
  в конфиге MCP-клиента) и поиск `.tasq/tasq.db` вверх от текущего каталога. Порядок именно такой:
  явное значение важнее окружения, окружение важнее поиска. Явно заданный путь не создаётся
  молча: если файла нет — это `DB_NOT_FOUND` с названным путём, а не пустая база и ошибка схемы.
  `tasq init` уважает `--db`/`TASQ_DB` (до 0.1.2 он их игнорировал и создавал базу в текущем
  каталоге — флаг работал для всех команд, кроме той, что создаёт базу).
- `tasq mcp` стартует и **без** базы: клиент получает подключение и каталог инструментов, а
  внятный отказ `DB_NOT_FOUND` с подсказкой приходит на сам вызов. Инструмент `tasq_init` создаёт
  базу прямо из сессии. Раньше сервер закрывал соединение, и MCP-клиент видел только
  «connection closed» — без причины и без способа это исправить.
- `--profile read-only` не отдаёт инструментов, меняющих состояние.
- Пути резервов не выходят за корень проекта; символические ссылки не разыменовываются.
- Секретов в БД нет; `metadata` — свободное поле, но в документации помечено как «не для секретов».
- Данные одного проекта не видны при работе с другим: `project_id` подставляется из контекста, а не
  берётся из аргумента за пределами явного `--project`.

### 17.5. Детерминизм и совместимость

- Одинаковые входные данные → одинаковый экспорт (канонический JSON, сортировка ключей).
- Время — UTC; никаких локальных зон в хранилище.
- `schema_version` в `meta`; миграции — вперёд, без «молчаливого» игнорирования неизвестной версии
  (`SCHEMA_TOO_NEW`). Отставание схемы — тоже отказ (`SCHEMA_OUTDATED`), а не тихая работа на
  старых таблицах; обновляет только `tasq migrate`.

## 18. Коды ошибок предметной области

`NODE_NOT_FOUND`, `AMBIGUOUS_ID`, `NODE_CLOSED`, `INVALID_TRANSITION`, `CHILDREN_UNFINISHED`,
`UNMET_PREREQUISITES`, `DESCRIPTION_LOCKED`, `ACTIVE_ATTEMPT_EXISTS`, `LEASE_EXPIRED`,
`NOT_YOUR_ATTEMPT`, `LEASE_REQUIRED`, `RESOURCE_RESERVATION_CONFLICT`, `TOO_MANY_RESERVATIONS`,
`PATH_OUTSIDE_PROJECT`, `CYCLE_DETECTED`, `ANCESTOR_BLOCK_EDGE`, `SELF_EDGE`, `CROSS_PROJECT_EDGE`,
`DUPLICATE_KEY`, `TOOL_NOT_IN_PROFILE`, `SCHEMA_TOO_NEW`, `EXPORT_CHECKSUM_MISMATCH`, `CONFIG_UNPARSABLE`,
`BRANCH_UNAVAILABLE`, `SCHEMA_OUTDATED`, `MIGRATION_BUSY`.

Каждая ошибка несёт `code`, `message`, `hint` и, где уместно, машиночитаемые детали
(`holder`, `expires_at`, `dot_id`, `blockers[]`).

## 19. Тестирование и ворота

| ворота | что проверяют |
| --- | --- |
| `cargo fmt --all -- --check` | форматирование |
| `cargo clippy --all-targets -- -D warnings` | без предупреждений |
| `cargo test --all-targets` | юнит + интеграционные |
| `cargo deny check` | advisories, лицензии, источники |
| `tasq tokens --max 6000` | бюджет схем MCP (и по профилям) |
| `tasq doctor --check` | `PRAGMA integrity_check`, осиротевшие резервы, истёкшие попытки |
| `make queue` | очередь вызовов через MCP: 100 вызовов в один сервер без отказов и в бюджет |
| `cargo test --test compact` | вынос в архив: размер падает, `board`/`burnup` не меняются, дамп видит вынесенное |
| `make perf` | бюджет p95 на фикстуре 10 000 узлов |

Обязательные тесты по существу:

- **Конкурентный захват**: 2–10 процессов, один узел — ровно один `claim`, остальные получают
  `ACTIVE_ATTEMPT_EXISTS` со своим телом. Повтор на 100 итерациях.
- **Смерть держателя**: `kill -9` в середине работы; другой процесс захватывает после `expires_at`;
  чекпойнт передан; прежняя попытка закрыта как `expired`/`superseded`.
- **Конфликты резервов**: таблица случаев (`src/api` ↔ `src/api/x.rs`, `src/apix`, `src/**/*.rs`,
  `port:8080`, `shared` против `exclusive`).
- **Циклы**: property-тест на случайных графах — после вставки любого ребра граф `blocks` ацикличен.
- **Dot-ID**: перенос узла с потомками — все пути пересчитаны, старые записи в логе; разрешение
  ссылки по трём формам; неоднозначность → `AMBIGUOUS_ID`.
- **Burn-up**: восстановление ряда из лога совпадает со снимками день в день; отмена узла уменьшает
  объём; добавление увеличивает `scope_added`.
- **Просрочка**: границы дня в часовом поясе проекта, `done` не считается, `blocked` считается.
- **Импорт/экспорт**: round-trip без потерь; испорченный `sha256` → `EXPORT_CHECKSUM_MISMATCH`;
  импорт saga-примера.
- **На входе извне** (вместо `cargo-fuzz`, который требует nightly): property-тесты в
  `tests/properties.rs` — разбор ссылки на произвольных строках, идемпотентность нормализации
  резервов, симметричность конфликтов, подстановка переменных шаблона, побайтово повреждённый
  документ импорта. Свойство одно: **вход любого вида не роняет процесс**, отказ — доменная
  ошибка с кодом. 256 кейсов на свойство: вход, который ломает разбор, обычно редкий.
  Property-тесты нашли два реальных дефекта нормализации резервов: шаблон `./` превращался
  в пустую строку и не проходил повторную нормализацию, а путь с табуляцией перед слэшем
  (`a\t/`) нормализовался в `a\t` и на втором проходе — уже в `a`, то есть заявка молча
  переименовывалась. Оба исправлены: префикс снимается до проверки на пустоту, а пробелы
  на краю сегмента отвергаются, потому что представить их устойчиво нельзя.
- **MCP**: рукопожатие + `tools/list` + `tools/call` через pipe; компактный JSON-ответ укладывается в бюджет.

