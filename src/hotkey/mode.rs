//! Режимы основного хоткея: держать / нажал-включил / гибрид. Чистая логика: нажатия и
//! отпускания со временем → «начать запись», «закончить», ничего.

use crate::config::HotkeyMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Start,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// сочетание нажато, запись идёт; `since` — момент нажатия, мс
    Holding { since: u64 },
    /// запись идёт без удержания (toggle или короткое нажатие в гибриде)
    Latched,
    /// запись закончили нажатием (toggle/гибрид) — ждём отпускания, оно ничего не делает
    StopPressed,
}

#[derive(Debug, Clone)]
pub struct ModeMachine {
    mode: HotkeyMode,
    hybrid_hold_ms: u64,
    phase: Phase,
}

impl ModeMachine {
    pub fn new(mode: HotkeyMode, hybrid_hold_ms: u64) -> Self {
        Self { mode, hybrid_hold_ms, phase: Phase::Idle }
    }

    /// Смена настроек: в момент смены запись не идёт (иначе применяется после неё).
    pub fn configure(&mut self, mode: HotkeyMode, hybrid_hold_ms: u64) {
        self.mode = mode;
        self.hybrid_hold_ms = hybrid_hold_ms;
    }

    pub fn press(&mut self, now_ms: u64) -> Action {
        match (self.phase, self.mode) {
            (Phase::Idle, HotkeyMode::Hold | HotkeyMode::Hybrid) => {
                self.phase = Phase::Holding { since: now_ms };
                Action::Start
            }
            (Phase::Idle, HotkeyMode::Toggle) => {
                self.phase = Phase::Latched;
                Action::Start
            }
            (Phase::Latched, _) => {
                self.phase = Phase::StopPressed;
                Action::Stop
            }
            // повторное нажатие без отпускания (отпускание потерялось) — не начинать вторую запись
            (Phase::Holding { .. }, _) | (Phase::StopPressed, _) => Action::None,
        }
    }

    pub fn release(&mut self, now_ms: u64) -> Action {
        match self.phase {
            Phase::Holding { since } => {
                if self.mode == HotkeyMode::Hybrid && now_ms.saturating_sub(since) < self.hybrid_hold_ms {
                    self.phase = Phase::Latched;
                    Action::None
                } else {
                    self.phase = Phase::Idle;
                    Action::Stop
                }
            }
            Phase::StopPressed => {
                self.phase = Phase::Idle;
                Action::None
            }
            Phase::Idle | Phase::Latched => Action::None,
        }
    }

    /// Запись закончилась не хоткеем (Esc, лимит длины, блокировка экрана).
    pub fn reset(&mut self) {
        self.phase = match self.phase {
            // сочетание ещё нажато: его отпускание не должно ничего начать или закончить
            Phase::Holding { .. } => Phase::StopPressed,
            _ => Phase::Idle,
        };
    }

    /// Удержание идёт: отпускание закончит запись (сторож проверяет модификаторы только тогда).
    pub fn holding(&self) -> bool {
        matches!(self.phase, Phase::Holding { .. })
    }

    /// После потерянного отпускания (Win+L, сочетание отпущено в окне с правами) — забыть удержание.
    pub fn forget_hold(&mut self) {
        if matches!(self.phase, Phase::StopPressed) {
            self.phase = Phase::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;

    #[test]
    fn hold_starts_on_press_and_stops_on_release() {
        let mut m = ModeMachine::new(HotkeyMode::Hold, 300);
        assert_eq!(m.press(0), Start);
        assert!(m.holding());
        assert_eq!(m.release(50), Stop);
        assert_eq!(m.release(60), None);
        assert_eq!(m.press(100), Start);
    }

    #[test]
    fn toggle_needs_two_presses() {
        let mut m = ModeMachine::new(HotkeyMode::Toggle, 300);
        assert_eq!(m.press(0), Start);
        assert_eq!(m.release(2000), None);
        assert_eq!(m.press(3000), Stop);
        assert_eq!(m.release(3100), None);
        assert_eq!(m.press(4000), Start);
    }

    #[test]
    fn hybrid_short_press_latches_long_press_holds() {
        let mut m = ModeMachine::new(HotkeyMode::Hybrid, 300);
        assert_eq!(m.press(0), Start);
        assert_eq!(m.release(120), None, "короткое нажатие фиксирует запись");
        assert_eq!(m.press(5000), Stop);
        assert_eq!(m.release(5100), None);

        assert_eq!(m.press(6000), Start);
        assert_eq!(m.release(6000 + 300), Stop, "граница: ровно порог — уже удержание");
    }

    #[test]
    fn repeated_press_without_release_does_nothing() {
        let mut m = ModeMachine::new(HotkeyMode::Hold, 300);
        assert_eq!(m.press(0), Start);
        assert_eq!(m.press(10), None);
        assert_eq!(m.release(20), Stop);
    }

    #[test]
    fn reset_while_held_swallows_the_release() {
        let mut m = ModeMachine::new(HotkeyMode::Hold, 300);
        assert_eq!(m.press(0), Start);
        m.reset();
        assert_eq!(m.release(500), None, "отпускание после Esc не шлёт второй стоп");
        assert_eq!(m.press(600), Start);
    }

    #[test]
    fn reset_when_latched_goes_idle() {
        let mut m = ModeMachine::new(HotkeyMode::Toggle, 300);
        assert_eq!(m.press(0), Start);
        m.reset();
        assert_eq!(m.press(100), Start);
    }

    #[test]
    fn lost_release_is_forgotten() {
        let mut m = ModeMachine::new(HotkeyMode::Hold, 300);
        assert_eq!(m.press(0), Start);
        m.reset();
        m.forget_hold();
        assert_eq!(m.press(10_000), Start);
    }
}
