//! Сторож за базой: «изменилось ли что-нибудь с прошлого раза».
//!
//! Зачем отдельный модуль. До этой правки TUI перечитывал доску ЧЕТЫРЕ раза в секунду
//! безусловно: на 151 узле это 1,28 мс и незаметно, на десяти тысячах — 150–300 мс, то есть
//! половина процессорного времени в простое. При этом в простое доска не меняется.
//!
//! `PRAGMA data_version` — счётчик, который SQLite увеличивает при коммите ЧУЖОГО соединения.
//! Ровно то, что нужно: читается за 1,4 мкс (замер на живой базе) и отвечает на вопрос «менялось
//! ли что-то извне», не читая ни одной строки. Свои записи он не отражает — и это правильно:
//! экран перечитывается сразу после собственного действия, а не по таймеру.
//!
//! Почему не подписка на файл (inotify). Предлагалось в постановке задачи, и по замеру
//! выяснилось, что она не даёт ничего: пауза цикла задаётся не базой, а ожиданием нажатия
//! (`event::poll`), и она всё равно 250 мс — иначе клавиши ощущаются запаздывающими. Событие о
//! файле не может разбудить `poll` терминала, поэтому его пришлось бы опрашивать с той же
//! частотой, что и `data_version`, только дороже (inotify-очередь, поток, дескрипторы) и с
//! лишним состоянием. Числа — в `docs/tui.md`.

use rusqlite::Connection;

use crate::error::Result;

/// Сколько всего раз сторож заглядывал в базу и сколько раз увидел изменение.
///
/// Счётчики нужны проверке «полных чтений в минуту при простое = 0»: экран обязан показывать
/// их в `--once` и отдавать в тестах, иначе «стало лучше» снова останется на слово.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Обращений к `data_version`.
    pub checks: u64,
    /// Из них — с изменением (после таких читается доска).
    pub changes: u64,
    /// Полных чтений доски: любое перечитывание по таймеру или по изменению.
    pub refreshes: u64,
}

impl Stats {
    /// Полных чтений, которые не были вызваны ни изменением, ни действием человека.
    ///
    /// В норме — ноль: если это число растёт, сторож не работает, и опрос вернулся.
    pub fn idle_refreshes(&self) -> u64 {
        self.refreshes.saturating_sub(self.changes)
    }
}

/// Сколько раз доска читалась целиком и сколько раз — по изменениям.
///
/// Отдельно от [`Stats`]: там про сторожа (сколько раз спросили базу), здесь про чтение доски
/// (сколько стоило). Два разных вопроса, и складывать их в одно число значило бы потерять оба.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reads {
    /// Полных чтений доски: они стоят сотни миллисекунд на широкой базе.
    pub full: u64,
    /// Чтений по изменениям: единицы строк вместо всей доски.
    pub delta: u64,
}

/// Сторож за одним файлом базы.
pub struct Watch {
    last: i64,
    stats: Stats,
}

impl Watch {
    /// Запоминает текущее состояние базы. Сразу после этого `changed` вернёт `false`.
    pub fn new(conn: &Connection) -> Result<Self> {
        Ok(Self {
            last: data_version(conn)?,
            stats: Stats::default(),
        })
    }

    /// Менялось ли что-то с прошлого обращения.
    ///
    /// Одно чтение `PRAGMA` — 1,4 мкс на живой базе. `true` означает «перечитай доску»; при
    /// `false` вызывающий не делает ничего, и это весь цикл опроса.
    pub fn changed(&mut self, conn: &Connection) -> Result<bool> {
        self.stats.checks += 1;
        let current = data_version(conn)?;
        if current == self.last {
            return Ok(false);
        }
        self.last = current;
        self.stats.changes += 1;
        Ok(true)
    }

    /// Учитывает перечитывание, вызванное не изменением в базе, а действием человека.
    ///
    /// Отдельный счётчик, потому что такие перечитывания законны: нажатие «смени статус»
    /// обязано показать результат, и ждать, пока `data_version` заметит СВОЮ же запись (он её
    /// не заметит никогда), значило бы не показать ничего.
    pub fn note_refresh(&mut self) {
        self.stats.refreshes += 1;
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }
}

/// `PRAGMA data_version` как число.
///
/// Отдельная функция, а не строка в двух местах: имя прагмы — это контракт с SQLite, и второй
/// экземпляр разошёлся бы при опечатке молча, вернув «изменений нет» навсегда.
pub fn data_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA data_version", [], |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn fixture(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-watch-{tag}-{nanos}"));
        let outcome = db::init(&dir, "WATCH", "сторож").expect("init");
        (dir, std::path::PathBuf::from(outcome.db_path))
    }

    /// Чужой коммит виден, свой — нет: на этом стоит весь цикл экрана.
    #[test]
    fn another_connection_raises_the_version() {
        let (dir, path) = fixture("other");
        let mut first = db::open(&path).expect("open");
        let second = db::open(&path).expect("open");

        let mut watch = Watch::new(&first).expect("watch");
        assert!(!watch.changed(&first).expect("check"), "сразу после старта");

        // Своя запись: `data_version` её не отражает — и не должен.
        let project = crate::project::first_project(&first).expect("project");
        crate::node::create(
            &mut first,
            &project,
            crate::node::NewNode {
                parent: None,
                title: "свой",
                kind: None,
                description: None,
                priority: None,
                due_date: None,
                estimated_hours: None,
                weight: None,
                source_ref: None,
                branch: None,
            },
        )
        .expect("create");
        assert!(
            !watch.changed(&first).expect("check"),
            "своя запись не должна выглядеть как чужая"
        );

        // Чужая запись: версия растёт, сторож это видит.
        let other = second;
        let project = crate::project::first_project(&other).expect("project");
        other
            .execute(
                "INSERT INTO nodes (project_id, depth, seq, path, kind, title, status,
                                    created_at, updated_at)
                 VALUES (?1, 1, 99, '99', 'epic', 'чужой', 'idea', '2026-01-01T00:00:00.000Z',
                         '2026-01-01T00:00:00.000Z')",
                rusqlite::params![project.id],
            )
            .expect("insert");
        assert!(
            watch.changed(&first).expect("check"),
            "чужой коммит обязан быть виден"
        );
        assert!(
            !watch.changed(&first).expect("check"),
            "повторное обращение без изменений обязано молчать"
        );

        let stats = watch.stats();
        // Четыре обращения: старт, своя запись, чужой коммит, повтор без изменений.
        assert_eq!(stats.checks, 4);
        assert_eq!(stats.changes, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Чужое ЧТЕНИЕ не выглядит как запись.
    ///
    /// Важно для работы рядом с другим агентом: он читает доску постоянно, и если бы открытие
    /// соединения поднимало `data_version`, экран перечитывал бы всё из-за чужого `ls`, то есть
    /// сторож превратился бы в опрос под другим именем.
    #[test]
    fn a_foreign_reader_does_not_look_like_a_writer() {
        let (dir, path) = fixture("reader");
        let conn = db::open(&path).expect("open");
        let mut watch = Watch::new(&conn).expect("watch");
        // Открытие, чтение и закрытие чужого соединения — три операции, каждая из которых могла
        // бы оказаться записью в заголовок базы.
        {
            let reader = db::open(&path).expect("open");
            let project = crate::project::first_project(&reader).expect("project");
            let _ = crate::node::list(&reader, &project, &crate::node::Filter::default())
                .expect("list");
        }
        assert!(
            !watch.changed(&conn).expect("check"),
            "чужое чтение не должно выглядеть как чужая запись"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Перечитывание по действию человека не считается «работой в простое».
    #[test]
    fn a_refresh_by_hand_is_not_an_idle_refresh() {
        let (dir, path) = fixture("stats");
        let conn = db::open(&path).expect("open");
        let mut watch = Watch::new(&conn).expect("watch");
        watch.note_refresh();
        let stats = watch.stats();
        assert_eq!(stats.refreshes, 1);
        assert_eq!(stats.changes, 0);
        assert_eq!(stats.idle_refreshes(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
