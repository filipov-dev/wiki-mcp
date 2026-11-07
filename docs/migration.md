# Переход со старого HTTP-сервера на TypeScript

До `wiki-mcp` агенты работали с Wiki через предыдущий MCP-сервер на TypeScript (HTTP, Docker). У него было 20 инструментов с префиксом `wiki_`, один токен в переменных окружения сервера на всех агентов, все инструменты записи и удаления были включены всегда, а файлы передавались через каталог на стороне сервера, base64 или одноразовый адрес для `curl`. В `wiki-mcp` инструментов больше (по одному на каждую операцию API), они называются как операции API, запись и удаление включаются явно, а файл загружается с диска одним вызовом.

На этой странице: какие инструменты заменяют старые, чем отличаются аргументы, как проверить новый сервер на тестовой странице и как переключить агентов.

## Таблица соответствия

Все замены входят в наборы по умолчанию (`pages`, `attachments`, `comments`, `grids`), поэтому `--toolsets` для них не нужен. Инструменты записи регистрируются только с `--allow-write`, удаления — с `--allow-destructive` ([уровни доступа](tools.md#уровни-доступа)).

| Предыдущий сервер | Запрос к API | `wiki-mcp` | Уровень | Набор |
| --- | --- | --- | --- | --- |
| `wiki_get_page` | `GET /v1/pages` | `get_page` | чтение | `pages` |
| `wiki_get_page_by_id` | `GET /v1/pages/{idx}` | `get_page_by_id` | чтение | `pages` |
| `wiki_list_descendants` | `GET /v1/pages/descendants` | `list_page_descendants` | чтение | `pages` |
| `wiki_create_page` | `POST /v1/pages` | `create_page` | запись | `pages` |
| `wiki_update_page` | `POST /v1/pages/{idx}` | `update_page` | запись | `pages` |
| `wiki_append_content` | `POST /v1/pages/{idx}/append-content` | `append_page_content` | запись | `pages` |
| `wiki_delete_page` | `DELETE /v1/pages/{idx}` | `delete_page` | удаление | `pages` |
| `wiki_list_attachments` | `GET /v1/pages/{idx}/attachments` | `list_attachments` | чтение | `attachments` |
| `wiki_download_attachment_by_id` | `GET /v1/pages/{idx}/attachments/{file_id}/download` | `download_attachment` | чтение | `attachments` |
| `wiki_download_attachment_by_slug` | `GET /v1/pages/attachments/download_by_url` | `download_attachment_by_url` | чтение | `attachments` |
| `wiki_attach_uploaded_files` | `POST /v1/pages/{idx}/attachments` | `attach_uploaded_files` | запись | `attachments` |
| `wiki_delete_attachment` | `DELETE /v1/pages/{idx}/attachments/{file_id}` | `delete_attachment` | удаление | `attachments` |
| `wiki_create_upload_session` | `POST /v1/upload_sessions` | `create_upload_session` | запись | `attachments` |
| `wiki_get_upload_session` | `GET /v1/upload_sessions/{session_id}` | `get_upload_session` | чтение | `attachments` |
| `wiki_upload_file_part` | `PUT /v1/upload_sessions/{session_id}/upload_part` | `upload_file_part` | запись | `attachments` |
| `wiki_finish_upload_session` | `POST /v1/upload_sessions/{session_id}/finish` | `finish_upload_session` | запись | `attachments` |
| `wiki_abort_upload_session` | `POST /v1/upload_sessions/{session_id}/abort` | `abort_upload_session` | запись | `attachments` |
| `wiki_abort_all_upload_sessions` | `POST /v1/upload_sessions/abort_active_uploads` | `abort_all_upload_sessions` | запись | `attachments` |
| `wiki_create_file_ingress` | — (приём файла самим сервером) | `upload_attachment` (stdio), `create_upload_url` (HTTP) | запись | `attachments` |
| `wiki_raw_request` | любой путь `/v1/…` | `api_request` | по операции | всегда |

`wiki_create_file_ingress` выдавал одноразовый адрес, на который агент отправлял файл `curl`-ом, чтобы сервер в Docker получил его в свой каталог. `wiki-mcp` в режиме stdio работает на машине агента и читает файл с диска сам: `upload_attachment` загружает и прикрепляет его одним вызовом. В [HTTP-режиме](http.md#загрузка-файлов) локальных файлов нет, а одноразовый адрес остался: `create_upload_url` выдаёт его вместе с командой `curl`, и сервер, приняв файл, сам загружает его в Wiki и прикрепляет к странице — каталога на сервере и пяти вызовов сессии загрузки больше нет. В отличие от старого адреса, новый работает только с тем же заголовком `Authorization` и организацией, что и MCP-клиент. Скачивание вложений в HTTP-режиме по-прежнему недоступно.

Кроме замен, агенту доступны инструменты, которых у предыдущего сервера не было: подстраницы по id (`list_page_descendants_by_id`), поиск (`search`), копирование и восстановление страниц (`clone_page`, `recover_page`), ресурсы и таблицы страницы (`list_page_resources`, `list_page_grids`), комментарии (`list_comments`, `create_comment`), динамические таблицы (`get_grid`, `add_grid_rows`, `update_grid_cells` и другие), текущий пользователь (`get_current_user`), ожидание фоновых операций (`wait_for_operation`); в выключенных по умолчанию наборах — права доступа к страницам (`access`) и операции, которых нет в справочнике (`extra`: перенос страниц `move_page`, история версий, обратные ссылки, пространства). Найти нужную помогает `api_search`.

## Отличия аргументов

Общие правила:

- **Id страницы** — `idx` (раньше `id` или `page_id`). Slug, как и раньше, — `slug`: путь страницы от корня Wiki без домена, например `team/project`.
- **Тело запроса** — аргумент `body` (JSON-объект). Раньше поля страницы, сессии загрузки и вложений передавались аргументами верхнего уровня; теперь всё, что уходит в тело запроса, лежит в `body`. Параметры строки запроса (`fields`, `is_silent`, `allow_merge`, `cursor`, `page_size`) остаются аргументами верхнего уровня.
- **`fields`** — строка через запятую (`"content,breadcrumbs"`), а не массив. Кроме `content`, `attributes`, `breadcrumbs` и `redirect` можно запросить `owner`, `access_policy`, `access_lists` и `actuality`.
- **Неизвестный аргумент** — ошибка до отправки запроса: сообщение перечисляет допустимые аргументы. Поля внутри `body` схемой не ограничиваются и уходят в Wiki как есть.
- **Файлы** — только пути к локальным файлам в [разрешённых каталогах](security.md#локальные-файлы): `file` для загрузки, `save_to` для скачивания. Base64 в аргументах и в ответах больше нет.

По инструментам:

| Инструмент | Было | Стало |
| --- | --- | --- |
| `get_page` | `slug`, `fields` (массив), `raise_on_redirect`, `revision_id` | Те же аргументы, `fields` — строка. |
| `get_page_by_id` | `id`, `fields` (массив), `raise_on_redirect`, `revision_id` | `idx`, `fields` (строка), `raise_on_redirect`, `revision_id`. |
| `list_page_descendants` | `slug` (по умолчанию `homepage`), `fields` | `slug` обязателен (пустая строка — страницы от корня Wiki), `fields` нет. Новые: `page_size` (до 100), `cursor` (`next_cursor` из ответа), `include_self`, `actuality`, `show_all`. Тот же список по id страницы — `list_page_descendants_by_id`. |
| `create_page` | `title`, `slug`, `content`, `page_type`, `grid_format`, `cloud_page` верхнего уровня; `fields`, `is_silent` | `body: {"title", "slug", "content", "access_policy"}` (`title` и `slug` обязательны); `fields`, `is_silent` верхнего уровня. Полей `page_type`, `grid_format` и `cloud_page` в схеме тела официальной спецификации нет. |
| `update_page` | `id`; `title`, `content`, `redirect` верхнего уровня; `fields`, `allow_merge`, `is_silent` | `idx`; `body` с полями, которые меняются: `title`, `content` (заменяет весь текст), `redirect`, `owner`, `access_policy`, `actuality` и `revision` — версия, на которой основано изменение (при несовпадении — конфликт); `fields`, `allow_merge`, `is_silent` верхнего уровня. |
| `append_page_content` | `id`, `content`, `target` (`body`, `section`, `anchor`), `location`, `section_id`, `anchor_name`, `anchor_fallback`, `anchor_regex`; `fields`, `is_silent` | `idx`; `body: {"content": "…"}` и ровно одно место вставки: `"body": {"location": "top"\|"bottom"}`, `"section": {"id": …, "location": …}` или `"anchor": {"name": …, "fallback": …, "regex": …}`; `revision` в `body`; `fields`, `is_silent` верхнего уровня. |
| `delete_page` | `id` | `idx`; `recursive` — вместе с подстраницами. Ответ содержит `recovery_token`: по нему страницу восстанавливает `recover_page`. |
| `list_attachments` | `page_id`, `cursor`, `order_by`, `order_direction`, `page_size` | `idx` и те же параметры выдачи. |
| `download_attachment` | `page_id`, `file_id`, `destination` (`base64` или `path` с `overwrite`) | `idx`, `file_id`, `save_to` — путь к новому файлу. Существующий файл не перезаписывается, base64 нет. |
| `download_attachment_by_url` | `slug`, `file_name`, `destination` | `url` — `slug/.files/имя_файла` без косой черты в начале (поле `download_url` из `list_attachments` без первого `/`), `download`, `save_to`. |
| `attach_uploaded_files` | `page_id`, `upload_sessions` | `idx`, `body: {"upload_sessions": ["<session_id>", …]}`. |
| `delete_attachment` | `page_id`, `file_id` | `idx`, `file_id`. |
| `create_upload_session` | `file_name`, `file_size` | `body: {"file_name": "…", "file_size": <байты>}`. |
| `get_upload_session`, `finish_upload_session`, `abort_upload_session` | `session_id` | Без изменений. |
| `upload_file_part` | `session_id`, `part_number`, `is_last_part`, `source` (`base64` или `path` с `offset` и `length` внутри каталога сервера) | `session_id`, `part_number`, `file` — путь к локальному файлу, содержимое которого целиком и есть эта часть (от 1 байта до 16 МиБ). `is_last_part`, `offset` и `length` нет: файл больше 16 МиБ загружайте через `upload_attachment`, он режет его на части сам. |
| `abort_all_upload_sessions` | — | Без аргументов. Предыдущий сервер обращался к пути `/upload_sessions/abort_all`; в спецификации Wiki API путь — `/v1/upload_sessions/abort_active_uploads`. |
| `upload_attachment` | `wiki_create_file_ingress` (`file_name`, `file_size`), затем `curl --upload-file` и пять вызовов сессии загрузки | `idx`, `file` — путь к локальному файлу, `file_name` — имя вложения (по умолчанию имя файла). Один вызов: сессия, части, завершение, прикрепление; при ошибке сессия отменяется. |
| `create_upload_url` (HTTP-режим) | `wiki_create_file_ingress` (`file_name`, `file_size`), затем `curl --upload-file` и пять вызовов сессии загрузки | `idx`, `file_name`, `file_size`, `expires_in_seconds`. Затем `curl` из ответа (с заголовком `Authorization` клиента); сервер сам создаёт сессию, загружает части и прикрепляет файл, ответ `curl` — вложение с `download_url`. |
| `api_request` | `method`, `path` относительно `/v1`, `query` (только скаляры), `body` | `method`, `path` с версией (`/v1/pages/42/comments`), `query` (массив — повторением ключа), `body`, `save_to` для скачивания файлов. Уровень доступа определяется по операции, с которой совпал путь ([подробнее](tools.md#api_search-и-api_request)). |

### Загрузка файла

Раньше агент делал до семи вызовов: клал файл в каталог сервера (или получал адрес `wiki_create_file_ingress` и отправлял файл `curl`-ом), создавал сессию, загружал части с `is_last_part`, завершал сессию и прикреплял файл. Теперь:

```json
upload_attachment {"idx": 42, "file": "reports/q3.pdf"}
```

Путь — абсолютный или относительный от рабочего каталога сервера (обычно каталога проекта), внутри разрешённых каталогов. Файл не читается в контекст агента. В HTTP-режиме вместо этого — `create_upload_url {"idx": 42, "file_name": "q3.pdf", "file_size": <байты>}` и команда `curl` из ответа ([загрузка файлов](http.md#загрузка-файлов)). Ответ — ответ `attach_uploaded_files` с `download_url` вложения и сведения о загрузке. Как и раньше, вложение не появляется в тексте страницы само: чтобы показать его, допишите `{% file src="<download_url>" name="<имя>" %}` через `append_page_content`. Ручной порядок (`create_upload_session` → `upload_file_part` → `finish_upload_session` → `attach_uploaded_files`) остаётся для случаев, когда нужен контроль над частями.

## Что изменилось по поведению

- **Только чтение по умолчанию.** У предыдущего сервера инструменты записи и удаления, а также `wiki_raw_request` с любым методом были доступны всегда. `wiki-mcp` без флагов регистрирует только чтение; агенту, который создаёт и меняет страницы, нужен `--allow-write`, а удаление (страниц, вложений, комментариев, таблиц; `DELETE` через `api_request`) и изменение прав доступа — `--allow-destructive`.
- **Аннотации.** Отмена сессий загрузки (`abort_upload_session`, `abort_all_upload_sessions`) раньше была помечена как разрушающая; теперь это уровень записи: она ничего не удаляет из Wiki. Разрушающими считаются удаление и изменение прав доступа.
- **Токен.** Раньше один токен лежал в переменных окружения сервера и был общим для всех агентов. Теперь у каждого агента свой профиль и токен в системном хранилище ([HTTP-режим](http.md) — токен в заголовке запроса). Токен выдаёт ваше собственное OAuth-приложение ([OAuth-приложение и токен](oauth.md)).
- **Файлы.** Раньше файлы жили в каталоге сервера (или передавались в base64), и агент, у которого не было доступа к этому каталогу, отправлял файл на одноразовый адрес. Теперь stdio-сервер работает на машине агента и читает и пишет файлы только в [разрешённых каталогах](security.md#локальные-файлы); скачивание не перезаписывает существующие файлы. HTTP-сервер принимает файл на одноразовый адрес (`create_upload_url`) и сразу загружает его в Wiki, ничего не сохраняя у себя.
- **Постраничная выдача** — как в API: `cursor` и `page_size` в аргументах, `next_cursor` в ответе.
- **Ответы.** Ответ больше `--max-response-bytes` (256 КиБ по умолчанию) возвращается ошибкой с подсказкой сузить запрос, а не обрезается. Значения, похожие на учётные данные, в ответах заменяются на `[redacted]` ([подробнее](security.md#учётные-данные-в-ответах)).
- **Ошибки Wiki** (`4xx`, `5xx`) возвращаются агенту как ошибка инструмента с HTTP-статусом и телом ошибки, сервер продолжает работать.

## Проверка на тестовой странице

Перед переключением проверьте новый сервер на живом API. Записывающие шаги выполняйте под отдельной тестовой страницей: ниже её slug обозначен `<TEST_SLUG>`, её id — `<TEST_ID>`, id созданной страницы — `<NEW_ID>`. В Claude Code инструменты сервера `wiki` вызываются как `mcp__wiki__<инструмент>`.

### Только чтение

Сервер по умолчанию: `wiki-mcp connect <клиент>` без флагов.

| # | Вызов | Ожидается |
| --- | --- | --- |
| 1 | `get_current_user` `{}` | Пользователь токена. |
| 2 | `api_search` `{"query": "загрузить файл"}` | `upload_attachment` в `composite_tools` с подсказкой `--allow-write`. |
| 3 | `get_page` `{"slug": "<TEST_SLUG>", "fields": "content,breadcrumbs"}` | Страница с текстом и `id` (это `<TEST_ID>`). |
| 4 | `get_page_by_id` `{"idx": <TEST_ID>, "fields": "attributes"}` | Та же страница. |
| 5 | `list_page_descendants` `{"slug": "<TEST_SLUG>", "page_size": 5}` | Подстраницы, `next_cursor`, если их больше пяти. |
| 6 | `list_attachments` `{"idx": <TEST_ID>}` | Список вложений (может быть пустым). |
| 7 | `api_request` `{"method": "POST", "path": "/v1/search", "body": {"query": "<слово со страницы>"}}` | Результаты поиска: читающий `POST` разрешён. |
| 8 | `create_page` `{"body": {"title": "x", "slug": "<TEST_SLUG>/x"}}` | Ошибка инструмента: нужен `--allow-write`. Страница не создана. |
| 9 | `api_request` `{"method": "POST", "path": "/v1/pages", "body": {"title": "x", "slug": "<TEST_SLUG>/x"}}` | Ошибка: запрос выше уровня сервера не отправляется. |

### Запись

Сервер с `--allow-write`: `wiki-mcp connect <клиент> --allow-write`. Для шагов с файлами положите в рабочий каталог сервера (обычно каталог проекта) файл `wiki-mcp-check.txt`.

| # | Вызов | Ожидается |
| --- | --- | --- |
| 10 | `create_page` `{"body": {"title": "wiki-mcp: проверка", "slug": "<TEST_SLUG>/wiki-mcp-check", "content": "Создано при проверке **wiki-mcp**"}}` | Новая страница `<NEW_ID>`. |
| 11 | `update_page` `{"idx": <NEW_ID>, "body": {"title": "wiki-mcp: проверка (изменено)"}}` | Новое название, текст не изменился. |
| 12 | `append_page_content` `{"idx": <NEW_ID>, "body": {"content": "Дописано в конец", "body": {"location": "bottom"}}}` | Текст дописан в конец страницы. |
| 13 | `upload_attachment` `{"idx": <NEW_ID>, "file": "wiki-mcp-check.txt"}` | Вложение с `download_url`. |
| 14 | `append_page_content` `{"idx": <NEW_ID>, "body": {"content": "{% file src=\"<download_url>\" name=\"wiki-mcp-check.txt\" %}", "body": {"location": "bottom"}}}` | Файл виден в тексте страницы. |
| 15 | `download_attachment` `{"idx": <NEW_ID>, "file_id": <id из list_attachments>, "save_to": "wiki-mcp-check.downloaded.txt"}` | Файл сохранён, содержимое совпадает. |
| 16 | `upload_attachment` `{"idx": <NEW_ID>, "file": "/etc/hosts"}` | Ошибка: путь вне разрешённых каталогов. |
| 17 | `create_comment` `{"idx": <NEW_ID>, "body": {"body": "Комментарий wiki-mcp"}}` | Комментарий на странице. |
| 18 | `clone_page` `{"idx": <NEW_ID>, "body": {"target": "<TEST_SLUG>/wiki-mcp-check-copy"}}`, затем `wait_for_operation` с `operation.type` и `operation.id` из ответа | Статус `success`, копия страницы. |
| 19 | `delete_page` `{"idx": <NEW_ID>}` | Ошибка: нужен `--allow-destructive`. |

Точную схему тела любого шага покажет `api_search` с `tool_name`, например `{"tool_name": "create_comment"}`.

### Удаление

Необязательно, сервер с `--allow-destructive`: `delete_attachment`, `delete_comment` и `delete_page` на объектах из шагов выше (копию из шага 18 — тоже), затем `recover_page` с `recovery_token` из ответа `delete_page`, чтобы проверить восстановление, и снова `delete_page`.

### HTTP-режим

Если агенты будут работать с [HTTP-сервером](http.md), повторите шаги 1, 3, 5, 7 и 10–12 на нём и проверьте, что `upload_attachment`, `upload_file_part` и `download_attachment` не зарегистрированы, а вызов по имени возвращает ошибку о локальных файлах.

## Переключение агентов

### Пробный период

Подключите `wiki-mcp` рядом с предыдущим сервером, под другим именем и только на чтение, с отдельным OAuth-приложением только с `wiki:read` ([OAuth-приложение и токен](oauth.md)):

```bash
wiki-mcp connect claude-code      # имя сервера по умолчанию — wiki
wiki-mcp connect codex
```

Предыдущий сервер остаётся зарегистрирован под своим именем (например, `yandex_wiki_mcp`), и агенты пишут через него. Новым сервером агенты читают: страницы, подстраницы, вложения, комментарии и таблицы. Попросите агентов в этот период пользоваться новым сервером для чтения и сообщать о расхождениях.

### Переключение

1. Сохраните запись предыдущего сервера, чтобы её можно было вернуть: `claude mcp get <старое имя>` (адрес и заголовки) и таблицу `[mcp_servers.<старое имя>]` из `~/.codex/config.toml`.
2. Перерегистрируйте `wiki-mcp` с нужными правами: `wiki-mcp connect <клиент> --allow-write` (повторный `connect` оставит сохранённый токен). Если OAuth-приложение профиля создано только с `wiki:read`, добавьте ему право `wiki:write` и получите новый токен ([права приложения](oauth.md#права-приложения)). Агентам, которые только читают, оставьте сервер и токен только на чтение.
3. Удалите регистрацию предыдущего сервера: `claude mcp remove --scope user <старое имя>`, таблицу `[mcp_servers.<старое имя>]` в `~/.codex/config.toml`.
4. Обновите инструкции агентов и разрешения клиентов:
   - имена инструментов — по [таблице соответствия](#таблица-соответствия), аргументы — по [отличиям](#отличия-аргументов);
   - полные имена в Claude Code: `mcp__<старое имя>__wiki_get_page` → `mcp__wiki__get_page`; разрешения в настройках Claude Code (`permissions.allow`) — так же, либо одной записью `mcp__wiki` на все инструменты сервера;
   - порядок загрузки файла — один вызов `upload_attachment` вместо каталога сервера, `wiki_create_file_ingress` и пяти вызовов сессии загрузки;
   - обходные пути, которые были нужны из-за ограничений предыдущего сервера (например, `wiki_raw_request` для комментариев, таблиц и подстраниц с `page_size`), замените отдельными инструментами.
5. Перезапустите клиенты.

Чтобы вызовы в инструкциях не пришлось менять, можно зарегистрировать `wiki-mcp` под старым именем (`connect … --name <старое имя>`; запись предыдущего сервера заменяется только после подтверждения в терминале). Но имена инструментов и аргументы всё равно другие, поэтому инструкции обновлять нужно в любом случае, а отдельное имя проще отличить при откате.

### Откат

Верните регистрацию предыдущего сервера из записи, сохранённой на шаге 1 (`claude mcp add --transport http <старое имя> <адрес> --header …`, таблица в `~/.codex/config.toml`), и инструкции агентов. Регистрацию `wiki-mcp` можно оставить на чтение (`wiki-mcp connect <клиент>` без флагов) или удалить: `claude mcp remove --scope user wiki`, таблица `[mcp_servers.wiki]` в Codex.
