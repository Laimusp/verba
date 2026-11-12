//! UI Automation: поле в фокусе при нажатии хоткея и возврат фокуса в него.
//!
//! Потоки снимка и доставки — в MTA (COM): указатель на элемент, полученный в MTA, годен в любом
//! потоке MTA процесса, поэтому снимок передаёт элемент потоку доставки (`Element`). Отпускается
//! он где угодно: потоки без `CoInitializeEx` попадают в неявный MTA, пока жив хоть один поток MTA,
//! а наши живут всё время работы.
//!
//! Таймауты UIA (`IUIAutomation2`): подключение к провайдеру и один запрос — не дольше секунды,
//! зависшее приложение не держит поток дольше.

use windows::core::{Interface, BSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCondition, IUIAutomationElement,
    TreeScope_Descendants, UIA_AutomationIdPropertyId, UIA_ControlTypePropertyId, UIA_IsKeyboardFocusablePropertyId,
    UIA_NamePropertyId, UIA_CONTROLTYPE_ID, UIA_E_ELEMENTNOTAVAILABLE,
};

const CONNECTION_TIMEOUT_MS: u32 = 1000;
const TRANSACTION_TIMEOUT_MS: u32 = 1000;
/// Протухший элемент ищется по положению: допуск на сдвиг рамки, пикселей.
const RECT_SLACK: i32 = 8;

/// Элемент UIA, полученный в MTA (см. шапку модуля).
pub struct Element(pub IUIAutomationElement);
// SAFETY: указатель из MTA годен в любом потоке MTA процесса (правила COM); все потоки, которые
// его трогают, — в MTA (явном или неявном, см. шапку модуля).
unsafe impl Send for Element {}
unsafe impl Sync for Element {}

/// Поле, запомненное при нажатии хоткея.
pub struct Field {
    pub element: Element,
    pub automation_id: String,
    pub name: String,
    pub control_type: i32,
    pub class: String,
    pub framework: String,
    pub rect: RECT,
    /// окно элемента (у полей веб-страниц — окно браузера целиком)
    pub hwnd: isize,
    /// сколько занял снимок UIA, мс
    pub ms: u64,
}

impl Field {
    /// Элемент — окно или панель, а не поле: так отвечает Chrome на первый запрос, пока его дерево
    /// доступности не включилось (`Pane` без своего окна — замер в `docs/focus-restore.md`).
    /// Такой элемент поле не опознаёт: фокус возвращается по окну, как без UIA.
    pub fn is_window_level(&self) -> bool {
        const PANE: i32 = 50033;
        const WINDOW: i32 = 50032;
        matches!(self.control_type, PANE | WINDOW)
    }

    /// Кратко для лога: без имени поля (в нём бывает текст человека).
    pub fn label(&self) -> String {
        let fw = if self.framework.is_empty() { "?" } else { &self.framework };
        let window = if self.hwnd != 0 {
            format!(" в окне {}", super::win::class_name(super::win::hwnd(self.hwnd)))
        } else {
            " без окна".to_string()
        };
        format!("{fw}/{}{window}", control_type_name(self.control_type))
    }
}

pub fn control_type_name(ct: i32) -> String {
    match ct {
        50004 => "Edit".into(),
        50030 => "Document".into(),
        50033 => "Pane".into(),
        50032 => "Window".into(),
        50025 => "Custom".into(),
        50020 => "Text".into(),
        50003 => "ComboBox".into(),
        n => n.to_string(),
    }
}

pub struct Uia {
    auto: IUIAutomation,
}

fn s(r: windows::core::Result<BSTR>) -> String {
    r.map(|b| b.to_string()).unwrap_or_default()
}

/// Элемент исчез (страница перерисовала поле, окно закрыто).
pub fn is_gone(e: &windows::core::Error) -> bool {
    e.code().0 as u32 == UIA_E_ELEMENTNOTAVAILABLE
}

impl Uia {
    /// В текущем потоке: вход в MTA и объект UIA с таймаутами.
    pub fn new() -> Result<Uia, String> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() {
                return Err(format!("COM: {hr:?}"));
            }
            let auto: IUIAutomation = match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) {
                Ok(a) => a,
                Err(_) => CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|e| format!("UIA: {e}"))?,
            };
            if let Ok(a2) = auto.cast::<IUIAutomation2>() {
                let _ = a2.SetConnectionTimeout(CONNECTION_TIMEOUT_MS);
                let _ = a2.SetTransactionTimeout(TRANSACTION_TIMEOUT_MS);
            }
            Ok(Uia { auto })
        }
    }

    pub fn focused(&self) -> windows::core::Result<IUIAutomationElement> {
        unsafe { self.auto.GetFocusedElement() }
    }

    /// Свойства элемента; элемент исчез — ошибка.
    pub fn describe(&self, el: IUIAutomationElement, ms: u64) -> windows::core::Result<Field> {
        unsafe {
            let control_type = el.CurrentControlType()?.0;
            Ok(Field {
                automation_id: s(el.CurrentAutomationId()),
                name: s(el.CurrentName()),
                class: s(el.CurrentClassName()),
                framework: s(el.CurrentFrameworkId()),
                rect: el.CurrentBoundingRectangle().unwrap_or_default(),
                hwnd: el.CurrentNativeWindowHandle().map(|h| h.0 as isize).unwrap_or(0),
                control_type,
                element: Element(el),
                ms,
            })
        }
    }

    /// Один и тот же элемент (сравнение по RuntimeId внутри UIA).
    pub fn same(&self, a: &IUIAutomationElement, b: &IUIAutomationElement) -> bool {
        unsafe { self.auto.CompareElements(a, b) }.is_ok_and(|r| r.as_bool())
    }

    /// Фокус сейчас на `el`.
    pub fn has_focus(&self, el: &IUIAutomationElement) -> bool {
        self.focused().is_ok_and(|f| self.same(&f, el))
    }

    fn condition(&self, prop: windows::Win32::UI::Accessibility::UIA_PROPERTY_ID, value: VARIANT) -> windows::core::Result<IUIAutomationCondition> {
        unsafe { self.auto.CreatePropertyCondition(prop, &value) }
    }

    /// Поле перерисовали (веб-страница пересоздала элемент — старый RuntimeId протух): найти его
    /// в том же окне — по AutomationId, иначе по имени, иначе по положению. Подходящих несколько —
    /// берётся ближайший к прежнему месту; дальше прежней рамки с допуском — не он.
    pub fn refind(&self, top: HWND, f: &Field) -> Option<IUIAutomationElement> {
        unsafe {
            let root = self.auto.ElementFromHandle(top).ok()?;
            let by_type = self.condition(UIA_ControlTypePropertyId, VARIANT::from(f.control_type)).ok()?;
            let key = if !f.automation_id.is_empty() {
                Some(self.condition(UIA_AutomationIdPropertyId, VARIANT::from(f.automation_id.as_str())).ok()?)
            } else if !f.name.is_empty() {
                Some(self.condition(UIA_NamePropertyId, VARIANT::from(f.name.as_str())).ok()?)
            } else {
                None
            };
            let cond = match key {
                Some(k) => self.auto.CreateAndCondition(&by_type, &k).ok()?,
                None => {
                    let focusable = self.condition(UIA_IsKeyboardFocusablePropertyId, VARIANT::from(true)).ok()?;
                    self.auto.CreateAndCondition(&by_type, &focusable).ok()?
                }
            };
            let all = root.FindAll(TreeScope_Descendants, &cond).ok()?;
            let n = all.Length().ok()?;
            let mut best: Option<(i64, IUIAutomationElement)> = None;
            for i in 0..n {
                let Ok(el) = all.GetElement(i) else { continue };
                let r = el.CurrentBoundingRectangle().unwrap_or_default();
                let d = distance(&r, &f.rect);
                if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                    best = Some((d, el));
                }
            }
            let (d, el) = best?;
            let slack = i64::from(RECT_SLACK) * 4;
            // единственное совпадение по AutomationId/имени принимается и после сдвига страницы
            if d <= slack || (n == 1 && (!f.automation_id.is_empty() || !f.name.is_empty())) {
                Some(el)
            } else {
                None
            }
        }
    }
}

/// Сумма сдвигов сторон рамки, пикселей.
pub fn distance(a: &RECT, b: &RECT) -> i64 {
    [a.left - b.left, a.top - b.top, a.right - b.right, a.bottom - b.bottom].iter().map(|d| i64::from(d.abs())).sum()
}

pub fn control_type(id: i32) -> UIA_CONTROLTYPE_ID {
    UIA_CONTROLTYPE_ID(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_distance() {
        let a = RECT { left: 10, top: 20, right: 110, bottom: 50 };
        assert_eq!(distance(&a, &a), 0);
        let b = RECT { left: 12, top: 18, right: 112, bottom: 52 };
        assert_eq!(distance(&a, &b), 8);
    }
}
