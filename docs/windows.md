# Обходы ограничений Windows (CLAUDE.md §9)

## Суть
- Хук клавиатуры Windows снимает молча — Verba видит это по Raw Input и ставит хук заново; плюс переустановка после разблокировки и сна.
- Пока впереди окно настроек Verba, Raw Input у процесса снят: иначе Windows не зовёт хук Verba вовсе.
- Сочетание с Alt или Win, проглоченное хуком, открыло бы меню окна или «Пуск» — Verba гасит это пустым нажатием неназначенной клавиши.
- Вставка: буфер с отложенной отдачей даёт квитанцию о прочтении; после неё прежнее содержимое возвращается. Текст диктовки не попадает в журнал Win+V и облако.
- Окно с правами администратора: Verba без прав в него не вставит, и Windows об этом не сообщает.
- Вставка в окно, которое уже не впереди: обходы и замеры — `docs/focus-restore.md`.
- Оверлей записи не берёт фокус и пропускает мышь; кадр — DIB с предумноженной альфой. Текст GDI рисуется в отдельную маску: GDI обнуляет альфу пикселей, которых коснулся.
- Автозапуск с правами — задача планировщика «Verba (admin)»: создаётся через одно окно UAC, читается и запускается без прав. Приоритет задачи — 4, иначе Windows запустит Verba ниже обычного.
- Каждый пункт ниже: что, источник, проверено ли на живой системе (12.11.2026, Windows 11 Pro 26100).

## Хук WH_KEYBOARD_LL снимается молча
- **Что:** обработчик дольше `LowLevelHooksTimeout` (с Windows 10 1709 — не больше 1000 мс) → с Windows 7 хук снимается без уведомления, приложение об этом не узнаёт.
- **Источник:** [LowLevelKeyboardProc](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc), раздел Remarks.
- **Как в Verba:** обработчик только сверяет клавишу по атомикам и кладёт событие в канал (`src/hotkey/hook.rs` `hook_proc`). Сторож: Raw Input (`RIDEV_INPUTSINK`) получает нажатия независимо от хука; клавиши шли, а хук молчал > 1,5 с → переустановка. После `WTS_SESSION_UNLOCK` и выхода из сна — переустановка всегда.
- **Проверено:** хук работает (15 диктовок подряд). Само снятие вживую не воспроизводилось — для этого нужно задержать обработчик дольше секунды. [не проверено вживую]
- **Риск:** при фокусе на окне с правами администратора хук этих клавиш не получает (UIPI); если Raw Input их получает, сторож переустанавливает хук раз в ~2 с, пока идёт ввод (безвредно, в логе — строка раз в минуту). [гипотеза]

## Raw Input глушит LL-хук, пока впереди окно этого же процесса
- **Что:** пока у процесса зарегистрирован Raw Input клавиатуры (любой: `RIDEV_DEVNOTIFY` на своё окно или `RIDEV_INPUTSINK` на скрытое окно сторожа), а впереди окно этого же процесса, Windows не вызывает его `WH_KEYBOARD_LL` вовсе. Окно другого процесса впереди — хук работает. Цикл Slint (winit) при старте регистрирует Raw Input клавиатуры на себя (`winit-0.30.13/src/platform_impl/windows/event_loop.rs:208`, `DeviceEvents::WhenFocused`) и этим заменяет регистрацию сторожа: у процесса она одна на устройство.
- **Источник:** документации нет; пробник `examples/hook_probe.rs` (12.11.2026): Slint-окно впереди — 0 вызовов хука на 6 нажатий; простое окно Win32 без Raw Input — 6; простое окно с Raw Input — 0; Slint-окно после `RIDEV_REMOVE` — 6. Нажатия — `keybd_event` из другого процесса; физическая клавиатура этим пробником не проверялась. [не проверено вживую с физической клавиатуры]
- **Как в Verba:** после старта цикла Slint сторож регистрирует Raw Input заново на своё окно (`HookThread::raw_input(true)`), пока открыто окно настроек — снимает (`raw_input(false)`, `RIDEV_REMOVE`), после закрытия — возвращает. Пока окно открыто, сторож по Raw Input не работает: снятый хук переустановится после разблокировки или сна.
- **Проверено:** с окном настроек впереди Ctrl+Alt+F11 начинает запись, захват сочетания ловит Ctrl+Alt+F7; до починки — ни того, ни другого.

## Порядок хуков: Verba и Handy одновременно
- **Что:** `SetWindowsHookEx` ставит хук в начало цепочки → последний запущенный перехватчик видит клавиши первым.
- **Источник:** [SetWindowsHookExW](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowshookexw).
- **Проверено:** Verba, запущенная после Handy, забирает Ctrl+Space себе. На паузе Verba пропускает Ctrl+Space дальше, и его забирает Handy (лог Handy, 21:49:57: запись без звука). Чтобы заменить Handy, его надо закрыть.

## Маска меню после сочетаний с Alt и Win
- **Что:** хук проглотил P из Ctrl+Alt+P → система видит Alt вниз и Alt вверх без клавиши между ними → открывается меню окна (Win → «Пуск»). Гасится пустым нажатием неназначенной клавиши VK 0xE8 до отпускания модификатора.
- **Источник:** AutoHotkey, `#MenuMaskKey` (тот же приём, VK E8 по умолчанию).
- **Проверено:** Ctrl+Alt+P и Ctrl+Alt+V в окне WinForms меню не открывают. Обратный пример: голое нажатие Alt при показанной форме ввело её в модальный цикл системного меню — тестовый сценарий так и завис.

## Вставка: квитанция чтения и возврат буфера
- **Что:** текст кладётся обещанием (`SetClipboardData(CF_UNICODETEXT, NULL)`). Когда приложение читает буфер, Windows шлёт владельцу `WM_RENDERFORMAT` — это квитанция. После затишья 200 мс (или через 8 с) возвращается прежнее содержимое, если буфер с тех пор не менялся (номер последовательности).
- **Источник:** Handy (MIT) `src-tauri\src\paste_tx\windows.rs`, `paste_tx\mod.rs`; [Clipboard Formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats) — форматы `ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory`, `CanUploadToCloudClipboard`.
- **Проверено:** 15 из 15 вставок, буфер после каждой прежний; приложение читает буфер через 17–25 мс после Ctrl+V.

## SendInput в окно с правами администратора
- **Что:** UIPI блокирует ввод в окно с более высоким уровнем целостности, и ни код возврата, ни `GetLastError` этого не показывают.
- **Источник:** [SendInput](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput), Remarks.
- **Как в Verba:** такую блокировку видно только по отсутствию квитанции чтения → строка в логе «не прочитало буфер за 8 с»; текст остаётся в «Последних диктовках». Запуск Verba с правами — задача T-004. [не проверено вживую: игры с правами на ПК не запускались]

## Автозапуск с правами: задача «Verba (admin)»
Код — `src/autostart.rs`. Образец — задача «Handy (admin)»: Handy её не создаёт (в его исходниках нет ни schtasks, ни Task Scheduler), её заводили руками.

- **Задача:** `LogonTrigger` с `UserId` этого пользователя (без него — вход любого), `Principal`: тот же `UserId`, `InteractiveToken`, `HighestAvailable`; `ExecutionTimeLimit PT0S`, без условий по батарее, `MultipleInstancesPolicy IgnoreNew`, `Priority 4`; запуск `Verba.exe --tray`. `UserId` — SID: планировщик принимает его и в `Principal`, и в триггере (в триггере при чтении показывает `ДОМЕН\имя`). Источник: [Task Scheduler Schema](https://learn.microsoft.com/en-us/windows/win32/taskschd/task-scheduler-schema). Проверено задачей «Verba (test)» с `LeastPrivilege`.
- **Файл для `schtasks /create /xml`:** только UTF-16 LE с BOM. UTF-16 без BOM → «XML-код задачи неверно записан… один корневой элемент», UTF-8 с BOM и без → «неправильный синтаксис» / «не удалось переключить кодировку». Проверено.
- **Создание без прав невозможно:** задача с `HighestAvailable` от процесса без прав → «Отказано в доступе». Поэтому `enable`/`disable` запускают schtasks через `ShellExecuteExW` с глаголом `runas` — одно окно UAC; отказ в окне — `ERROR_CANCELLED` (1223). Verba уже с правами — schtasks напрямую с `CREATE_NO_WINDOW`. Источник: [ShellExecuteExW](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecuteexw) (там же: COM до вызова, STA с `COINIT_DISABLE_OLE1DDE`).
- **Окно консоли:** `ShellExecuteEx` с `SW_HIDE` — ни окна консоли, ни Windows Terminal (терминал по умолчанию здесь — «пусть решает Windows»); проверено с глаголом `open`, весь путь (кавычки, ожидание, код выхода, создание и удаление задачи — 32 мс). С `runas` окно UAC не подтвердить без человека. [не проверено вживую: `runas`]
- **Ответ schtasks без консоли** — в OEM-кодировке (cp866) и на языке системы («ОШИБКА: Отказано в доступе.»); из консоли PowerShell тот же schtasks отвечает по-английски. Verba декодирует `CP_OEMCP`. Код выхода — 1 при любой ошибке. Проверено.
- **Чтение без прав:** задачу, созданную с правами, процесс без прав читает — планировщик даёт пользователю из `Principal` право чтения (`(A;;FR;;;<SID>)` в дескрипторе «Handy (admin)»). `schtasks /query /xml` и COM читают; реестр `TaskCache\Tree` без прав закрыт. Выбран COM `ITaskService`: «задачи нет» — точный код 0x80070002, выключенная задача при запуске — 0x80041326 (`SCHED_E_TASK_DISABLED`), а schtasks на любую ошибку даёт код 1 и переведённый текст. Задачу с пропавшим exe планировщик запускает «успешно», и запуск падает молча (результат 0x80070002) — Verba проверяет файл сама. Проверено.
- **Замена и удаление на ходу:** `schtasks /create … /f` поверх работающей задачи и `schtasks /delete` запущенный ею процесс не трогают — Verba из задачи может включать и выключать автозапуск, не убивая себя. Проверено задачей «Verba (test)» с `conhost --headless ping`.
- **Запуск без прав:** `IRegisteredTask::Run` — тот же механизм, что ярлык Handy `schtasks /run /tn "Handy (admin)"`. Для задачи, созданной без прав, проверено; для задачи с правами — по ярлыку Handy. [не проверено вживую: Run задачи с правами из Verba]
- **Приоритет:** по умолчанию у задачи `Priority 7`. Замер на задаче «Verba (test)»: 7 → класс `BELOW_NORMAL`, приоритет ввода-вывода Low (1), памяти 2; 4 → `NORMAL`, ввод-вывод Normal (2), память 5 — как у процесса, запущенного вручную. У «Handy (admin)» стоит 4 (базовый приоритет процесса 8). Источник: [TaskSettings.Priority](https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings-priority) — «7 и 8 — фоновые задачи, 4–6 — интерактивные».
- **Verba с правами глазами процесса без прав:** `Get-Process … .Path` и `Win32_Process.ExecutablePath` пусты; путь отдаёт `QueryFullProcessImageNameW` с `PROCESS_QUERY_LIMITED_INFORMATION` (так `scripts/release.mjs` находит Verba на рабочем столе). `Stop-Process` её не завершит; WM_CLOSE дойдёт, только если окно трея разрешило его `ChangeWindowMessageFilterEx`. Проверено на Handy из такой же задачи (сообщения ему не слались).
- **Один экземпляр:** мьютекс экземпляра с правами процесс без прав открывает — `CreateMutexW` отдаёт `ERROR_ALREADY_EXISTS` (проба на мьютексе Handy с правами). А `PostMessage` своего сообщения (`WM_SECOND_INSTANCE`) окну с правами UIPI отбросит молча. Источник: [ChangeWindowMessageFilterEx](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-changewindowmessagefilterex). [гипотеза: то же для широковещательного `TaskbarCreated` от Проводника — без фильтра значок трея не вернётся после перезапуска Проводника и при старте раньше панели задач]
- **Риск:** задача запускает с правами то, что лежит по пути exe. Рабочий стол (как и папка Handy в `%LOCALAPPDATA%`) доступен на запись любой программе пользователя → подменённый exe получит права администратора при следующем входе без окна UAC. Защита — exe в папке, куда пишет только администратор (`C:\Program Files\…`).

## Тёмное меню трея
- **Что:** меню Win32 следует тёмной теме только после недокументированных `SetPreferredAppMode(AllowDark)` (uxtheme, ординал 135) и `FlushMenuThemes` (136), Windows 10 1903+.
- **Источник:** проект win32-darkmode (ysc3839); так делают Notepad++ и PowerToys.
- **Как в Verba:** функций нет → меню остаётся светлым, ничего не ломается (`src/tray.rs` `follow_system_theme_for_menus`). [не проверено вживую: снимок меню не делался]

## Значок трея в Windows 11
- Новые значки Windows 11 прячет в скрытую область. Закрепить: «Параметры → Персонализация → Панель задач → Другие значки панели задач → Verba».

## Микрофон: открытие по требованию
- **Замер:** WASAPI через cpal открывает «Микрофон (IO Mike)» за 4–13 мс от нажатия (лог, 3 записи). Значит, в режиме по требованию первые слова не теряются, и держать микрофон открытым по умолчанию незачем: так в простое 0 % CPU и индикатор микрофона Windows не горит.

## Сеть: отказ в соединении на localhost
- **Замер:** подключение к порту, где никто не слушает, на Windows длится ~2 с (повторы SYN после RST), а не мгновенно. Три отправки подряд — ~6 с до ошибки (`tests/openrouter.rs` `nothing_listening_is_unreachable_fast`).

## codex app-server: дочерний процесс без окна и без сирот
- **Что:** Codex из npm — `codex.cmd` → node → `codex.exe`. Rust запускает `.cmd` через `cmd.exe` сам (обработка `.bat`/`.cmd` в `std::process::Command`, исправленная в Rust 1.77.2 после CVE-2024-24576). Verba — оконное приложение: без `CREATE_NO_WINDOW` у cmd.exe появилось бы консольное окно. Убить только cmd.exe мало — node и codex.exe остались бы сиротами.
- **Источник:** [Process Creation Flags](https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags) (`CREATE_NO_WINDOW`); [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects) и [JOBOBJECT_BASIC_LIMIT_INFORMATION](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_limit_information) (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; вложенные задания — с Windows 8).
- **Как в Verba:** процесс сразу после запуска входит в Job Object с KILL_ON_JOB_CLOSE; дети (node, codex.exe) входят в задание сами. Закрытие: stdin закрыт → сервер выходит сам; не вышел за 2 с → `TerminateJobObject`. Verba упала — Windows закрывает хэндл задания, дерево уходит с ним (`src/net/app_server.rs`).
- **Проверено:** фейковый сервер, зависший на `initialize` за `.cmd` (cmd.exe → exe), снят целиком — после тестов 0 процессов; настоящий `codex app-server` (Codex CLI 0.154) закрылся по простою, `codex.exe` не осталось (12.11.2026). Окно консоли глазами не проверялось. [не проверено вживую: мелькание окна]
- **Риск:** между `CreateProcess` и входом в задание проходят микросекунды; cmd.exe запускает node позже, так что гонки нет [гипотеза].

## Вывод CLI оконного приложения в файл
- **Что:** у оконного exe `WriteConsoleW` в консоль родителя (`AttachConsole`) идёт мимо перенаправления: `verba codex check > out.txt` давал пустой файл. Перенаправленный stdout (тип `FILE_TYPE_PIPE` или `FILE_TYPE_DISK` у `GetStdHandle`) — пишем в него.
- **Источник:** [GetFileType](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfiletype), [AttachConsole](https://learn.microsoft.com/en-us/windows/win32/api/wincon/nf-wincon-attachconsole).
- **Проверено:** `verba codex check > файл` из bash — вывод в файле (12.11.2026).

## Оверлей: окно без фокуса, сквозь которое проходит мышь
- **Что:** плашка записи не должна забирать фокус (иначе текст встанет не в то поле) и ловить щелчки. `WS_EX_NOACTIVATE` — окно не становится активным от щелчка; `WS_EX_LAYERED | WS_EX_TRANSPARENT` — форма layered-окна не участвует в попадании мыши, щелчки уходят окну под ним; `WS_EX_TOOLWINDOW` — нет в Alt+Tab и на панели задач. Показ — только `ShowWindow(SW_SHOWNOACTIVATE)`, поверх всех — `SetWindowPos(HWND_TOPMOST, …, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW)`, как Handy поднимает свой оверлей (`handy-098\src-tauri\src\overlay.rs` `force_overlay_topmost`).
- **Источник:** [Extended Window Styles](https://learn.microsoft.com/en-us/windows/win32/winmsg/extended-window-styles), [Layered Windows](https://learn.microsoft.com/en-us/windows/win32/winmsg/window-features#layered-windows), [ShowWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-showwindow), [SetWindowPos](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowpos).
- **Как в Verba:** `src/overlay.rs`; окно и цикл сообщений — в своём потоке, команды — каналом. Монитор — окна на переднем плане (`GetForegroundWindow` → `MonitorFromWindow`), размеры × DPI монитора (`GetDpiForMonitor`), место — по центру `rcWork`, на 48 px × DPI выше панели задач.
- **Проверено:** `examples/overlay_demo.rs` (12.11.2026, монитор 1920×1080, 100 %): во всех состояниях окно на переднем плане то же, что до показа; `WindowFromPoint` в середине плашки возвращает окно под ней; плашка по центру (840 / 840 px), 48 px над панелью задач; после скрытия CPU процесса за 2 с — 0 мс. Ввод текста в поле при видимой плашке живьём не проверялся — проверка T-004 с контроллером. Монитор с масштабом не 100 % — только юнит-тесты размеров. [не проверено вживую]

## Оверлей: альфа кадра и текст GDI
- **Что:** `UpdateLayeredWindow` с `AC_SRC_ALPHA` берёт 32-битный DIB с предумноженной альфой: каждый цветовой канал уже умножен на альфу (канал больше альфы — пересвет). GDI альфу не пишет: замер — `TextOutW` в 32-битный DIB с альфой 255 обнулил альфу у всех 174 пикселей, которых коснулся, — текст на плашке был бы дырой. Поэтому GDI рисует текст белым по чёрному в отдельный DIB (`ANTIALIASED_QUALITY` — сглаживание оттенками серого, без цветной каймы ClearType), яркость пикселя — покрытие, и кадр смешивает цвет текста по нему.
- Появление и уход — прозрачностью окна целиком (`SourceConstantAlpha`), тем же кадром: вызов без `hdcSrc` документация разрешает только с `dwFlags = 0` («If hdcSrc is NULL, dwFlags should be zero»), поэтому кадр подаётся каждый раз (~10 вызовов на уход).
- Шрифт: GDI отдаёт «Segoe UI Variable Text» как семейство (проверено `GetTextFaceW`); нет его — «Segoe UI». Время рисуется по символу в клетки ширины самой широкой цифры — не дрожит.
- **Источник:** [BLENDFUNCTION](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-blendfunction) (AC_SRC_ALPHA: «the APIs use premultiplied alpha»), [UpdateLayeredWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-updatelayeredwindow), [LOGFONTW](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-logfontw) (`ANTIALIASED_QUALITY`); снимки экрана с layered-окнами — `BitBlt` с `CAPTUREBLT` ([BitBlt](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/nf-wingdi-bitblt)).
- **Проверено:** снимки плашки в тёмной и светлой теме (`examples/overlay_demo.rs --shots`) — текст ровный, без дыр и каймы; волна за 210 мс меняется, при «Распознаю…» за 300 мс — ни пикселя.

## Активация окна из тестового сценария
- **Что:** `SetForegroundWindow` из фонового процесса запрещён (foreground lock); работает после `AttachThreadInput` к потоку переднего окна.
- **Источник:** [SetForegroundWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow), [AttachThreadInput](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-attachthreadinput).
- **Проверено:** тестовое окно встаёт вперёд. Приём пригодится T-003 «Вставка в запомненное поле».
- **T-003:** первым приёмом оказалось пустое событие мыши через `SendInput` (PowerToys) — сработало во всех попытках, `AttachThreadInput` остался запасным. Обходы вставки в запомненное поле (foreground lock, `EM_REPLACESEL`, первый запрос UIA к Chrome, протухший RuntimeId) и замеры — `docs/focus-restore.md`.
