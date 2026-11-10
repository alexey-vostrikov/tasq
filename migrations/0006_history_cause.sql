-- tasq schema v6: причина `lease_expired` в истории статусов
--
-- Список причин в `node_status_history.cause` — тоже `CHECK`, поэтому тоже неизменяем.
-- Добавить `lease_expired` к восьми существующим можно только пересборкой таблицы; таблица
-- новая и маленькая, пересборка дешёвая.
--
-- Зачем отдельная причина, а не переиспользование `lease_released`: истёкшая аренда — не
-- решение исполнителя и не доведённая работа. Для отчётов это разница между «сделал и
-- отпустил» и «не успел»; склеивать их значило бы врать в обе стороны.
--
-- Внешние ключи на время пересборки выключает раннер (`apply_migrations`), как и в v3/v4:
-- иначе `DROP TABLE` выполнит неявное DELETE и каскадом снесёт строки истории.

CREATE TABLE node_status_history_v6 (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  from_status TEXT REFERENCES statuses(name),
  to_status   TEXT NOT NULL REFERENCES statuses(name),
  cause       TEXT NOT NULL CHECK (cause IN ('manual','lease_claimed','lease_completed',
                                             'lease_released','lease_expired','unblock',
                                             'gc','import','restore')),
  at          TEXT NOT NULL,
  actor       TEXT
);

INSERT INTO node_status_history_v6 (id, node_id, from_status, to_status, cause, at, actor)
SELECT id, node_id, from_status, to_status, cause, at, actor
FROM node_status_history;

DROP TABLE node_status_history;

ALTER TABLE node_status_history_v6 RENAME TO node_status_history;

CREATE INDEX idx_status_history_node ON node_status_history(node_id, at);
-- Ответ на вопрос «когда узел стал done» — основной запрос burn-up, поэтому индекс по паре
-- статус+время, а не только по узлу.
CREATE INDEX idx_status_history_to ON node_status_history(to_status, at);
