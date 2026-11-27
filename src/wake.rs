//! Пробуждение по событию: клавиша, изменение в базе — и ничего больше.
//!
//! Зачем. Прежний цикл экрана просыпался четыре раза в секунду и на каждом пробуждении
//! спрашивал терминал «есть ли клавиша». Изменение в базе он замечал только на таком тике, а
//! тик нельзя сделать длиннее: клавиша начала бы ощущаться запаздывающей. Отсюда две независимые
//! частоты, и обе платятся процессором.
//!
//! Здесь обе частоты заменены ожиданием. Ввод читается в отдельном потоке (crossterm умеет это
//! только блокирующим чтением) и приходит письмом; наблюдение за КАТАЛОГОМ базы присылает письмо
//! на чужую запись; цикл спит в `recv_timeout` и просыпается по письму или по часам. Часы нужны
//! ровно одному: аренда истекает и просрочка наступает по времени, а не по записи в базе, и
//! события об этом взять неоткуда.
//!
//! Почему каталог, а не файл. `.tasq/tasq.db` в рабочих каталогах — симлинк на базу в
//! `~/.local/share/tasq/`, а `tasq load` и восстановление из дампа пересоздают файл. Наблюдение за
//! inode после первого такого пересоздания умерло бы тихо: экран перестал бы видеть изменения и
//! выглядел бы «залипшим». Каталог переживает подмену файла, потому что создание новой записи
//! в нём — тоже событие.
//!
//! Почему `notify`, а не свои вызовы inotify. В крейте запрещён `unsafe` (spec §19), а прямые
//! вызовы `inotify_init1`/`inotify_add_watch` без него не сделать. `notify` держит `unsafe` у
//! себя, а нам отдаёт поток и колбэк — плюс кроссплатформенность, которую мы пока не используем,
//! но и не оплачиваем: на Linux он компилируется в inotify.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use notify::{RecursiveMode, Watcher as _};
use ratatui::crossterm::event::{self, Event};

use crate::error::{Error, Result};

/// Письмо циклу экрана: то, из-за чего он просыпается.
#[derive(Debug)]
pub enum Mail {
    /// Нажатие, движение мыши, ресайз — всё, что отдаёт терминал.
    Input(Event),
    /// Файл базы тронут. Это ещё не значит «доска изменилась»: `data_version` подтвердит.
    DbTouched,
    /// Чтение терминала кончилось отказом: цикл обязан выйти, а не крутиться без ввода.
    InputFailed(String),
}

/// То, что будит цикл: поток ввода и наблюдение за каталогом базы.
pub struct Inbox {
    mail: Receiver<Mail>,
    /// Наблюдатель держится здесь: его `Drop` снимает наблюдение. Поле не читается — важна жизнь.
    _watcher: Option<notify::RecommendedWatcher>,
    /// Поток ввода отсоединён: он завершится сам, когда закроется канал (конец цикла).
    _input: std::thread::JoinHandle<()>,
}

impl Inbox {
    /// Заводит поток ввода и, если база лежит в файле, наблюдение за его каталогом.
    ///
    /// Наблюдение необязательно: у сборки вокруг доски базы может не быть вовсе (память, тест), и
    /// отсутствие сторожа не повод не показывать экран — часы и клавиши работают и без него.
    pub fn start(db_path: Option<&Path>) -> Result<Self> {
        let (sender, mail) = channel();

        let input = {
            let sender: Sender<Mail> = sender.clone();
            std::thread::Builder::new()
                .name("tasq-tui-input".to_string())
                .spawn(move || {
                    loop {
                        match event::read() {
                            Ok(event) => {
                                if sender.send(Mail::Input(event)).is_err() {
                                    return;
                                }
                            }
                            Err(err) => {
                                let _ = sender.send(Mail::InputFailed(err.to_string()));
                                return;
                            }
                        }
                    }
                })
                .map_err(|err| Error::NoTerminal {
                    reason: err.to_string(),
                })?
        };

        let watcher = match db_path.and_then(Path::parent) {
            Some(dir) => Some(watch_dir(dir, sender)?),
            None => None,
        };

        Ok(Self {
            mail,
            _watcher: watcher,
            _input: input,
        })
    }

    /// Ждёт письма не дольше `timeout`. `None` — время вышло (в простое это норма).
    pub fn wait(&self, timeout: Duration) -> Option<Mail> {
        match self.mail.recv_timeout(timeout) {
            Ok(mail) => Some(mail),
            Err(RecvTimeoutError::Timeout) => None,
            // Отправители кончились: поток ввода завершился, наблюдение снято. Экран обязан
            // выйти, а не ждать вечно письма, которого не будет.
            Err(RecvTimeoutError::Disconnected) => Some(Mail::InputFailed(
                "input and file watcher are gone".to_string(),
            )),
        }
    }
}

/// Наблюдение за каталогом базы: письмо на любое событие, касающееся файла базы.
///
/// Фильтр по имени, а не «любое событие в каталоге»: рядом с базой живут `-wal`, `-journal` и
/// файлы блокировок, а в рабочем каталоге — вообще всё, что пишет человек. Слать письмо на каждую
/// запись в каталоге значило бы будить экран чужой работой.
fn watch_dir(dir: &Path, sender: Sender<Mail>) -> Result<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if !event.paths.iter().any(|path| is_database_file(path)) {
            return;
        }
        let _ = sender.send(Mail::DbTouched);
    })
    .map_err(|err| Error::InvalidValue {
        field: "watcher".to_string(),
        value: err.to_string(),
    })?;
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .map_err(|err| Error::InvalidValue {
            field: "watcher".to_string(),
            value: format!("{}: {err}", dir.display()),
        })?;
    Ok(watcher)
}

/// Имя файла базы или её спутников: `tasq.db`, `tasq.db-wal`, `tasq.db-journal`, `tasq.db-shm`.
///
/// Сравнение по имени, а не по полному пути: события приходят и с путём каталога без файла (когда
/// запись удаляют), и с путём вида `./.tasq/tasq.db` — сравнивать строки путей значило бы зависеть
/// от того, как их собрала библиотека.
fn is_database_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == crate::db::DB_FILE
        || name.starts_with(&format!("{}-", crate::db::DB_FILE))
        || name == format!("{}.lock", crate::db::DB_FILE)
}

/// Путь к каталогу базы в том виде, в каком его знает соединение.
pub fn database_path(conn: &rusqlite::Connection) -> Option<PathBuf> {
    conn.path().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Имена спутников базы распознаются все: `-wal`, `-journal`, `-shm` и замок.
    ///
    /// Проверяется отдельно от наблюдения, потому что именно здесь ошибка дешевле всего и дороже
    /// всего в жизни: пропущенный `-wal` означает «изменения в базе видны только при закрытии
    /// соединения», а это уже не задержка, а неверные данные на экране.
    #[test]
    fn every_companion_file_of_the_database_is_recognised() {
        let base = crate::db::DB_FILE;
        for name in [
            base.to_string(),
            format!("{base}-wal"),
            format!("{base}-journal"),
            format!("{base}-shm"),
            format!("{base}.lock"),
        ] {
            assert!(
                is_database_file(Path::new("/tmp/x").join(&name).as_path()),
                "{name} не распознан как файл базы"
            );
        }
        for name in ["other.db", "tasq.db.bak", "notes.txt"] {
            assert!(
                !is_database_file(Path::new("/tmp/x").join(name).as_path()),
                "{name} принят за файл базы"
            );
        }
    }
}
