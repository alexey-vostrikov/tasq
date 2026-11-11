//! Карта того, что нарисовано: прямоугольник на экране → что за ним стоит.
//!
//! Мышь приходит координатами, а экран живёт индексами (проект №2, колонка `todo`, строка №5).
//! Связь между ними знает только отрисовка: она же решает, сколько строк влезло и с каким
//! смещением. Поэтому карта СОБИРАЕТСЯ при рисовании, а не считается второй раз из модели —
//! второй расчёт неизбежно разошёлся бы с первым (та же ошибка, из-за которой таблица и `--json`
//! когда-то показывали разные списки), и выглядела бы она как «кликнул по строке, открылся сосед».
//!
//! Карта обнуляется на каждый кадр: устаревшее попадание хуже отсутствующего — клик по пустому
//! месту после прокрутки открыл бы узел, которого там уже нет.

use ratatui::layout::{Position, Rect};

/// Что стоит за прямоугольником экрана.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Строка панели проектов: номер в списке проектов.
    Project(usize),
    /// Колонка канбана: клик ниже последней карточки ставит курсор на колонку.
    Column(usize),
    /// Карточка: колонка и строка в ней.
    Row { column: usize, row: usize },
    /// Панель текста под канбаном: клик открывает карточку узла.
    Detail,
}

/// Собранные за кадр попадания.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    items: Vec<(Rect, Hit)>,
}

impl Hits {
    /// Запоминает область. Пустые области пропускаются: прямоугольник нулевой высоты — это
    /// «панель не нарисована», и попадание в него означало бы клик по воздуху.
    pub fn add(&mut self, area: Rect, hit: Hit) {
        if area.width > 0 && area.height > 0 {
            self.items.push((area, hit));
        }
    }

    /// Присоединяет попадания другого кадра — например, панели текста к доске.
    pub fn extend(&mut self, other: Hits) {
        self.items.extend(other.items);
    }

    /// Что стоит в точке экрана.
    ///
    /// Поиск идёт от последнего добавленного: мелкие области (карточка) рисуются после крупных
    /// (колонка), и клик по карточке должен попадать в карточку, а не в колонку под ней.
    pub fn find(&self, column: u16, row: u16) -> Option<Hit> {
        let point = Position::new(column, row);
        self.items
            .iter()
            .rev()
            .find(|(area, _)| area.contains(point))
            .map(|(_, hit)| *hit)
    }

    /// Сколько попаданий собрано — для проверок, что кадр вообще размечен.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_added_area_wins() {
        let mut hits = Hits::default();
        hits.add(Rect::new(0, 0, 10, 10), Hit::Column(0));
        hits.add(Rect::new(0, 2, 10, 1), Hit::Row { column: 0, row: 3 });
        assert_eq!(hits.find(5, 2), Some(Hit::Row { column: 0, row: 3 }));
        assert_eq!(hits.find(5, 7), Some(Hit::Column(0)));
        assert_eq!(hits.find(20, 7), None, "вне всех областей — попадания нет");
    }

    #[test]
    fn an_empty_area_is_not_a_hit() {
        let mut hits = Hits::default();
        hits.add(Rect::new(0, 0, 0, 10), Hit::Detail);
        hits.add(Rect::new(0, 0, 10, 0), Hit::Detail);
        assert!(hits.is_empty(), "пустая область стала попаданием");
        assert_eq!(hits.find(0, 0), None);
    }
}
