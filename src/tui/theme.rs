//! Цветовые темы: набор и значения как у bottom (`ClementTsang/bottom`).
//!
//! Файл написан так, чтобы его можно было вынести в отдельный крейт без правок: он зависит
//! только от `ratatui::style`, ничего не знает про tasq и не обращается ни к базе, ни к
//! состоянию экрана. Пока он живёт модулем, потому что второй потребитель (bazis-wp-cli) —
//! чужой репозиторий, и подключать его к нашему крейту без слова владельца нельзя.
//!
//! Три решения, которые задают вид:
//!
//! * **Шесть встроенных тем и те же имена, что у bottom** (`default`, `default-light`,
//!   `gruvbox`, `gruvbox-light`, `nord`, `nord-light`). Имена не выдуманы: человек, у которого
//!   bottom уже настроен на `nord`, ожидает увидеть тот же оттенок, а не «похожий».
//! * **Цвет — подсказка, а не носитель смысла.** Смысл остаётся в тексте: статус написан
//!   словом, просрочка — словом и числом дней, отказ — кодом. Поэтому тема без цвета
//!   (монохромный терминал, `--once` в лог) читается так же, и тесты проверяют текст, а не
//!   цвет.
//! * **Зебра считается от фона темы**, а не задана литералом: иначе светлая тема получала бы
//!   тёмные полосы и текст на них не читался.

use ratatui::style::{Color, Modifier, Style};

/// Палитра интерфейса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Имя темы — как у bottom.
    pub name: &'static str,
    /// Обычный текст.
    pub text: Color,
    /// Приглушённый текст: подписи, счётчики, неактивные элементы.
    pub dim: Color,
    /// Рамка неактивной панели.
    pub border: Color,
    /// Рамка и заголовок панели в фокусе.
    pub focus: Color,
    /// Шапка таблицы.
    pub header: Color,
    /// Цвет текста выделенной строки.
    pub selected_fg: Color,
    /// Фон выделенной строки.
    pub selected_bg: Color,
    /// Фон интерфейса: от него считается чередование строк.
    pub background: Color,
    /// Узел, с которым ещё не начали: `idea`, `backlog`, `planned`.
    pub fresh: Color,
    /// Работа идёт: `todo`, `in_progress`, `review`.
    pub active: Color,
    /// Узел заблокирован.
    pub blocked: Color,
    /// Узел закрыт: `done`.
    pub done: Color,
    /// Узел отменён или отложен.
    pub cancelled: Color,
    /// Просрочка.
    pub overdue: Color,
    /// Узел под живой арендой.
    pub leased: Color,
    /// Отказ и ошибка.
    pub error: Color,
    /// Подтверждение действия.
    pub ok: Color,
}

/// Доля сдвига яркости для соседних строк списка: ±5 % — строки различимы, но не спорят с
/// текстом.
const ZEBRA_SHIFT: f64 = 0.05;

/// Сдвигает яркость цвета на долю: `0.05` — на 5 % светлее, `-0.05` — на 5 % темнее.
///
/// Именованные цвета (их ставят светлые варианты тем) не трогаем: их оттенок задаёт терминал,
/// и «сдвиг» серого в терминале человека может стать неотличимым.
fn shade(color: Color, amount: f64) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let channel = |value: u8| -> u8 {
        let value = f64::from(value);
        let shifted = if amount >= 0.0 {
            value + (255.0 - value) * amount
        } else {
            value * (1.0 + amount)
        };
        shifted.round().clamp(0.0, 255.0) as u8
    };
    Color::Rgb(channel(r), channel(g), channel(b))
}

impl Theme {
    /// Тема bottom по умолчанию: цвета терминала, ничего не навязывает.
    pub const DEFAULT: Theme = Theme {
        name: "default",
        text: Color::Gray,
        dim: Color::DarkGray,
        border: Color::Gray,
        focus: Color::LightBlue,
        header: Color::LightBlue,
        selected_fg: Color::Black,
        selected_bg: Color::LightBlue,
        background: Color::Rgb(28, 28, 28),
        fresh: Color::LightCyan,
        active: Color::LightYellow,
        blocked: Color::LightRed,
        done: Color::Green,
        cancelled: Color::DarkGray,
        overdue: Color::LightRed,
        leased: Color::LightMagenta,
        error: Color::LightRed,
        ok: Color::LightGreen,
    };

    /// `default-light` — то же на светлом фоне.
    pub const DEFAULT_LIGHT: Theme = Theme {
        name: "default-light",
        text: Color::Black,
        dim: Color::Gray,
        border: Color::Black,
        focus: Color::Blue,
        header: Color::Black,
        selected_fg: Color::White,
        selected_bg: Color::Blue,
        background: Color::Rgb(244, 244, 244),
        fresh: Color::Blue,
        // Жёлтый, а не красный: красный в этой теме занят блокировкой и ошибкой, и «работа
        // идёт» не должно выглядеть как «нельзя».
        active: Color::Yellow,
        blocked: Color::Red,
        done: Color::Green,
        cancelled: Color::Gray,
        overdue: Color::Red,
        leased: Color::Magenta,
        error: Color::Red,
        ok: Color::Green,
    };

    /// `gruvbox` — «retro groove», тёмная.
    pub const GRUVBOX: Theme = Theme {
        name: "gruvbox",
        text: Color::Rgb(235, 219, 178),
        dim: Color::Rgb(146, 131, 116),
        border: Color::Rgb(102, 92, 84),
        focus: Color::Rgb(254, 128, 25),
        header: Color::Rgb(131, 165, 152),
        selected_fg: Color::Rgb(29, 32, 33),
        selected_bg: Color::Rgb(235, 219, 178),
        background: Color::Rgb(40, 40, 40),
        fresh: Color::Rgb(131, 165, 152),
        active: Color::Rgb(250, 189, 47),
        blocked: Color::Rgb(251, 73, 52),
        done: Color::Rgb(142, 192, 124),
        cancelled: Color::Rgb(102, 92, 84),
        overdue: Color::Rgb(251, 73, 52),
        leased: Color::Rgb(211, 134, 155),
        error: Color::Rgb(251, 73, 52),
        ok: Color::Rgb(184, 187, 38),
    };

    /// `gruvbox-light`.
    pub const GRUVBOX_LIGHT: Theme = Theme {
        name: "gruvbox-light",
        text: Color::Rgb(60, 56, 54),
        dim: Color::Rgb(146, 131, 116),
        border: Color::Rgb(168, 153, 132),
        focus: Color::Rgb(175, 58, 3),
        header: Color::Rgb(7, 102, 120),
        selected_fg: Color::Rgb(235, 219, 178),
        selected_bg: Color::Rgb(60, 56, 54),
        background: Color::Rgb(251, 241, 199),
        fresh: Color::Rgb(7, 102, 120),
        active: Color::Rgb(181, 118, 20),
        blocked: Color::Rgb(157, 0, 6),
        done: Color::Rgb(121, 116, 14),
        cancelled: Color::Rgb(146, 131, 116),
        overdue: Color::Rgb(157, 0, 6),
        leased: Color::Rgb(143, 63, 113),
        error: Color::Rgb(157, 0, 6),
        ok: Color::Rgb(66, 123, 88),
    };

    /// `nord` — «arctic, north-bluish».
    pub const NORD: Theme = Theme {
        name: "nord",
        text: Color::Rgb(229, 233, 240),
        dim: Color::Rgb(143, 161, 179),
        border: Color::Rgb(76, 86, 106),
        focus: Color::Rgb(136, 192, 208),
        header: Color::Rgb(129, 161, 193),
        selected_fg: Color::Rgb(46, 52, 64),
        selected_bg: Color::Rgb(136, 192, 208),
        background: Color::Rgb(46, 52, 64),
        fresh: Color::Rgb(136, 192, 208),
        active: Color::Rgb(235, 203, 139),
        blocked: Color::Rgb(191, 97, 106),
        done: Color::Rgb(163, 190, 140),
        cancelled: Color::Rgb(76, 86, 106),
        overdue: Color::Rgb(191, 97, 106),
        leased: Color::Rgb(180, 142, 173),
        error: Color::Rgb(191, 97, 106),
        ok: Color::Rgb(163, 190, 140),
    };

    /// `nord-light`.
    pub const NORD_LIGHT: Theme = Theme {
        name: "nord-light",
        text: Color::Rgb(46, 52, 64),
        dim: Color::Rgb(129, 161, 193),
        border: Color::Rgb(180, 190, 205),
        focus: Color::Rgb(94, 129, 172),
        header: Color::Rgb(94, 129, 172),
        selected_fg: Color::Rgb(245, 245, 245),
        selected_bg: Color::Rgb(94, 129, 172),
        background: Color::Rgb(236, 239, 244),
        fresh: Color::Rgb(94, 129, 172),
        active: Color::Rgb(163, 122, 30),
        blocked: Color::Rgb(191, 97, 106),
        done: Color::Rgb(94, 129, 76),
        cancelled: Color::Rgb(129, 141, 159),
        overdue: Color::Rgb(191, 97, 106),
        leased: Color::Rgb(143, 100, 140),
        error: Color::Rgb(191, 97, 106),
        ok: Color::Rgb(94, 129, 76),
    };

    /// Все встроенные темы — тот же список и порядок, что у bottom.
    pub fn all() -> [Theme; 6] {
        [
            Self::DEFAULT,
            Self::DEFAULT_LIGHT,
            Self::GRUVBOX,
            Self::GRUVBOX_LIGHT,
            Self::NORD,
            Self::NORD_LIGHT,
        ]
    }

    /// Имена тем для справки `--help`.
    pub fn names() -> &'static str {
        "default, default-light, gruvbox, gruvbox-light, nord, nord-light"
    }

    /// Тема по имени; регистр не важен.
    pub fn by_name(name: &str) -> Option<Theme> {
        let needle = name.trim().to_ascii_lowercase();
        Self::all().into_iter().find(|theme| theme.name == needle)
    }

    /// Следующая тема по кругу — для переключения в TUI.
    pub fn next(&self) -> Theme {
        let all = Self::all();
        let index = all
            .iter()
            .position(|theme| theme.name == self.name)
            .unwrap_or(0);
        all[(index + 1) % all.len()]
    }

    /// Фон строки списка: чётные на 5 % темнее основы, нечётные — на 5 % светлее.
    ///
    /// Номер строки — тот, что видно на экране, поэтому и после фильтра полосы идут подряд.
    pub fn zebra(&self, index: usize) -> Color {
        let shift = if index.is_multiple_of(2) {
            -ZEBRA_SHIFT
        } else {
            ZEBRA_SHIFT
        };
        shade(self.background, shift)
    }

    /// Цвет статуса узла.
    ///
    /// Неизвестный статус получает обычный текст: экран, который красит незнакомое в
    /// «ошибочный» красный, врёт про состояние базы.
    pub fn status(&self, status: &str) -> Color {
        match status {
            "idea" | "backlog" | "planned" => self.fresh,
            "todo" | "in_progress" | "review" => self.active,
            "blocked" => self.blocked,
            "done" => self.done,
            "cancelled" | "deferred" => self.cancelled,
            _ => self.text,
        }
    }

    /// Обычный текст.
    pub fn text_style(&self) -> Style {
        Style::default().fg(self.text)
    }

    /// Приглушённый текст.
    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.dim)
    }

    /// Рамка и заголовок панели в фокусе.
    pub fn focus_style(&self) -> Style {
        Style::default().fg(self.focus)
    }

    /// Рамка неактивной панели: у bottom её цвет — обычный текст темы.
    pub fn border_style(&self) -> Style {
        Style::default().fg(self.border)
    }

    /// Шапка таблицы — полужирная, как `table_header_style` у bottom.
    pub fn header_style(&self) -> Style {
        Style::default()
            .fg(self.header)
            .add_modifier(Modifier::BOLD)
    }

    /// Выделенная строка: фон и цвет текста из палитры.
    pub fn selected_style(&self) -> Style {
        Style::default().fg(self.selected_fg).bg(self.selected_bg)
    }

    /// Строка с отказом.
    pub fn error_style(&self) -> Style {
        Style::default().fg(self.error)
    }

    /// Строка с подтверждением.
    pub fn ok_style(&self) -> Style {
        Style::default().fg(self.ok)
    }
}

impl Default for Theme {
    /// Тема по умолчанию — `gruvbox`: тёмная, контрастная и не спорит с цветами статусов.
    fn default() -> Self {
        Self::GRUVBOX
    }
}

impl std::fmt::Display for Theme {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("ожидался RGB, а не {other:?}"),
        }
    }

    /// Чередование строк сдвигает фон на 5 % и не проваливается в серый на светлых темах.
    #[test]
    fn the_zebra_shift_follows_the_background() {
        // Основа gruvbox — #282828: чётная строка на 5 % темнее, нечётная на 5 % светлее.
        let (even, _, _) = rgb(Theme::GRUVBOX.zebra(0));
        let (odd, _, _) = rgb(Theme::GRUVBOX.zebra(1));
        assert_eq!(even, 38);
        assert_eq!(odd, 51);
        assert!(odd > even, "нечётная строка светлее чётной");
        assert_eq!(Theme::GRUVBOX.zebra(2), Theme::GRUVBOX.zebra(0));

        // На светлой теме полосы так же расходятся, но остаются светлыми: тёмная полоса на
        // светлом фоне сделала бы текст нечитаемым.
        let (light_even, _, _) = rgb(Theme::NORD_LIGHT.zebra(0));
        let (light_odd, _, _) = rgb(Theme::NORD_LIGHT.zebra(1));
        assert!(light_odd > light_even);
        assert!(
            light_even >= 200,
            "полоса на светлом фоне слишком тёмная: {light_even}"
        );

        // Именованный фон не трогаем: оттенок серого задаёт терминал.
        assert_eq!(shade(Color::Gray, 0.05), Color::Gray);
    }

    /// Все шесть тем находятся по имени, регистр не важен, мусор не проходит.
    #[test]
    fn every_theme_is_found_by_name() {
        for name in [
            "default",
            "default-light",
            "gruvbox",
            "gruvbox-light",
            "nord",
            "nord-light",
        ] {
            let theme = Theme::by_name(name).unwrap_or_else(|| panic!("нет темы {name}"));
            assert_eq!(theme.name, name);
            assert_ne!(theme.next().name, theme.name, "переход в себя у {name}");
        }
        assert_eq!(Theme::by_name("NORD").map(|t| t.name), Some("nord"));
        assert_eq!(
            Theme::by_name("  Gruvbox  ").map(|t| t.name),
            Some("gruvbox")
        );
        assert!(Theme::by_name("nord-unknown").is_none());
        assert!(Theme::by_name("").is_none());
    }

    /// Переключение идёт по кругу и возвращается в исходную тему.
    #[test]
    fn the_cycle_returns_to_the_start() {
        let mut theme = Theme::DEFAULT;
        for _ in 0..Theme::all().len() {
            theme = theme.next();
        }
        assert_eq!(theme.name, "default");
    }

    /// У каждого статуса есть свой цвет, и неизвестный получает обычный текст, а не «ошибку».
    #[test]
    fn statuses_map_to_their_own_colours() {
        for theme in Theme::all() {
            for (status, expected) in [
                ("idea", theme.fresh),
                ("backlog", theme.fresh),
                ("planned", theme.fresh),
                ("todo", theme.active),
                ("in_progress", theme.active),
                ("review", theme.active),
                ("blocked", theme.blocked),
                ("done", theme.done),
                ("cancelled", theme.cancelled),
                ("deferred", theme.cancelled),
            ] {
                assert_eq!(theme.status(status), expected, "{}: {status}", theme.name);
            }
            assert_eq!(
                theme.status("нет-такого"),
                theme.text,
                "{}: неизвестный статус покрашен как ошибка",
                theme.name
            );
        }
    }

    /// Смысловые цвета в каждой теме действительно различаются: тема, где «закрыто» и
    /// «заблокировано» одного цвета, бесполезна.
    #[test]
    fn meaningful_colours_differ_within_a_theme() {
        for theme in Theme::all() {
            let pairs = [
                ("done", theme.done, theme.blocked),
                ("done", theme.done, theme.error),
                ("blocked", theme.blocked, theme.active),
                ("overdue", theme.overdue, theme.done),
                ("leased", theme.leased, theme.done),
            ];
            for (what, left, right) in pairs {
                assert_ne!(left, right, "{}: {what} одного цвета с соседом", theme.name);
            }
        }
    }
}
