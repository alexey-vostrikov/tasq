# tasq — локальный трекер задач для ИИ-агентов (ТЗ)

`tasq` — один статический бинарь на Rust: дерево работ (проект → эпик → задача → подзадача → …) в одном
SQLite-файле, MCP-сервер поверх stdio и CLI для человека. Предназначен для случая, когда работу ведут
несколько ИИ-агентов параллельно, а не один человек в браузере.

Документ — техническое задание. Кода пока нет: это контракт, по которому проект будет собран.

- имя проекта, пакета и бинаря: `tasq`
- репозиторий (локально): `~/DevOps/tools/tasq`; GitHub: `alexey-vostrikov/tasq`
- лицензия: MIT
- исходный класс задачи: `saga-mcp` (Node/npm), плюс приёмы из `mtix`, `rhizome-mcp`, `plandb`,
  `lific`, `backlog` — см. «Отношение к предшественникам».

---

## 1. Проблема, которую решает tasq

Агентская работа рассыпается на три вещи, которых нет в обычных трекерах:

1. **Контекст теряется между сессиями.** Jira и Linear рассчитаны на человека, который помнит план;
   агент — нет. Нужна структура, где сама иерархия несёт постановку: родитель объясняет «зачем»,
   ребёнок — «что именно сделать».
2. **Мёртвая работа.** Агент умирает на середине (таймаут, кончился контекст, упал процесс) и
   оставляет задачу в состоянии `in_progress` навсегда. Статус, который хранится, — ложь; статус,
   выводимый из живой аренды, — правда.
3. **Столкновения.** Два агента одновременно берут одну задачу или правят один файл. Блокировки
   нужны не на уровне «статус задачи», а на уровне «файл, каталог, порт, окно миграции».

`tasq` закрывает все три в одном бинаре, без сервера БД, без Docker и без сети.

## 2. Отношение к предшественникам

| Проект | Язык | Что берём | Чего у него нет |
| --- | --- | --- | --- |
| `saga-mcp` | TypeScript, npm | полный набор функций: иерархия, шаблоны с `{переменными}`, заметки, комментарии, лог активности, дашборд, импорт/экспорт, `source_ref`, учёт времени | dot-notation ID, аренды, резервирование ресурсов, зависимости между уровнями, просрочка, burn-up |
| `hyper-swe/mtix` | Go | dot-notation ID как «цепочка контекста», пересчёт прогресса вверх, матрица переходов, SHA-256 содержимого | шаблоны, заметки, аренды, резервирование, burn-up |
| `Odrin/rhizome-mcp` | Go | аренды с истечением, производный `in_progress`, резервирование ресурсов, профили инструментов, отдельный `--profile read-only` | шаблоны, заметки, дашборд, burn-up |
| `Agent-Field/plandb` | Rust | зависимости, пересекающие границы вложенности | зрелость (0.2.x), аренды, шаблоны |
| `VoidNullable/lific` | Rust | бюджет схем `tools/list` в токенах, `lific connect` — прописка MCP-конфига в 11 клиентов | иерархия эпик → задача → подзадача |
| `altenwald/backlog` | Go | burn-up, дедлайны с просрочкой | один файл без GUI, MCP-первый сценарий |

Формулировка одной фразой: **saga по функциям + dot-notation и аренды + резервирование ресурсов и
граф зависимостей сквозь уровни + дисциплина схем MCP и burn-up.**

## 3. Границы v1

**Входит в v1**

- Модель: проект → узел любого уровня (эпик, задача, подзадача, микро-задача), dot-notation ID.
- Аренды с истечением (claim/renew/release), производный `in_progress`, передача чекпойнта.
- Резервирование ресурсов: файлы, каталоги, glob, логические ресурсы (порт, слот деплоя, окно миграции).
- Зависимости `blocks` / `related` / `duplicates` / `discovered_from` / `split_from`, в том числе
  между узлами разных ветвей и уровней; детект циклов.
- Дедлайны, просрочка, burn-up по трём метрикам (штуки, очки, часы).
- Всё из saga: шаблоны с `{переменными}`, заметки 8 типов, комментарии с мягким удалением, лог
  активности, дашборд, «что дальше», пакетные операции, поиск, импорт/экспорт, `source_ref`, учёт времени.
- MCP-сервер stdio + профили инструментов + измеряемый бюджет схем в токенах.
- `tasq connect`: автоматическая прописка сервера в конфиги MCP-клиентов.
- CLI для человека: доска, дерево, burn-up, диагностика.

**Не входит в v1** (сознательно)

- Веб-интерфейс, GUI, TUI (TUI — этап после v1, ratatui).
- Сетевая синхронизация, серверный режим на несколько пользователей, авторизация, роли.
- Облако, аккаунты, телеметрия — tasq не открывает сокет без явного флага.
- HTTP-транспорт MCP: только loopback и только под флагом (см. §13.3), по умолчанию выключен.
- Синхронизация команды через общий Postgres (есть у mtix) — не нужна без многопользовательского режима.

## 4. Термины

- **Узел (node)** — единица работы любого уровня: эпик, задача, подзадача, микро-задача. Одна таблица,
  различие — глубина и `kind`.
- **Dot-ID** — человекочитаемый идентификатор вида `TASQ-12.3.1`: путь в дереве, а не случайное число.
- **Попытка (attempt)** — один захват узла одним исполнителем. Аренда живёт внутри попытки.
- **Аренда (lease)** — право работать над узлом до момента `expires_at`; продлевается heartbeat-ом.
- **Исполнитель (holder)** — строка-идентификатор агента (`claude-code`, `codex/session-42`), не учётная запись.
- **Резерв** — заявка попытки на ресурс (файл, каталог, glob, логический ключ) с обнаружением конфликтов.
- **Эффективный статус** — вычисляемый из хранимого статуса и живых аренд/блокеров; в БД не хранится.
- **Чекпойнт** — свободный текст/JSON, который умирающая попытка оставляет следующей.

## 5. Модель данных

Один файл на проект: `.tasq/tasq.db` (SQLite 3, WAL). Один файл может содержать несколько проектов —
как в saga; обнаружение идёт поиском `.tasq/` вверх от текущего каталога, переопределяется `--db` и
переменной `TASQ_DB`.

Принципы:

- **Хранится только факт, не вывод.** `in_progress` из аренды, прогресс из детей, `blocked` из рёбер,
  `actual_hours` из попыток — это представления, а не колонки, которые кто-то забыл обновить.
- **Мягкое удаление вместо `DELETE`** для узлов и комментариев: строка остаётся ради аудита, скрывается
  из выдач (`deleted_at IS NULL`), восстанавливается парным инструментом.
- **Один писатель.** Запись — только в `BEGIN IMMEDIATE`, `busy_timeout = 5000`, ретраи на `SQLITE_BUSY`.
- **Время — UTC ISO-8601 с миллисекундами** (`2026-11-08T05:20:31.412Z`). Даты (`due_date`) — `YYYY-MM-DD`
  в часовом поясе проекта (`projects.timezone`, по умолчанию `UTC`).

### 5.1. Схема (v1)

**Версия схемы 2** (миграция `0002_search_and_time.sql`, M5) добавляет полнотекстовые индексы
`nodes_fts`/`notes_fts` (FTS5, внешнее содержимое, наполняются триггерами) и представление
`v_node_hours` для учёта времени. Базы v1 достраиваются автоматически при открытии.

Колонка `status` в `CHECK` перечисляет **объединение** статусов всех уровней; какие из них допустимы для
конкретного `kind` и какие переходы разрешены — решает код по матрице §7 и проверяется тестами (CHECK —
страховка от мусора, а не замена правилам).

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
  status             TEXT NOT NULL
                       CHECK (status IN ('planned','todo','in_progress','blocked','review',
                                         'done','cancelled','deferred')),
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
2. `depth = parent.depth + 1`; глубина не ограничена, но `path` ≤ 64 сегментов, длина dot-ID ≤ 255 байт.
3. Узел в `done` не может быть захвачен; для повторного захвата нужен переход в `in_progress` (reopen).
4. `blocks`-ребро не может соединять предка и потомка (тупик по построению) — отказ `ANCESTOR_BLOCK_EDGE`.
5. Ребро `blocks` не создаёт цикл в графе `blocks` — отказ `CYCLE_DETECTED` до записи.
6. Одна незакрытая попытка на узел (гарантируется частичным уникальным индексом + транзакцией).
7. Резерв живёт не дольше своей попытки: `released_at` либо при release, либо при истечении аренды.
8. Каждая мутация пишет строку в `activity_log` в той же транзакции.
9. `description_locked = 1` запрещает менять `description` (остальные поля — свободно), отказ `DESCRIPTION_LOCKED`.
10. Удалять (`node_delete`) можно только узел в `todo`; дальше — есть история, её не затираем.

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

Если проект не определён и ключ не указан — поиск по всем проектам в файле; при совпадении более
одного — ошибка `AMBIGUOUS_ID` со списком кандидатов (не «взять первый»).

### 6.3. Перемещение и перенос

- `node_move` меняет родителя, сохраняя `seq`: dot-ID узла и всех его потомков меняется — это
  документированное следствие, а не сбой. В `activity_log` пишется пара `old_path → new_path` для узла
  и для каждого потомка (одна запись на узел, `action='move'`).
- `--renumber` (необязательный) перенумеровывает сиблингов нового родителя по `sort_order`;
  без флага структура сохраняется.
- Старые dot-ID не резервируются: при создании `seq` никогда не выдаётся заново, но перенесённый узел
  может занять освободившийся путь (он приходит со своим `seq`), поэтому прежний адрес не гарантирует
  прежний узел. Для стабильных внешних ссылок — `#id` или `metadata.external_ref`.

## 7. Статусы и переходы

Наборы по `kind` (совместимы с saga; при импорте `completed → done`, `cancelled → cancelled`):

| kind | допустимые хранимые статусы |
| --- | --- |
| epic | planned, in_progress, blocked, review, done, cancelled, deferred |
| task | todo, in_progress, blocked, review, done, cancelled, deferred |
| subtask, micro | todo, in_progress, blocked, review, done, cancelled, deferred |

Матрица переходов (`→` разрешено; всё прочее — `INVALID_TRANSITION`, без «мягкой» подмены):

| из | в |
| --- | --- |
| (создание) | planned (epic), todo (остальные) |
| planned | in_progress, blocked, cancelled, deferred |
| todo | in_progress, blocked, cancelled, deferred |
| in_progress | review, done, blocked, todo (снять захват), cancelled, deferred |
| review | in_progress, done, blocked, cancelled |
| blocked | todo, in_progress (если блокеры сняты), cancelled |
| deferred | todo, cancelled |
| done | in_progress (reopen), todo (reopen) — требует `force` |
| cancelled | todo (restore) — требует `force` |

Флаги, меняющие строгость (а не подменяющие статус), соответствуют saga:

- `force` — завершить узел с незакрытыми детьми (`CHILDREN_UNFINISHED`) или начать работу при
  невыполненных предусловиях (`UNMET_PREREQUISITES`); факт `force` пишется в лог.
- `description_locked` — защита постановки от перезаписи (`task_lock_description` в saga).

Прочее:

- `in_progress` **не выставляется руками** при живой аренде: захват и есть переход. Матрица выше задаёт,
  какие переходы вообще допустимы, а право выполнить `todo → in_progress` даёт только живая аренда —
  `node_update` без валидного `token` отвергается `LEASE_REQUIRED` (в CLI — `tasq claim`, он и делает
  переход). Это единственное место, где матрица и права расходятся, и расхождение намеренное.
- Пересчёт прогресса вверх по дереву — вычисляемый (`v_node_progress`), кэша нет: дёшево на SQLite,
  зато нет рассинхрона.
- `archived_at` (архив эпика/ветви) скрывает поддерево из выдач, дашборда, поиска и рекомендаций,
  пока не передан `include_archived`. Ничего не удаляется.

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
- Истечение: фоновая уборка **не обязательна**. `v_node_effective` считает аренду истёкшей по времени,
  поэтому захват другим исполнителем проходит сразу. Периодический `tasq doctor --gc` (и опциональный
  `tasq serve --sweep`) закрывает истёкшие попытки с `end_reason='expired'`, чтобы лог и учёт времени
  были упорядочены.

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

### 13.2. Инструменты v1 (43) и бюджет токенов

Бюджет считается по `tools/list` (компактный JSON) в токенах `o200k_base`:

| профиль | инструментов | бюджет (цель) | факт (`tasq tokens`) |
| --- | --- | --- | --- |
| `full` | 45 | ≤ 6 000 | 4 623 |
| `agent` | 29 | ≤ 4 000 | 3 136 |
| `read-only` | 16 | ≤ 2 000 | 1 401 |
| `migration` | 6 | ≤ 800 | 549 |

Факт снят `tasq tokens` на 2026-11-08 с бинаря 0.1.0. Бюджет — гейт: `make tokens` и шаг
«Tool schema budget» в CI падают при перерасходе, поэтому «плюс один инструмент» не проедет
молча. Самые дорогие инструменты — `node_create` (195), `tasq_claim` (184), `node_update` (180):
у них больше всего параметров, и это цена за то, что агент делает всё одним вызовом.

Счёт 45, а не 42 из первичного плана: в M6 добавился `tasq_risk` («узлы, отстающие от своего срока»),
и он вошёл в каталог отдельным инструментом, а не параметром `tasq_dashboard` — у него своя выдача
и свой порог; позже добавились `project_select` и `project_delete` — работа с проектами (см. §15.1).

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

### 13.3. HTTP-транспорт (опция, v1 — выключен)

`tasq serve --http 127.0.0.1:0` — только loopback, эфемерный порт, без аутентификации, с проверкой
`Host`/`Origin` и лимитом тела 1 МиБ. Предназначен для клиентов, которые не умеют stdio; по умолчанию
не поднимается. Печать адреса — в stderr.

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
tasq ls [--status] [--assignee] [--due] [--mine]   список со светофором сроков
tasq board                                          доска по статусам + просрочка + живые аренды
tasq claim <ref> [--holder] [--ttl] [--steal] [--checkpoint]  захват; печатает token
tasq renew <token> [--ttl]                         продление
tasq release <token> [--outcome released|failed|completed] [--checkpoint]
tasq holders                                       живые аренды и остаток TTL
tasq next [--explain]                              рекомендация + причина
tasq ctx <ref>                                     цепочка контекста от корня до узла
tasq dep add|rm|ls|graph <ref> …                   зависимости и их граф
tasq find <запрос>                                 полнотекстовый поиск (FTS5)
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
tasq mcp [--profile]                               MCP-сервер stdio
tasq doctor [--check] [--gc] [--json]             целостность БД, истёкшие аренды, осиротевшие резервы
tasq --version | --help
```

Соглашения: машиночитаемый вывод — `--json` у любой команды; коды выхода — `0` успех, `1` ошибка
предметной области, `2` неверные аргументы, `3` отказ из-за внешнего файла (непарсимый конфиг).
Глобальные флаги (работают с любой командой): `--db PATH` / `TASQ_DB` — файл базы,
`--project KEY` / `TASQ_PROJECT` — проект, в котором идёт работа.
Человекочитаемый вывод CLI — английский (как и описания инструментов): это поверхность, которую читают
и агент, и скрипты; README и документация — русские.

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

- **Проект выбирается явно: `--project KEY` или `TASQ_PROJECT`.** Без него берётся первый по порядку
  создания — так работают существующие базы и скрипты, но второй проект в этом режиме не виден.
- **Флаг важнее переменной окружения.** `--project` перекрывает `TASQ_PROJECT`; явное значение всегда
  сильнее окружения и тем более значения по умолчанию.
- **Ключ нормализуется к верхнему регистру:** `tasq create project prj` создаёт `PRJ`. Ключ входит
  в dot-ID, а он всегда заглавный.
- **Неизвестный ключ — `PROJECT_NOT_FOUND` с перечнем доступных:** `project NOPE not found
  (known: ALPHA, PRJ)`. Молчаливого «взяли проект по умолчанию» нет.
- **Изоляция полная:** `tasq ls` показывает узлы только выбранного проекта, счётчики в
  `list projects` считают живые узлы своего проекта, удалённые не считаются.
- **Переименование ключа меняет все dot-ID проекта разом.** `dot_id` собирается из ключа при
  чтении, поэтому `--new-key BETA` не правит строки узлов: адреса `PRJ-1.2` становятся `BETA-1.2`
  одним обновлением. Команда говорит об этом вслух («all dot-IDs of this project now start with
  BETA-»), потому что внешние ссылки в тексте задач и коммитах остались прежними.
- **Удаление защищено двумя отказами.** Непустой проект не удаляется без `--force` (`PROJECT_NOT_EMPTY`
  с числом узлов), последний проект в базе — тоже (`LAST_PROJECT`): пустая база делает все команды
  бесполезными. Оба отказа советуют выход: архивировать (`--status archived`) или удалить сам файл базы.
- **Удаление каскадное и необратимое:** узлы, рёбра, заметки, комментарии, попытки, резервы и лог —
  всё уходит вместе с проектом по `ON DELETE CASCADE` схемы. `doctor --check` после удаления чист.
- **В MCP те же операции с оговорками про контекст:** `project_update` правит проект сессии (или
  указанный `key`) и, если ключ сменился, переводит контекст сессии на новый ключ — иначе следующий
  вызов ушёл бы в первый проект по порядку. `project_delete` при удалении проекта сессии переезжает
  в первый оставшийся и сообщает `switched_to`; если проектов не осталось, возвращает
  `active_project: null` и подсказку. `project_delete` входит только в профиль `full`.
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
- Экспорт попыток и аренд в файл не входит: это состояние живой машины.

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

- `PRAGMA journal_mode=WAL; synchronous=NORMAL; foreign_keys=ON; busy_timeout=5000`.
- Любая мутация — одна транзакция с записью в лог; частичных состояний не бывает.
- Ошибки — типизированные коды (§19), а не строки; `--json` отдаёт `{"error":{"code":…,"message":…,"hint":…}}`.
- Fail-fast: неизвестный `kind`, статус, профиль, вид ресурса, конфликтная конфигурация — ошибка сразу,
  без «мягкой» подстановки значения по умолчанию.
- Конкурентность проверяется тестами: N процессов одновременно захватывают один узел, побеждает ровно
  один; `kill -9` держателя не оставляет узел в `in_progress` навсегда.

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
  (`SCHEMA_TOO_NEW`).

## 18. Коды ошибок предметной области

`NODE_NOT_FOUND`, `AMBIGUOUS_ID`, `NODE_CLOSED`, `INVALID_TRANSITION`, `CHILDREN_UNFINISHED`,
`UNMET_PREREQUISITES`, `DESCRIPTION_LOCKED`, `ACTIVE_ATTEMPT_EXISTS`, `LEASE_EXPIRED`,
`NOT_YOUR_ATTEMPT`, `LEASE_REQUIRED`, `RESOURCE_RESERVATION_CONFLICT`, `TOO_MANY_RESERVATIONS`,
`PATH_OUTSIDE_PROJECT`, `CYCLE_DETECTED`, `ANCESTOR_BLOCK_EDGE`, `SELF_EDGE`, `CROSS_PROJECT_EDGE`,
`DUPLICATE_KEY`, `TOOL_NOT_IN_PROFILE`, `SCHEMA_TOO_NEW`, `EXPORT_CHECKSUM_MISMATCH`, `CONFIG_UNPARSABLE`,
`BRANCH_UNAVAILABLE`.

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

## 20. Этапы

| этап | объём | признак завершения |
| --- | --- | --- |
| M0 | каркас: cargo, `lib.rs`/`main.rs`, миграции, `tasq init`, `doctor --check` | `tasq init` создаёт БД, `doctor` чист |
| M1 | узлы и дерево: CRUD, dot-ID, матрица переходов, лог активности, архивация | сценарий «проект → эпик → задача → 3 подзадачи» целиком через CLI |
| M2 | аренды: claim/renew/release/steal, производный статус, чекпойнт, gc | тесты конкурентности и `kill -9` зелёные |
| M3 | резервы: path/glob/logical, конфликты, `holdings` | таблица конфликтов проходит целиком |
| M4 | зависимости: рёбра, циклы, сквозные связи, `graph`, автоснятие блокировки | property-тест циклов + `tasq_next` объясняет блокеры |
| M5 | остальное из saga: шаблоны, заметки, комментарии, поиск, `context`, `dashboard`, `next`, batch, `source_ref`, учёт времени | каждая функция saga имеет рабочий аналог (§13.2) |
| M6 | сроки и burn-up: `due_date`, просрочка, снимки, ASCII-график, прогноз | ряд совпадает с восстановленным из лога |
| M7 | импорт/экспорт, совместимость с saga | round-trip + импорт реального экспорта saga |
| M8 | MCP: сервер, профили, бюджет `tasq tokens`, `connect` | рукопожатие через pipe; `connect --dry-run` для 11 клиентов; бюджет в гейте |
| M9 | укрепление: fuzz, `cargo-deny`, покрытие, документация `docs/` | все ворота §19 зелёные |

## 21. Критерии приёмки v1

1. `tasq init && tasq add … && tasq board` работает на пустой машине без сети и без Docker.
2. Dot-ID вида `TASQ-12.3.1` стабильны при работе и документированно меняются только при переносе.
3. Два агента не могут одновременно держать один узел; умерший агент не блокирует работу дольше своего TTL.
4. Конфликт резервов обнаруживается до начала работы и называет держателя и срок аренды.
5. Зависимость «задача в одной ветви → подзадача в другой» создаётся, а цикл — отвергается.
6. `tasq_next` объясняет выбор словами: приоритет, срок, что разблокирует.
7. `tasq burnup --ascii` показывает объём, готовое и линию цели; данные совпадают с логом.
8. `tasq tokens --max 6000` проходит на профиле `full`, а `read-only` укладывается в 1 600.
9. `tasq connect --dry-run` показывает корректный фрагмент для установленных клиентов; запись
   идемпотентна и не ломает существующие ключи.
10. Каждая функция saga из §13.2 имеет рабочий аналог (таблица соответствия — `docs/saga-parity.md`).
11. Экспорт → импорт в чистую БД даёт идентичный проект (dot-ID, рёбра, комментарии, заметки, шаблоны).
12. `cargo fmt`/`clippy -D warnings`/`test`/`deny`/`tokens` зелёные на чистом клоне.

## 22. Сборка, гейты и CI

Принцип: **на хосте rust-тулчейна нет, всё собирается в контейнере на базе `rust:1.99.1-bookworm`**,
и локальные гейты, `act` и GitHub Actions используют один и тот же образ. Расхождение возможно только
в версии rustfmt/clippy, поэтому они ставятся явно и там, и там.

- `Dockerfile` — `FROM rust:1.99.1-bookworm` + `rustup component add rustfmt clippy`: официальный образ
  этих компонентов не несёт, и без шага `cargo fmt --check` падает с «cargo-fmt is not installed».
- `make` (по умолчанию) — `fmt` + `clippy` + `deny` + `test` + `tokens`; `make build` — release-бинарь;
  `make smoke` — `init` + `doctor` во временном каталоге внутри контейнера; `make act` — прогон
  workflow на хосте.
- `cargo-deny` ставится готовым musl-бинарём (0.20.2) одним скриптом `scripts/install-cargo-deny.sh`
  и в `Dockerfile`, и шагом CI: две копии логики установки разъехались бы, и «проходит локально»
  перестало бы что-либо значить. Скрипт сверяет `sha256` скачанного архива.
- `deny.toml`: `[advisories]` — любая уязвимость и любой yank роняют сборку, `ignore = []`;
  `[licenses]` — только реально встречающиеся в дереве лицензии (MIT, Apache-2.0, Unicode-3.0,
  Unlicense, Zlib), снятые `cargo deny list`, а не список «на будущее»: новая лицензия — повод
  посмотреть на зависимость. `[sources]` разрешает только crates.io.
- Кэш cargo/target — в `~/.cache/tasq/{cargo,target,home}`, вне репозитория; каталоги создаются от
  хостового uid (`cache-init`), иначе docker создаёт их root-овыми и следующий прогон падает
  `Permission denied`.
- `.github/workflows/ci.yml` — job `check` с `container: rust:1.99.1-bookworm`; шаги: компоненты
  (rustfmt, clippy) → `cargo-deny` → `fmt --check` → `clippy -D warnings` → `cargo deny check` →
  `test` → пол покрытия → `build --release` → smoke (`init` + `doctor --json` с проверкой
  `"ok": true`) → бюджет схем. Тот же файл исполняет `act` с хоста.
- Покрытие (`make cov`, шаг «Coverage floor»): `cargo-llvm-cov` с `--fail-under-lines 78`.
  Порог — **пол, а не цель**: он не мешает добавлять код, но не даёт выпустить «написали и не
  проверили». После тестов каталога и property-тестов — 81,0 % строк и 83,2 % регионов; слабее всего `main.rs`
  (60 %) — это разбор аргументов CLI, он проверяется через сами команды, а не через вызовы
  функций. `cargo-llvm-cov` ставится скриптом
  `scripts/install-cargo-llvm-cov.sh` с закреплённым хешем (у проекта нет файла контрольных сумм,
  поэтому хеш для x86_64 записан в скрипте, а для aarch64 установка честно отказывается).
  `cov` не входит в `make all`: инструментированная сборка пересобирает всё дерево.
- Подводный камень `act`: шаги он запускает через `sh -e` (это dash), поэтому `set -euo pipefail` падает
  `Illegal option -o pipefail`. У шага smoke явно указан `shell: bash` — на GitHub Actions поведение
  не меняется, в `act` работает.
- Готовый бинарь требует glibc ≥ 2.34 (фактический максимум символов — `GLIBC_2.34`, база bookworm 2.36);
  для хостов старше Debian 12 / Astra надо пересобрать на более старой базе и проверить
  `objdump -T | grep GLIBC_`.

## 23. Документация (`docs/`)

Справочники не пишутся руками: `tasq docs` снимает их с самого бинаря и с пустой базы,
созданной текущими миграциями.

| файл | источник | что внутри |
| --- | --- | --- |
| `docs/cli.md` | описание CLI (clap) | все команды, их флаги, значения по умолчанию и описания |
| `docs/mcp-tools.md` | каталог `tools/list` | 43 инструмента, доступность по профилям, стоимость в токенах, обязательные аргументы |
| `docs/schema.md` | свежая база | фактические `CREATE TABLE`/`CREATE VIEW`/`CREATE INDEX` и версия схемы |

Гейт: `tasq docs --check` сравнивает файлы на диске с тем, что получилось бы сейчас, и возвращает
код 3 при расхождении. Он стоит в `make docs` (входит в `make all`) и шагом «Reference docs are
up to date» в CI. Смысл в том, что справочник команд, написанный отдельно от кода, расходится
с ним на первой же правке флага, и расхождение замечает не автор, а тот, кто по нему работает.

`docs/saga-parity.md` — исключение: это не справочник, а разовый разбор API `saga-mcp@1.19.0`,
он написан руками и генерировать его не из чего.

## 24. Состояние реализации

Отметки ставятся по факту прогона, а не по плану; ворота — `make all` и `act push -j check`.

| этап | состояние | что проверено |
| --- | --- | --- |
| M0 каркас | готово | `init` идемпотентен, `doctor` чист, схема v2 создаётся миграциями, представления исполняются на данных |
| M1 узлы и дерево | готово | dot-ID `TASQ-1.1.2`, `seq` не переиспользуется, три формы ссылки, матрица переходов с `force`, гварды `CHILDREN_UNFINISHED`/`LEASE_REQUIRED`, блокировка описания, перенос с пересчётом путей потомков, архив поддерева, мягкое удаление и восстановление, запись в лог на каждую мутацию |
| M2 аренды | готово | захват атомарен (8 процессов, 10 кругов — ровно один победитель), второй захват получает `ACTIVE_ATTEMPT_EXISTS` с держателем и сроком, TTL в границах 60…28800, продление и отказ после истечения, чекпойнт наследуется при следующем захвате, просроченная аренда не блокирует, `--steal` для зависшего держателя, `doctor --gc` закрывает истёкшие и возвращает узел в `todo` |
| M3 резервы | готово | путь сравнивается по границе сегмента (`src/api` задевает `src/api/x.rs`, но не `src/apix`), glob — консервативно по литеральным префиксам, `logical` — по точному равенству; `shared` конфликтует только с `exclusive`; резервы заявляются атомарно с арендой, снимаются при release, истечении и `--gc`; чужой резерв называет держателя и срок; выход за корень проекта — `PATH_OUTSIDE_PROJECT`; пересечения внутри одной заявки разрешены |
| M4 зависимости | готово | ребро между любыми уровнями и ветвями, пять видов, идемпотентная вставка, `SELF_EDGE`/`ANCESTOR_BLOCK_EDGE`/`CYCLE_DETECTED` с путём круга, снятие последнего блокера возвращает узел в `todo`, `dep graph` показывает «ждёт / разблокирует / транзитивно / цепочка» и Mermaid, `next` объясняет выбор и не предлагает контейнеры и заблокированные узлы |
| M5 остальное из saga | готово | заметки 8 типов с привязкой к узлу, комментарии с мягким удалением и причиной, шаблоны-деревья с `{переменными}` (без значений — отказ до первой записи), полнотекстовый поиск FTS5 по узлам и заметкам с индексом на триггерах, `ctx` (цепочка от корня), `board` (просрочка и живые аренды), `time` (оценка против факта из попыток), `bulk set` с отчётом по каждому узлу |
| M6 сроки и burn-up | готово | ряд восстанавливается из лога (создание/готово/отмена) и совпадает со снимками день в день, три метрики (штуки, очки, часы), ASCII-график с линией цели, прогноз по скользящему окну 7 дней помечен как прогноз, `risk` показывает отставание «прошло времени против сделано» с порогом 0.2 |
| M7 импорт/экспорт | готово | детерминированный экспорт (сумма по `data`, а не по моменту выгрузки), проверка `sha256` при импорте, `merge`/`replace`, полная валидация до записи (ссылки, дубли путей, циклы `blocks`), `--dry-run`; импорт saga `format_version` 1.0…1.3 на реальной фикстуре: статусы, приоритеты, блокировка описания, зависимости, комментарии, заметки, `metadata.saga_id` |
| M8 MCP и `connect` | готово | рукопожатие и `tools/call` через stdin/stdout настоящего бинаря; `connect` для 11 клиентов: JSON и TOML правятся атомарно с копией и идемпотентно, YAML и файлы с комментариями только печатаются (код 3), HTTP — лишь на петле | профили: full 4 391 / agent 3 010 / read-only 1 401 / migration 549 токенов, все в бюджете; `tasq tokens --max 100` роняет гейт; `connect --check` различает «прописан» и «нет» |
| M10 проекты | готово | `create project` / `list projects` / `update project` / `delete project`; селектор `--project`/`TASQ_PROJECT` (флаг важнее env, env важнее порядка создания); `PROJECT_NOT_FOUND` с перечнем ключей; ключ нормализуется к верхнему регистру; изоляция узлов и счётчиков проверена; в MCP `project_create` не переключает сессию (`switched: false` + подсказка), `project_select` переключает и возвращает `previous`, сервер стартует в проекте через `--project`; переименование ключа двигает все dot-ID разом, удаление непустого и последнего проекта требует `--force` | 19 тестов CLI и 6 MCP: изоляция проектов, env и флаг, дубликат и невалидный ключ, счётчики, перенос адресов при смене ключа, отказ на чужой ключ, каскад при удалении, `doctor` после удаления |
| M9 укрепление | готово | `cargo deny check` — advisories/bans/licenses/sources ok; 14 property-тестов (нашли дефект нормализации `./`) и 9 тестов каталога (нашли `TASQ--` в ответе `template_apply` и молчаливый `remove: true` в `tasq_reserve` — оба исправлены); покрытие 81,0 % строк при поле 78 %; `tasq docs --check` ловит ручную правку справочника |

Тесты: 234 (32 модульных, 19 проектов, 18 `connect`, 17 каталога MCP, 16 по резервам, 15 CLI,
14 по арендам, 14 по переносу данных, 14 по сгоранию и Unicode, 14 property, 13 по памяти и обзору,
12 по дереву, 11 по зависимостям, 11 MCP, 6 схемных, 5 бюджета, 5 документации, 3 про путь к базе). Проверки по ветке (`branch`) ещё не подключены: поле в схеме есть, чтение
`git rev-parse` появится отдельной задачей.

## 25. Решения по открытым вопросам

Вопросы, стоявшие до M1, закрыты реализацией; здесь зафиксировано, чем именно, и что осталось.

| вопрос | решение | где видно |
| --- | --- | --- |
| идентификатор исполнителя | строка `holder` от клиента, без привязки к проекту: один агент может работать в нескольких | `attempts.holder`, `claim --holder`, `tasq_claim` |
| дедлайн эпика против дедлайна задачи | хранятся отдельно, эпик свой срок не агрегирует: `min(сроки детей)` скрыл бы отставание детей | `nodes.due_date`, `tasq risk` |
| `shared`-резервы | оставлены: без них нельзя заявить «читаю, но не пишу»; конфликт `shared` только с `exclusive` | `Mode::Shared`, `tests/resource.rs` |
| часы против очков | `weight` по умолчанию `1`, метрика burn-up выбирается флагом, а не настройкой проекта | `burnup --metric count\|points\|hours` |
| публикация | публичный репозиторий GitHub, MIT; в crates.io — отдельным решением | `LICENSE`, `Cargo.toml` |
| TUI | не входит в v1: `tasq board` в терминале закрывает ежедневный сценарий | `board` |

Открытый пункт один: **HTTP-транспорт MCP-сервера не реализован**, сервер работает только по stdio.
`tasq connect --url` пишет адрес, который должен обслуживать внешний прокси (например `mcp-proxy`);
сам `tasq` порт не слушает. Пока это не сделано, `--url` — способ подключить прокси, а не встроенный
HTTP-режим, и README не должен создавать обратного впечатления.
