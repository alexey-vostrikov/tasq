# Паритет с saga-mcp

Источник: npm-пакет `saga-mcp@1.19.0` (`dist/tools/*.js`), 41 инструмент; данные модели — из `schema.js`
того же пакета. Проверено 2026-11-08. Таблица — обязательство: каждый пункт слева имеет рабочий аналог
справа, иначе критерий приёмки №10 не выполнен.

## Инструменты (41)

| # | saga-mcp | tasq | как именно |
| --- | --- | --- | --- |
| 1 | `activity_log` | `activity_log` | тот же смысл + фильтры по узлу и актору |
| 2 | `tracker_session_diff` | `session_diff` | агрегат по действиям и сущностям с отметки времени |
| 3 | `task_batch_update` | `node_batch_update` | пакет по dot-ID; одна транзакция |
| 4 | `comment_add` | `comment_add` | автор = `holder` попытки или `-` |
| 5 | `comment_list` | `comment_list` | `include_deleted` показывает удалённые с причиной |
| 6 | `comment_delete` | `comment_delete` (`op=delete`) | мягкое удаление, строка остаётся |
| 7 | `comment_restore` | `comment_delete` (`op=restore`) | обратная операция того же инструмента |
| 8 | `tracker_dashboard` | `tasq_dashboard` | + живые аренды, просрочка, risk-флаг |
| 9 | `tracker_init` | `tasq_init` | создаёт проект, если БД пуста |
| 10 | `epic_create` | `node_create` (`kind=epic`) | отдельного инструмента на уровень нет |
| 11 | `epic_list` | `node_list` (`depth=1`) | фильтры статус/приоритет/ветка/архив |
| 12 | `epic_update` | `node_update` | только переданные поля |
| 13 | `epic_archive` | `node_archive` | архив поддерева, ничего не удаляется |
| 14 | `tracker_export` | `tasq_export` | + `sha256` канонического JSON |
| 15 | `tracker_import` | `tasq_import` | `merge`/`replace`, одна транзакция |
| 16 | `tracker_next` | `tasq_next` | + причина выбора и «что разблокирует» |
| 17 | `note_save` | `note_save` | upsert по `id`, 8 типов заметок |
| 18 | `note_list` | `note_list` | фильтры по типу/узлу/проекту |
| 19 | `note_search` | `note_search` | FTS5, префиксный поиск |
| 20 | `note_delete` | `note_delete` | удаление заметки (не узла) |
| 21 | `project_create` | `project_create` | + обязательный уникальный `key` для dot-ID |
| 22 | `project_list` | `project_list` | + счётчики просрочки и аренд |
| 23 | `project_update` | `project_update` | `key` неизменяем |
| 24 | `tracker_search` | `tasq_search` | по проектам, узлам, комментариям, заметкам |
| 25 | `subtask_create` | `node_create` (`kind=subtask`) | массив заголовков = пакетное создание |
| 26 | `subtask_update` | `node_update` + `tasq_link` | `depends_on`/`blocks` вынесены в рёбра |
| 27 | `subtask_reorder` | `node_reorder` | порядок сиблингов, пропущенные — в конец |
| 28 | `subtask_delete` | `node_delete` | только `todo`, мягкое удаление |
| 29 | `task_create` | `node_create` (`kind=task`) | внутри эпика |
| 30 | `task_list` | `node_list` | без родителя — по всем ветвям |
| 31 | `task_reorder` | `node_reorder` | порядок по умолчанию для выдач |
| 32 | `task_delete` | `node_delete` | мягкое, `task_restore` → `node_restore` |
| 33 | `task_restore` | `node_restore` | возвращает из мягкого удаления |
| 34 | `task_lock_description` | `node_update` (`description_locked`) | защита постановки от перезаписи |
| 35 | `task_get` | `node_get` | + дерево, рёбра, попытки, заметки |
| 36 | `task_update` | `node_update` | матрица переходов, `force` для гвардов |
| 37 | `template_create` | `template_save` | upsert по имени |
| 38 | `template_list` | `template_list` | `include_tasks` показывает дерево |
| 39 | `template_apply` | `template_apply` | подстановка `{переменных}` |
| 40 | `template_update` | `template_save` | тот же upsert |
| 41 | `template_delete` | `template_delete` | удаление шаблона |

## Функции saga, не выраженные отдельным инструментом

| функция saga | где в tasq |
| --- | --- |
| иерархия project → epic → task → subtask | `nodes` с `depth`/`kind`, предел 16 уровней |
| статусы по уровням (project/epic/task/subtask) | единый набор + матрица переходов по `kind` |
| приоритеты low…critical | `nodes.priority` |
| `tags` и `metadata` (JSON) на всех уровнях | те же колонки |
| `branch` + `branch="current"` | `nodes.branch`, `git rev-parse`, ошибка `BRANCH_UNAVAILABLE` |
| зависимости задач (junction-таблица) | `edges` с `kind` и проверкой циклов |
| зависимости подзадач | те же `edges` между узлами любой глубины |
| мягкое удаление комментариев с причиной | `comments.is_deleted/deleted_by/delete_reason` |
| архив эпика со скрытием из выдач | `nodes.archived_at`, `include_archived` |
| шаблоны `{variable}` | `templates.tree` + подстановка при `template_apply` |
| 8 типов заметок | `notes.note_type` (те же значения) |
| автоматический лог мутаций (old/new) | `activity_log` в той же транзакции |
| `source_ref` — ссылка на код | `nodes.source_ref` |
| авторасчёт часов из активности | `v_attempt_hours` + `attempts` |
| чекпойнт сессии через комментарии | `attempts.checkpoint` (машинный, не текстовый) |
| аннотации MCP (`readOnlyHint` и др.) | на каждом инструменте, §13.1 |
| правило «удалять только `todo`» | инвариант 10, §5.3 |
| гвард завершения при незакрытых детях | `CHILDREN_UNFINISHED` + `force` |
| завершение задачи при незакрытых подзадачах | `CHILDREN_UNFINISHED` (дети любого уровня) |

## Что добавлено сверх saga

- dot-notation ID как адрес узла (`TASQ-12.3.1`) и «цепочка контекста» для агента (`tasq_context`).
- Аренды с истечением, производный `in_progress`, чекпойнт и перехват просроченной аренды.
- Резервирование ресурсов (файл, каталог, glob, логический ключ) с обнаружением конфликтов до начала работы.
- Зависимости сквозь границы вложенности + детект циклов + запрет ребра «предок → потомок».
- Пересчёт прогресса вверх по дереву (`v_node_progress`).
- Дедлайны с просрочкой, `at_risk`, burn-up с прогнозом и «расползанием объёма».
- Профили инструментов (`full`/`agent`/`read-only`/`migration`) и измеряемый бюджет схем в токенах.
- `tasq connect` — прописка MCP-конфига в клиентов с атомарной записью и бэкапом.
- `tasq doctor` — целостность БД, уборка истёкших аренд и осиротевших резервов.
- Контрольная сумма экспорта и round-trip импорта без потерь.
