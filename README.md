# Verba

*Verba volant, scripta manent.*

Диктовка для Windows: держишь хоткей → говоришь → текст встаёт в поле, где был курсор.
Распознавание — OpenRouter (по ключу) или ChatGPT по входу Codex (по подписке).

## Запуск
```powershell
cargo build --release
.\target\release\verba.exe key set --from-handy   # ключ OpenRouter из Handy; или: verba key set (из буфера обмена)
.\target\release\verba.exe                        # значок в трее
```
Handy перед этим закрыть: он тоже ловит Ctrl+Space.

## Хоткеи (по умолчанию)
- **Ctrl+Space** — диктовка; режим `hotkeys.mode`: `hold` (держать), `toggle` (нажал-включил), `hybrid` (короткое нажатие фиксирует запись, долгое — держать).
- **Esc** — отмена записи.
- **Ctrl+Alt+P** — пауза: Ctrl+Space уходит в приложение (игру) нетронутым.
- **Ctrl+Alt+V** — вставить последнее; последняя диктовка не распозналась — отправить её запись снова.

## Файлы
- Настройки: `%APPDATA%\Verba\config.json` (меню трея → «Открыть настройки»; изменения подхватываются на лету).
- Лог: `%LOCALAPPDATA%\Verba\logs\verba.log` — фазы каждой диктовки, без текста и ключей.
- Ключ OpenRouter и пароль прокси — Windows Credential Manager (`Verba/OpenRouter`, `Verba/Proxy`).
- Прокси: `verba proxy set "<строка с сайта>"` или пункт трея «Прокси из буфера обмена».

Обходы ограничений Windows (хук, вставка, буфер обмена) — `docs/windows.md`. Лицензия — MIT (`LICENSE`).
