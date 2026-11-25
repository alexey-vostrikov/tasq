-- tasq schema v11: статус `incomplete` — закрыто, но не выполнено
--
-- Зачем. До сих пор попытка закрыть непустой узел имела два исхода, и оба плохи:
--
--   * отказ `CHILDREN_UNFINISHED` — человек не может закрыть эпик, работу по которому решил
--     прекратить: он остаётся в очереди навсегда и висит в `next`;
--   * `--force` — запись `done` при четырёх открытых задачах, то есть ложь в отчётах: burn-up
--     считает эпик выполненным, `ls -n` его прячет, доска показывает его закрытым наравне с
--     действительно законченным.
--
-- `incomplete` разводит эти два смысла: работа по узлу прекращена (закрыт), но не доведена
-- (не выполнен). Пользователь видит ровно это, отчёты перестают завышать результат.
--
-- Что меняется.
--
--   1. Справочник `statuses` получает строку `incomplete` с `rank = 11` (после `cancelled`):
--      порядок существующих статусов не сдвигается, чужие дампы и старые номера воронки
--      остаются валидными.
--   2. Появляется колонка `counts_done`. До неё «закрытость» и «выполненность» были одним и тем
--      же (`status IN ('done','cancelled')`), и именно поэтому нового статуса нельзя было
--      добавить, не переписав каждый отчёт. Теперь это два разных вопроса: `terminal`
--      отвечает «работа прекращена», `counts_done` — «работа сделана». У `done` 1/1,
--      у `cancelled` 1/0, у `incomplete` 1/0.
--   3. Четыре представления перестают держать литералы `'done','cancelled'` и читают оба
--      признака из справочника. Это не косметика: пока закрытость была литералом в SQL и в 19
--      запросах кода, каждый новый статус приходилось вписывать в каждое место и одно
--      забывать — так уже случилось с `import` (он выпал из burn-up) и с проверкой блокеров.
--
-- Отдельного внимания заслуживает `v_node_effective`: закрытый узел обязан остаться собой,
-- даже если у него открытые блокирующие рёбра. Эпик, прекращённый с хвостами, не «заблокирован»:
-- он закрыт, и показать его в колонке `blocked` значило бы вернуть его в работу на доске.

-- ---------------------------------------------------------------------------
-- 1. Справочник: признак выполненности и новый статус
-- ---------------------------------------------------------------------------
ALTER TABLE statuses ADD COLUMN counts_done INTEGER NOT NULL DEFAULT 0
  CHECK (counts_done IN (0, 1));

UPDATE statuses SET counts_done = CASE WHEN name = 'done' THEN 1 ELSE 0 END;

-- `rank = 11`, а не 9: `incomplete` — не ступень воронки, а исход, и он стоит рядом с другими
-- исходами. Сдвигать `done` и `cancelled` значило бы менять смысл чисел, которые уже лежат в
-- базах и в дампах.
INSERT INTO statuses (name, rank, terminal, counts_done) VALUES ('incomplete', 11, 1, 0);

-- ---------------------------------------------------------------------------
-- 2. Представления: закрытость и выполненность из справочника
-- ---------------------------------------------------------------------------
-- `v_node_effective` — единственное место, где решается «что с узлом сейчас» (v5). Закрытость
-- берётся из справочника; остальная логика (аренда, блокеры, возврат в очередь) не меняется.
DROP VIEW v_node_effective;

CREATE VIEW v_node_effective AS
SELECT
  n.*,
  CASE
    WHEN EXISTS (
      SELECT 1 FROM statuses s WHERE s.name = n.status AND s.terminal = 1
    ) THEN n.status
    -- Живая аренда: работа идёт прямо сейчас.
    WHEN EXISTS (
      SELECT 1 FROM attempts a
      WHERE a.node_id = n.id AND a.ended_at IS NULL
        AND a.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
    ) THEN 'in_progress'
    -- Аренда истекла, но попытку никто не закрыл: работа не идёт. Узел возвращается в очередь
    -- в тот статус, который допустим для его вида.
    WHEN n.status = 'in_progress' THEN
      CASE WHEN n.kind = 'epic' THEN 'planned' ELSE 'todo' END
    WHEN n.status = 'blocked' OR EXISTS (
      SELECT 1 FROM edges e
      JOIN nodes b ON b.id = e.to_node
      WHERE e.from_node = n.id AND e.kind = 'blocks'
        AND b.deleted_at IS NULL
        AND NOT EXISTS (SELECT 1 FROM statuses s WHERE s.name = b.status AND s.terminal = 1)
    ) THEN 'blocked'
    ELSE n.status
  END AS effective_status,
  -- Только живая попытка: истёкшая аренда для читателя не существует.
  (SELECT a.expires_at FROM attempts a
    WHERE a.node_id = n.id AND a.ended_at IS NULL
      AND a.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
    ORDER BY a.id DESC LIMIT 1) AS lease_expires_at,
  (SELECT a.holder FROM attempts a
    WHERE a.node_id = n.id AND a.ended_at IS NULL
      AND a.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
    ORDER BY a.id DESC LIMIT 1) AS lease_holder
FROM nodes n;

DROP VIEW v_blockers;

CREATE VIEW v_blockers AS
SELECT
  e.from_node                    AS node_id,
  COUNT(*)                       AS blocker_count,
  group_concat(b.path, ',')      AS blocker_paths
FROM edges e
JOIN nodes b ON b.id = e.to_node
WHERE e.kind = 'blocks'
  AND b.deleted_at IS NULL
  AND NOT EXISTS (SELECT 1 FROM statuses s WHERE s.name = b.status AND s.terminal = 1)
GROUP BY e.from_node;

DROP VIEW v_overdue;

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
  AND NOT EXISTS (SELECT 1 FROM statuses s WHERE s.name = n.status AND s.terminal = 1);

-- Прогресс: доля ВЫПОЛНЕННОГО, а не закрытого. `incomplete` закрыт, но в числитель не идёт —
-- иначе эпик, прекращённый с четырьмя открытыми задачами, показывал бы 100 %.
DROP VIEW v_node_progress;

CREATE VIEW v_node_progress AS
WITH RECURSIVE tree(ancestor_id, node_id, steps) AS (
  SELECT id, id, 0 FROM nodes
  UNION ALL
  SELECT t.ancestor_id, n.id, t.steps + 1
  FROM tree t
  JOIN nodes n ON n.parent_id = t.node_id
  -- Сторож завершаемости и предел глубины — одно число (`__MAX_DEPTH__`, как в v10).
  WHERE t.steps < __MAX_DEPTH__
)
SELECT
  t.ancestor_id                                                       AS node_id,
  COUNT(*)                                                            AS total_count,
  SUM(CASE WHEN s.counts_done = 1 THEN 1 ELSE 0 END)                  AS done_count,
  SUM(n.weight)                                                       AS total_weight,
  SUM(CASE WHEN s.counts_done = 1 THEN n.weight ELSE 0 END)           AS done_weight
FROM tree t
JOIN nodes n ON n.id = t.node_id AND n.deleted_at IS NULL
JOIN statuses s ON s.name = n.status
GROUP BY t.ancestor_id;
