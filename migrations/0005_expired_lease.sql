-- tasq schema v5: истёкшая аренда перестаёт держать узел
--
-- Что было не так. Аренда живёт до `expires_at`, и это обещано в README §8: умерший агент не
-- держит узел дольше своего TTL. Обещание выполнялось наполовину:
--
--   * `effective_status` срок проверял и честно переставал показывать `in_progress`;
--   * `lease_holder` и `lease_expires_at` срок НЕ проверяли — брали любую открытую попытку;
--   * `next` отбирал узлы по `lease_holder IS NULL`, то есть узел с истёкшей арендой исчезал
--     из «что делать дальше» навсегда, пока руками не запустят `doctor --gc`;
--   * хранимый `status` оставался `in_progress`, и `next` не видел узел ещё и по этой причине.
--
-- То есть узел освобождался только по явной команде. На живой базе это выглядело как 22
-- «занятых» узла в проекте RRL при одной реально живой аренде.
--
-- Что меняется. Представление — единственное место, где решается «что с узлом сейчас»,
-- поэтому исправляется именно оно, а не каждый читатель по отдельности:
--
--   1. `lease_holder` и `lease_expires_at` заполняются только для ЖИВОЙ попытки
--      (`ended_at IS NULL AND expires_at > now`). Истёкшая аренда для читателя не существует —
--      как и было обещано.
--   2. `in_progress` без живой аренды не показывается как `in_progress`: работа не идёт,
--      значит узел вернулся в очередь. Статус возвращается к тому, который допустим для вида
--      узла — `todo` для задачи, `planned` для эпика (у эпика `todo` не бывает, см. §7).
--
-- Почему статус восстанавливается по виду узла, а не хранится: до захвата узел уже имел
-- какой-то статус, но `claim` его перезаписал, и прежнее значение нигде не сохранено. Выбирать
-- приходится, а выбор по виду узла — единственный, который не может нарушить правило вида.
-- Согласованность с `status::allowed_for` проверяется тестом, иначе правило разъедется молча.
--
-- Само освобождение в таблицах (`attempts.ended_at`, снятые резервы, возврат `status`) делает
-- `lease::release_expired` — она вызывается на путях записи и из `doctor --gc`. Представление
-- чинит ЧТЕНИЕ, сборка мусора чинит ХРАНЕНИЕ: первое нужно всегда, второе — чтобы база не
-- обрастала открытыми попытками мёртвых сессий.

DROP VIEW v_node_effective;

CREATE VIEW v_node_effective AS
SELECT
  n.*,
  CASE
    WHEN n.status IN ('done','cancelled') THEN n.status
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
        AND b.deleted_at IS NULL AND b.status NOT IN ('done','cancelled')
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
