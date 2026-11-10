-- tasq schema v8: цикл в дереве не вешает чтение
--
-- Найдено тестом на защиту переноса. Сама защита от переноса «в себя» работала, но испорченная
-- база (цикл записан руками, другой версией бинаря или досталась от прежнего дефекта) вешала
-- ЧТЕНИЕ: `v_node_progress` рекурсивно обходит дерево вверх, и на цикле обход не кончается.
-- Проверено напрямую: `select count(*) from v_node_progress` на базе с циклом не завершается
-- (таймаут 20 с, rc=124). Значит `tasq show` и `ls` на такой базе виснут — и это хуже, чем
-- неверный ответ: база кажется рабочей, пока в неё не заглянешь.
--
-- Исправление — сторож завершаемости в самом представлении: обход ограничен числом шагов.
-- Это не проверка входа (её роль играет `DEPTH_LIMIT` при записи), а защита от чужой ошибки:
-- представление обязано завершаться на любых данных, включая испорченные.
--
-- Предел — 64 шага, а не 16 (`node::MAX_DEPTH`): базы, созданные до v4, могли иметь деревья
-- глубже нынешнего предела, и обрезать их прогресс на 16 значило бы соврать в отчёте. 64 —
-- столько, сколько допускала прежняя схема.

DROP VIEW v_node_progress;

CREATE VIEW v_node_progress AS
WITH RECURSIVE tree(ancestor_id, node_id, steps) AS (
  SELECT id, id, 0 FROM nodes
  UNION ALL
  SELECT t.ancestor_id, n.id, t.steps + 1
  FROM tree t
  JOIN nodes n ON n.parent_id = t.node_id
  -- Сторож: на цикле или на дереве глубже предела обход прекращается, а не идёт по кругу.
  WHERE t.steps < 64
)
SELECT
  t.ancestor_id                                                       AS node_id,
  COUNT(*)                                                            AS total_count,
  SUM(CASE WHEN n.status = 'done' THEN 1 ELSE 0 END)                  AS done_count,
  SUM(n.weight)                                                       AS total_weight,
  SUM(CASE WHEN n.status = 'done' THEN n.weight ELSE 0 END)           AS done_weight
FROM tree t
JOIN nodes n ON n.id = t.node_id AND n.deleted_at IS NULL
GROUP BY t.ancestor_id;
