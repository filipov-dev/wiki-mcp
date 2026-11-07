# Подключение к MCP-клиентам

Сервер работает через stdio: MCP-клиент сам запускает `wiki-mcp serve --profile <профиль>` и общается с ним через stdin и stdout. Токен и организацию сервер берёт из профиля (токен — из системного хранилища), поэтому в конфигурации клиента их нет.

Рекомендуемая схема: **один клиент — один профиль — одно OAuth-приложение и токен**. Тогда токен любого агента можно отозвать или заменить, не трогая остальных, а права настраиваются для каждого клиента отдельно ([OAuth-приложение и токен](oauth.md)).

Если на машине агента нет системного хранилища или клиент подключается к серверу по адресу, используйте [HTTP-режим](http.md#подключение-агентов).

## Claude Code и Codex: connect

```bash
wiki-mcp connect claude-code
wiki-mcp connect codex
```

`connect` по шагам:

1. Помогает создать собственное OAuth-приложение Яндекса и сохраняет его `ClientID` в профиль (только в первый раз).
2. Спрашивает организацию: тип (Yandex 360 или Yandex Cloud) и ID.
3. Открывает ссылку получения токена, принимает токен скрытым вводом и проверяет его запросом `GET /v1/users/me`. Если рабочий токен уже сохранён, предлагает его оставить.
4. Сохраняет токен в системное хранилище, а настройки — в профиль `~/.config/wiki-mcp/profiles.toml` (в Windows — `%APPDATA%\wiki-mcp\profiles.toml`, см. [Windows](#windows)).
5. Показывает, что изменится в конфигурации клиента, и после подтверждения регистрирует сервер `wiki`: в Claude Code — через `claude mcp add`, в Codex — таблицей `[mcp_servers.wiki]` в `~/.codex/config.toml`.

После этого перезапустите клиент. Профиль по умолчанию называется как клиент (`claude-code`, `codex`), имя сервера — `wiki`. Все флаги и подробности — в [справочнике CLI](cli.md#connect).

Федеративный или сервисный аккаунт Yandex Cloud подключается без OAuth-приложения и токена: `connect` с `--iam-source yc` или `--service-account-key <файл>` пропускает шаги 1 и 3, проверяет, что источник выдаёт IAM-токен, и регистрирует сервер; токены сервер получает и обновляет сам ([IAM-токены](iam.md#подключение)).

### Права и наборы

Флаги `connect` сохраняются в профиль и попадают в команду запуска сервера:

```bash
# Только чтение (по умолчанию)
wiki-mcp connect codex

# Создание и изменение страниц, комментариев, таблиц, загрузка файлов
wiki-mcp connect codex --allow-write

# Плюс удаление и изменение прав доступа
wiki-mcp connect codex --allow-destructive

# Дополнительные наборы инструментов
wiki-mcp connect claude-code --toolsets pages,attachments,comments,grids,extra
```

Чтобы поменять права, запустите `connect` ещё раз с нужными флагами: сохранённый токен можно оставить, а флаги доступа и наборы в профиле и в записи клиента перезаписываются. Для записи OAuth-приложению профиля нужно право `wiki:write` ([права приложения](oauth.md#права-приложения)).

С `--scope project` (только Claude Code) сервер регистрируется в `.mcp.json` текущего каталога. Токен при этом остаётся в хранилище того, кто запускал `connect`, а в команде записан абсолютный путь к бинарнику на его машине: другим участникам проекта нужен свой токен под тем же именем профиля (`wiki-mcp auth login --profile <профиль>`) и бинарник по тому же пути.

### Рядом с другим сервером Wiki

`--name` задаёт имя сервера в клиенте. Так `wiki-mcp` можно подключить рядом с другим MCP-сервером Wiki, не трогая его запись, например на пробный период только на чтение ([Переход со старого сервера](migration.md#переключение-агентов)). Запись с тем же именем, которая запускает не `wiki-mcp`, `connect` заменяет только после явного ответа в терминале.

## Другие клиенты: ручное подключение

`connect` настраивает только Claude Code и Codex. Любой клиент, который умеет запускать stdio MCP-серверы, подключается вручную. Ниже — общий порядок, затем особенности [Google Antigravity](#google-antigravity), [Kimi Code](#kimi-code) и [ZCode](#zcode).

1. Сохраните токен и организацию в отдельный профиль:

   ```bash
   wiki-mcp auth login --profile cursor
   ```

   Новый профиль создаётся с доступом только на чтение. Если в профиле нет `client_id`, получите токен по ссылке вашего приложения ([получение токена](oauth.md#получение-токена)).

2. Узнайте абсолютный путь к бинарнику: `command -v wiki-mcp` (в Windows — `(Get-Command wiki-mcp).Source`). Клиенты, запущенные не из терминала, часто не видят `PATH` оболочки.

3. Зарегистрируйте в клиенте stdio-сервер: команда — путь к `wiki-mcp`, аргументы — `serve --profile <профиль>` и нужные флаги. Формат `mcpServers`, который понимает большинство клиентов:

   ```json
   {
     "mcpServers": {
       "wiki": {
         "command": "/opt/homebrew/bin/wiki-mcp",
         "args": ["serve", "--profile", "cursor", "--allow-write"]
       }
     }
   }
   ```

   Переменные окружения и токен в записи не нужны.

4. Перезапустите клиент и проверьте, что у сервера `wiki` появились инструменты.

Если флаги доступа и `--toolsets` в `args` не указаны, сервер берёт их из настроек профиля, а без настроек работает только на чтение с наборами по умолчанию. Проверить, с какими настройками запустится сервер: `wiki-mcp serve --status --profile <профиль>`.

### Ручная регистрация в Claude Code и Codex

Если вы не хотите, чтобы `connect` менял конфигурацию, зарегистрируйте сервер сами после `auth login`:

```bash
claude mcp add --scope user wiki -- "$(command -v wiki-mcp)" serve --profile claude-code
```

```toml
# ~/.codex/config.toml
[mcp_servers.wiki]
command = "/opt/homebrew/bin/wiki-mcp"
args = ["serve", "--profile", "codex"]
```

### Google Antigravity

Профиль: `wiki-mcp auth login --profile antigravity`.

Глобальный конфиг MCP — `~/.gemini/config/mcp_config.json`, общий для Antigravity App, IDE и CLI (версия 2.x). Antigravity IDE 1.x читает `~/.gemini/antigravity/mcp_config.json`. Если не уверены, какой файл действует, откройте его из интерфейса: в IDE — «…» на панели агента → MCP Servers → Manage MCP Servers → View raw config, в App — Customizations → Installed → MCP Servers → Copy Config File Path. Конфиг одного проекта — `<проект>/.agents/mcp_config.json`.

```json
{
  "mcpServers": {
    "wiki": {
      "command": "/opt/homebrew/bin/wiki-mcp",
      "args": ["serve", "--profile", "antigravity"]
    }
  }
}
```

После сохранения нажмите Refresh в Manage MCP Servers (в App — Restart у сервера, в CLI — `/mcp`). Сервер `wiki` должен появиться со списком инструментов.

Antigravity не включает больше 100 инструментов. У `wiki-mcp` их меньше даже со всеми наборами и удалением (61), поэтому сужать наборы ради этого лимита не нужно.

### Kimi Code

Профиль: `wiki-mcp auth login --profile kimi`.

Kimi Code CLI читает серверы из `~/.kimi-code/mcp.json` (или `$KIMI_CODE_HOME/mcp.json`), конфиг одного проекта — `.kimi-code/mcp.json`. Запись можно добавить командой `/mcp-config` в сессии или вписать в файл:

```json
{
  "mcpServers": {
    "wiki": {
      "command": "/opt/homebrew/bin/wiki-mcp",
      "args": ["serve", "--profile", "kimi"]
    }
  }
}
```

Сервер появляется только в новой сессии; проверьте его командой `/mcp`. Инструменты в Kimi называются `mcp__wiki__<инструмент>`, вызовы без постоянного разрешения клиент подтверждает.

Предыдущий клиент, kimi-cli, хранит тот же формат в `~/.kimi/mcp.json` и регистрирует сервер командой:

```bash
kimi mcp add --transport stdio wiki -- "$(command -v wiki-mcp)" serve --profile kimi
```

### ZCode

Профиль: `wiki-mcp auth login --profile zcode`.

Сервер добавляется в Settings → MCP servers → New MCP server: область User (все проекты) или Workspace (текущий проект), имя `wiki`, тип stdio, в `command` — абсолютный путь к `wiki-mcp`, в `args` — `serve --profile zcode`. Проверьте, что переключатель сервера включён.

Панель пишет в `~/.zcode/cli/config.json` (проект — `<проект>/.zcode/config.json`). Серверы там лежат под ключом `mcp.servers`, а не `mcpServers`:

```json
{
  "mcp": {
    "servers": {
      "wiki": {
        "command": "/opt/homebrew/bin/wiki-mcp",
        "args": ["serve", "--profile", "zcode"]
      }
    }
  }
}
```

Импорт серверов из Claude Code и Codex на той же странице перенесёт их запись вместе с профилем: агент ZCode будет работать с токеном другого клиента. Чтобы у ZCode был свой токен, добавляйте запись вручную.

## Windows

`connect`, `auth` и `serve` работают в Windows так же, команды можно запускать из PowerShell, `cmd` или Git Bash. Установка — [Установка: Windows](install.md#windows).

- **Токен** хранится в Windows Credential Manager: общие учётные данные `<профиль>.dev.filipov.wiki-mcp` (Панель управления → Диспетчер учётных данных → Учётные данные Windows). Настройки профилей — в `%APPDATA%\wiki-mcp\profiles.toml`. Credential Manager доступен только в сеансе, вошедшем с паролем: в SSH-сеансе со входом по ключу, в службе или задаче планировщика без сохранённого пароля Windows не даёт хранилища (ошибка «A specified logon session does not exist»). Запускайте `connect`, `auth` и MCP-клиент в обычном сеансе пользователя; для агента на другой машине есть [HTTP-режим](http.md).
- **Claude Code**: `connect claude-code` вызывает `claude mcp add`, поэтому команда `claude` должна быть в `PATH` (подходят и `claude.exe` из установщика Claude Code, и `claude.cmd` из npm).
- **Codex**: `connect codex` пишет в `%USERPROFILE%\.codex\config.toml` (или в `config.toml` каталога из `CODEX_HOME`).
- В конфигурацию клиента записывается полный путь к `wiki-mcp.exe`, из которого запущен `connect`, например `C:\Users\<имя>\AppData\Local\Programs\wiki-mcp\wiki-mcp.exe`. Обновляйте бинарник заменой файла по тому же пути, тогда `connect` повторять не нужно.

Ручная регистрация в Codex в Windows (в TOML обратные косые черты внутри одинарных кавычек не экранируются):

```toml
# %USERPROFILE%\.codex\config.toml
[mcp_servers.wiki]
command = 'C:\Users\<имя>\AppData\Local\Programs\wiki-mcp\wiki-mcp.exe'
args = ["serve", "--profile", "codex"]
```

## Рабочий каталог и локальные файлы

Инструменты, которые загружают или сохраняют файлы (`upload_attachment`, `upload_file_part`, скачивание вложений), работают только с путями внутри рабочего каталога сервера, системного временного каталога и каталогов из `--allow-path`. Рабочий каталог сервера — тот, в котором клиент его запустил (обычно каталог проекта). Если клиент запускает сервер в домашнем каталоге или в корне файловой системы, рабочий каталог в разрешённые не попадает: добавьте нужные каталоги через `--allow-path`. Подробнее — [Безопасность](security.md#локальные-файлы).

## Несколько профилей

Профилей может быть сколько угодно: например, `claude-code` только на чтение в одной организации и `codex` с правом записи в другой. Проверить профиль:

```bash
wiki-mcp auth status --profile codex
```

Команда показывает, сохранён ли токен (не его значение), тип токена, организацию, пользователя токена, клиент, разрешены ли запись и удаление и выбранные наборы.

## Отключение

1. Удалите запись `wiki` из клиента: `claude mcp remove --scope user wiki`, таблицу `[mcp_servers.wiki]` из `~/.codex/config.toml` или запись из конфигурации другого клиента.
2. Удалите токен из хранилища и настройки профиля: `wiki-mcp auth logout --profile codex --forget`.
3. Отзовите токен ([отзыв токена](oauth.md#отзыв-токена)).
