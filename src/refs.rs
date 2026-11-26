//! Переписывание ссылок на узел, когда меняется его адрес.
//!
//! Зачем отдельный модуль и чистые функции. Ссылка на узел — это текст в чужом тексте: в
//! описании задачи, в комментарии, в `source_ref`, в заметке. При переносе узла в другой проект
//! меняется ключ в адресе (`RRL-3.1` → `TASQDEV-12`), и если ссылки не переписать, они тихо
//! перестают разрешаться — искать причину будет человек, который через месяц откроет заметку и
//! получит `NODE_NOT_FOUND` на узле, который существует.
//!
//! Правила, из которых собран поиск:
//!
//!   * адрес — токен, а не подстрока: `RRL-3.1` не должен совпасть внутри `XRRL-3.1`, `RRL-3.10`
//!     или `RRL-3.1x`. Поэтому проверяются границы с двух сторон;
//!   * потомок — тоже ссылка: `RRL-3.1.5` переписывается в `<новый>-5`, потому что это тот же
//!     узел поддерева, а не другая ветка;
//!   * чужое не трогаем: `RRL-3.11` при переносе `RRL-3.1` остаётся как было — это другой узел.

/// Переписывает все ссылки на перенесённое поддерево в тексте.
///
/// `None` — менять нечего. `Some(text)` — новый текст; возвращается только когда что-то
/// изменилось, чтобы вызывающий не писал в базу строки, которые остались прежними (иначе
/// `updated_at` и журнал врали бы про правку, которой не было).
pub fn rewrite(text: &str, old_prefix: &str, new_prefix: &str) -> Option<String> {
    // `old_prefix` — это `КЛЮЧ-путь` целиком, например `RRL-3.1`; `new_prefix` — новый адрес
    // того же узла, например `TASQDEV-12`.
    let (old_key, old_path) = old_prefix.split_once('-')?;
    let (new_key, new_path) = new_prefix.split_once('-')?;
    if old_key.is_empty() || old_path.is_empty() || new_path.is_empty() {
        return None;
    }

    let needle = format!("{old_key}-");
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    let mut changed = false;

    while let Some(offset) = text[pos..].find(&needle) {
        let start = pos + offset;
        // Граница слева: адрес не должен быть хвостом другого слова (`XRRL-1`, `MY-RRL-1`).
        let left_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_');
        let path_start = start + needle.len();
        let path_end = path_end(bytes, path_start);
        let found = &text[path_start..path_end];

        let replacement = if left_ok {
            match_path(found, old_path).map(|suffix| (suffix, path_end))
        } else {
            None
        };

        match replacement {
            Some((suffix, end)) => {
                out.push_str(&text[pos..start]);
                out.push_str(new_key);
                out.push('-');
                out.push_str(new_path);
                out.push_str(suffix);
                changed = true;
                pos = end;
            }
            None => {
                // Совпадение не наше: переносим найденный кусок целиком и продолжаем за ним,
                // иначе поиск зациклился бы на том же месте.
                out.push_str(&text[pos..path_start]);
                pos = path_start;
            }
        }
    }
    out.push_str(&text[pos..]);

    if changed { Some(out) } else { None }
}

/// Хвост пути после совпавшей части: пусто для самого узла, `.5` для потомка.
///
/// `found` — то, что стоит в тексте после `КЛЮЧ-`. Совпадением считается либо точное равенство
/// пути, либо путь, продолжающийся точкой: `3.1` и `3.1.5` — один узел и его потомок. Число,
/// прилипшее без точки (`3.1` + `0` = `3.10`), — другой узел, и его трогать нельзя.
///
/// Хвост возвращается целиком, вместе с точкой: он подставляется сразу после нового пути
/// (`TASQDEV-12` + `.5`).
fn match_path<'a>(found: &'a str, old_path: &str) -> Option<&'a str> {
    let suffix = found.strip_prefix(old_path)?;
    if suffix.is_empty() || suffix.starts_with('.') {
        Some(suffix)
    } else {
        None
    }
}

/// Конец числового пути: цифры и точки с цифрами после них.
///
/// Точка в конце фразы (`см. RRL-3.1.`) в путь не входит: иначе адрес «съел» бы знак
/// препинания и переписал его вместе с собой.
fn path_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    if index >= bytes.len() || !bytes[index].is_ascii_digit() {
        return start;
    }
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        if bytes[index] == b'.' && index + 1 < bytes.len() && bytes[index + 1].is_ascii_digit() {
            index += 1;
            continue;
        }
        break;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::rewrite;

    #[test]
    fn the_node_and_its_descendants_are_rewritten() {
        assert_eq!(
            rewrite("см. RRL-3.1 и RRL-3.1.5", "RRL-3.1", "TASQDEV-12").as_deref(),
            Some("см. TASQDEV-12 и TASQDEV-12.5")
        );
    }

    /// Соседние узлы не трогаются: `3.10`, `3.11` — другие задачи, и переписать их значило бы
    /// подменить ссылки в чужих записях.
    #[test]
    fn neighbouring_nodes_are_left_alone() {
        assert_eq!(
            rewrite("RRL-3.10 и RRL-3.11", "RRL-3.1", "TASQDEV-12"),
            None
        );
        assert_eq!(rewrite("RRL-3.12", "RRL-3.1", "TASQDEV-12"), None);
    }

    /// Адрес внутри слова — не адрес.
    #[test]
    fn a_prefix_inside_another_word_is_not_a_reference() {
        assert_eq!(rewrite("XRRL-3.1", "RRL-3.1", "TASQDEV-12"), None);
        assert_eq!(rewrite("MY-RRL-3.1", "RRL-3.1", "TASQDEV-12"), None);
        // А вот в скобках, кавычках и после двоеточия — адрес.
        assert_eq!(
            rewrite("(RRL-3.1), «RRL-3.1», RRL-3.1:", "RRL-3.1", "TASQDEV-12").as_deref(),
            Some("(TASQDEV-12), «TASQDEV-12», TASQDEV-12:")
        );
    }

    /// Точка в конце фразы — знак препинания, а не часть адреса.
    #[test]
    fn a_trailing_full_stop_is_not_part_of_the_path() {
        assert_eq!(
            rewrite("закрыто в RRL-3.1.", "RRL-3.1", "TASQDEV-12").as_deref(),
            Some("закрыто в TASQDEV-12.")
        );
    }

    /// Ссылка на другой проект — не наша: ключ обязан совпасть целиком.
    #[test]
    fn another_projects_kept_as_is() {
        assert_eq!(rewrite("RRL-9.1", "RRL-3.1", "TASQDEV-12"), None);
        assert_eq!(rewrite("KRB-3.1", "RRL-3.1", "TASQDEV-12"), None);
    }

    /// Несколько вхождений и текст вокруг них.
    #[test]
    fn every_occurrence_is_rewritten() {
        assert_eq!(
            rewrite(
                "начало RRL-3.1 середина RRL-3.1.2 и RRL-3.1 конец",
                "RRL-3.1",
                "TASQDEV-12"
            )
            .as_deref(),
            Some("начало TASQDEV-12 середина TASQDEV-12.2 и TASQDEV-12 конец")
        );
    }

    /// Юникод вокруг адреса не ломает границы.
    #[test]
    fn unicode_neighbours_are_boundaries() {
        assert_eq!(
            rewrite("«RRL-3.1» — задача", "RRL-3.1", "TASQDEV-12").as_deref(),
            Some("«TASQDEV-12» — задача")
        );
    }
}
