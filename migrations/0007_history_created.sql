-- tasq schema v7: причина `created` и первый факт истории на создание узла
--
-- Почему это нужно. История пишется на ПЕРЕХОДЫ статуса, а создание узла переходом не является:
-- узел появляется уже в начальном статусе. Из-за этого у любого узла, созданного после v4 и ни
-- разу не менявшего статус, истории нет вовсе — а burn-up, различающий «не закрыто» и
-- «истории нет», честно называл такие узлы неизвестными. На живой базе это дало «unknown for
-- 10 of 34» при том, что про эти узлы известно всё: их создали, они лежат в `idea`.
--
-- Два следствия, оба исправляются здесь:
--   1. Создание узла пишет первый факт: `NULL → начальный статус`, причина `created`.
--   2. Узлы, созданные раньше, получают тот же факт задним числом — но ТОЛЬКО нетерминальные.
--      Для закрытых (`done`, `cancelled`) без истории дата закрытия неизвестна, и подставить
--      вместо неё дату создания значило бы соврать в ряду burn-up. Такие узлы остаются
--      «неизвестными», и отчёт говорит это словами.
--
-- Список причин — снова `CHECK`, поэтому снова пересборка: таблица молодая, это дешево.

CREATE TABLE node_status_history_v7 (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  from_status TEXT REFERENCES statuses(name),
  to_status   TEXT NOT NULL REFERENCES statuses(name),
  cause       TEXT NOT NULL CHECK (cause IN ('created','manual','lease_claimed','lease_completed',
                                             'lease_released','lease_expired','unblock',
                                             'gc','import','restore')),
  at          TEXT NOT NULL,
  actor       TEXT
);

INSERT INTO node_status_history_v7 (id, node_id, from_status, to_status, cause, at, actor)
SELECT id, node_id, from_status, to_status, cause, at, actor
FROM node_status_history;

DROP TABLE node_status_history;

ALTER TABLE node_status_history_v7 RENAME TO node_status_history;

CREATE INDEX idx_status_history_node ON node_status_history(node_id, at);
CREATE INDEX idx_status_history_to ON node_status_history(to_status, at);

-- Первый факт задним числом: «узел существует с этого момента и находится в этом статусе».
-- Временем берётся создание узла — единственное, что про него точно известно.
INSERT INTO node_status_history (node_id, from_status, to_status, cause, at, actor)
SELECT n.id, NULL, n.status, 'created', n.created_at, '-'
FROM nodes n
WHERE NOT EXISTS (SELECT 1 FROM node_status_history h WHERE h.node_id = n.id)
  AND n.status NOT IN ('done', 'cancelled');
