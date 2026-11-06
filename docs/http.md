# HTTP-режим

`wiki-mcp serve --http` запускает тот же MCP-сервер не через stdio, а как HTTP-сервис (Streamable HTTP). Он нужен, когда:

- агент работает на машине без системного хранилища (Linux без графической сессии, где нет Keychain и Secret Service), и хранить токен в нём негде;
- MCP-клиент не запускает локальный процесс, а подключается к серверу по адресу;
- один сервер (например, в Docker) обслуживает несколько агентов, у каждого из которых свой токен.

Инструменты, уровни доступа, наборы и вырезание учётных данных из ответов — те же, что у stdio-сервера. Отличается то, откуда берутся токен и организация, и то, что сервер не работает с локальными файлами.

## Токен и организация

В HTTP-режиме **нет профилей и системного хранилища**. Каждый HTTP-запрос агента несёт его токен Wiki и ID организации в заголовках:

```text
Authorization: OAuth <OAuth-токен>
X-Org-Id: <ID организации Yandex 360 для бизнеса>
```

| Заголовок | Значение |
| --- | --- |
| `Authorization: OAuth <токен>` | OAuth-токен Яндекса (как в stdio-режиме). |
| `Authorization: Bearer <токен>` | IAM-токен Yandex Cloud. В Wiki API схема `Bearer` означает именно IAM-токен, поэтому OAuth-токен с ней не подойдёт: API ответит `401`. |
| `X-Org-Id: <ID>` | Организация Yandex 360 для бизнеса. |
| `X-Cloud-Org-Id: <ID>` | Организация Yandex Cloud. |

Имена заголовков и схема в `Authorization` — без учёта регистра. Сервер передаёт в Wiki API ровно ту схему и тот заголовок организации, которые прислал агент.

- Без заголовка `Authorization` или с некорректным заголовком (другая схема, пустой токен, два заголовка) сервер отвечает `401 Unauthorized` с `WWW-Authenticate: OAuth realm="wiki-mcp"` и `WWW-Authenticate: Bearer realm="wiki-mcp"` и не передаёт запрос в MCP.
- Без организации сервер отвечает `400 Bad Request` (`"error": "missing_organization"`). Оба заголовка сразу, повтор заголовка или недопустимый ID (допустимы 1–64 символа из `A-Z`, `a-z`, `0-9`, `-`, `_`) — тоже `400` (`invalid_organization`). Ошибка возвращается на уровне HTTP, а не как ошибка инструмента: это ошибка конфигурации клиента, и она видна сразу при подключении, а не в середине работы агента.
- Организацию можно задать и на сервере: `--org-id <ID>` (Yandex 360) или `--cloud-org-id <ID>` (Yandex Cloud). Тогда запросы могут её не передавать. Сервер с такой настройкой **закреплён** за организацией: запрос с той же организацией принимается, с другой — `403 Forbidden` (`organization_not_allowed`). Так оператор сервера может быть уверен, что через него не работают с другими организациями.
- Токен и организация читаются заново из каждого запроса и используются только для вызовов Wiki API, сделанных в ответ на этот запрос. Сервер не хранит их ни в памяти между запросами, ни на диске, не пишет в логи (даже на уровне `trace`) и не возвращает в ответах и ошибках (если API отразил токен в ответе, он заменяется на `***`).
- Токен уходит только в Wiki API (`https://api.wiki.yandex.net`). Редиректы на другие хосты не выполняются.
- Сервер работает без сессий: каждый запрос обрабатывается независимо, поэтому разные агенты с разными токенами и организациями могут работать с одним сервером одновременно.

### Модель угроз

Токен лежит в конфигурации MCP-клиента, и агент может его прочитать. Поэтому в HTTP-режиме:

- **настоящая граница — права токена в Wiki.** Выдавайте каждому агенту свой токен (своё OAuth-приложение с минимальными правами: `wiki:read`, а `wiki:write` — только если агенту нужна запись). Такой токен можно отозвать, не затрагивая остальных;
- флаги доступа сервера (`--allow-write`, `--allow-destructive`, `--toolsets`) и вырезание учётных данных — **страховка от ошибок агента, а не граница безопасности**: агент, который знает токен, может обратиться к Wiki API и в обход сервера;
- сервер на loopback (по умолчанию) доступен только процессам этой машины. За её пределами токены идут открытым текстом, поэтому снаружи сервер должен быть доступен только через TLS (reverse proxy) или в доверенной сети.

Если важно, чтобы агент не видел токен, используйте stdio-сервер: там токен хранится в системном хранилище и агенту недоступен.

## Отличия от stdio

| | stdio (`serve`) | HTTP (`serve --http`) |
| --- | --- | --- |
| Токен | Системное хранилище, профиль `--profile` | Заголовок `Authorization` каждого запроса |
| Организация | Профиль | Заголовок запроса или `--org-id` / `--cloud-org-id` сервера |
| Настройки доступа | Флаги или настройки профиля | Только флаги командной строки |
| Локальные файлы | В разрешённых каталогах (`--allow-path`) | Выключены |

Локальные файлы выключены, потому что сервер работает не на машине агента: путь из аргумента указывал бы на файл сервера. Поэтому в HTTP-режиме не регистрируются загрузка файла вложением (`upload_attachment`), загрузка части файла (`upload_file_part`) и скачивание вложений (`download_attachment`, `download_attachment_by_url`, `download_attachment_preview`), а у `api_request` нет аргумента `save_to`; вызов такого инструмента по имени возвращает ошибку инструмента. Остальные операции с вложениями (список, удаление, сессии загрузки) доступны.

`--profile`, `--allow-path` и `--status` вместе с `--http` — ошибка запуска, как и флаги HTTP-режима без `--http`. Настройки HTTP-сервера задаются только флагами запуска, поэтому `serve --status` для него не нужен: проверить запущенный сервер можно через `GET /health`.

## Запуск

### Бинарник

```bash
wiki-mcp serve --http
# wiki-mcp: MCP endpoint http://127.0.0.1:8767/mcp
```

| Флаг | По умолчанию | Назначение |
| --- | --- | --- |
| `--http` | выкл. | Режим HTTP вместо stdio. |
| `--listen <IP:порт>` | `127.0.0.1:8767` | Адрес и порт. Адрес не loopback — предупреждение в stderr: нужен TLS. |
| `--allowed-host <имя>` | — | Дополнительное допустимое значение заголовка `Host`: `имя` (любой порт) или `имя:порт`. Можно повторять. |
| `--allowed-origin <origin>` | — | Допустимый `Origin` браузерного клиента: `http://localhost:6274` (без порта — порт схемы по умолчанию) или `http://localhost:*` (любой порт). Можно повторять. |
| `--org-id <ID>` | — | Организация Yandex 360 для всех запросов (`X-Org-Id`); закрепляет сервер за ней. |
| `--cloud-org-id <ID>` | — | Организация Yandex Cloud для всех запросов (`X-Cloud-Org-Id`); закрепляет сервер за ней. |
| `--max-request-bytes <байты>` | `1048576` (1 МиБ) | Предел тела запроса MCP; больший запрос — `413 Payload Too Large`. |

Остальные флаги `serve` работают так же, как в stdio: `--allow-write` (создание и правка), `--allow-destructive` (удаление и изменение прав доступа, включает `--allow-write`), `--toolsets` (наборы инструментов; по умолчанию `pages,attachments,comments,grids`, `all` — все), `--timeout`, `--max-response-bytes`, `--log-level`. Полный список — `wiki-mcp serve --help`.

Порт `8767` выбран так, чтобы не пересекаться с другими MCP-серверами на той же машине. Если он занят, задайте другой: `--listen 127.0.0.1:<порт>`.

Эндпоинты:

- `POST /mcp` — MCP (Streamable HTTP без сессий, ответы `application/json`); нужны токен и организация. Другие методы отвечают `405`.
- `GET /health` — проверка готовности без токена: `{"status":"ok","service":"wiki-mcp",…}`.

Запрос к `/mcp` проверяется по порядку: `Host` и `Origin` (`403`), токен (`401`), организация (`400` или `403`), размер тела (`413`). Ответы с ошибкой — JSON вида `{"error": "...", "message": "..."}` (кроме `413` и ошибок самого протокола MCP); значения заголовков в них не попадают.

Сервер завершается по SIGINT и SIGTERM. Логи пишутся в stderr; уровень — `--log-level`, иначе переменная `RUST_LOG`, иначе `warn`.

### Защита от DNS rebinding

- **`Host`.** Принимаются только `localhost`, `127.0.0.1`, `::1` (с любым портом), конкретный адрес из `--listen` и имена из `--allowed-host`; остальные запросы получают `403` ещё до проверки токена. За reverse proxy добавьте имя, которое proxy передаёт в `Host`.
- **`Origin`.** Запросы без `Origin` (обычные MCP-клиенты, не браузер) принимаются. Запрос из браузера с `Origin` принимается, только если origin указан в `--allowed-origin`; по умолчанию такие запросы отклоняются (`403`).

### Docker

В репозитории есть `Dockerfile`: бинарник собирается в Alpine статически (musl), как бинарники релизов, без системных библиотек. Итоговый образ — минимальный `alpine`, из пакетов в нём только корневые сертификаты (`ca-certificates`) для проверки TLS Wiki API; непривилегированный пользователь (uid `10001`), порт `8767`, `HEALTHCHECK` по `/health` через `wget` из BusyBox. Токенов в образе нет.

```bash
docker build -t wiki-mcp .
docker run -d --name wiki-mcp --restart unless-stopped \
  --read-only --cap-drop ALL --security-opt no-new-privileges:true \
  -p 127.0.0.1:8767:8767 wiki-mcp
curl http://127.0.0.1:8767/health
```

Внутри контейнера сервер слушает `0.0.0.0:8767` (команда по умолчанию `serve --http --listen 0.0.0.0:8767`), поэтому при запуске печатает предупреждение о TLS. Публикуйте порт на loopback хоста (`127.0.0.1:8767:8767`), а не на всех интерфейсах. Флаги добавляются после имени образа, команда при этом задаётся целиком:

```bash
docker run -d -p 127.0.0.1:8767:8767 wiki-mcp \
  serve --http --listen 0.0.0.0:8767 --allow-write --toolsets pages,grids --org-id <ID>
```

Если к серверу обращаются другие контейнеры по имени (`http://wiki-mcp:8767/mcp`), добавьте это имя: `--allowed-host wiki-mcp`.

Если порт `8767` на хосте занят, опубликуйте другой порт хоста (`-p 127.0.0.1:<порт>:8767`): `Host` с любым портом на loopback-именах разрешён.

### Docker Compose

`compose.yaml` в репозитории:

```yaml
services:
  wiki-mcp:
    build: .
    image: wiki-mcp:local
    command:
      - serve
      - --http
      - --listen
      - 0.0.0.0:8767
      - --allowed-host
      - wiki-mcp
    ports:
      - "127.0.0.1:8767:8767"
    environment:
      RUST_LOG: warn
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    restart: unless-stopped
```

```bash
docker compose up --build -d
docker compose logs -f wiki-mcp
```

Флаги доступа и организацию добавляйте в `command`, например `--allow-write` или `--org-id <ID>`.

### systemd

Файл `/etc/systemd/system/wiki-mcp.service` для бинарника `/usr/local/bin/wiki-mcp`:

```ini
[Unit]
Description=Yandex Wiki MCP server (HTTP)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/wiki-mcp serve --http --listen 127.0.0.1:8767
Environment=RUST_LOG=warn
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now wiki-mcp
curl http://127.0.0.1:8767/health
```

Токенов в unit-файле нет: их передают агенты.

### За reverse proxy с TLS

Чтобы агенты с других машин подключались к серверу, оставьте его на loopback и поставьте перед ним reverse proxy с TLS. Пример для nginx:

```nginx
server {
    listen 443 ssl;
    server_name mcp.example.com;
    # ssl_certificate и ssl_certificate_key — ваши сертификаты

    location /mcp {
        proxy_pass http://127.0.0.1:8767;
        proxy_set_header Host $host;
        proxy_http_version 1.1;
        proxy_buffering off;
        proxy_read_timeout 120s;
    }
}
```

Сервер запускается с `--allowed-host mcp.example.com`. Proxy не должен логировать заголовки `Authorization`, `X-Org-Id` и `X-Cloud-Org-Id`.

## Подключение агентов

Каждому агенту — свой токен своего OAuth-приложения: создайте приложение Яндекс OAuth с правами `wiki:read` (и `wiki:write`, если агенту нужна запись) и получите токен по ссылке `https://oauth.yandex.ru/authorize?response_type=token&client_id=<ClientID>` ([OAuth-приложение и токен](oauth.md)). URL в клиенте — эндпоинт `/mcp`, а не `/health`.

### Claude Code

```bash
claude mcp add --transport http wiki http://127.0.0.1:8767/mcp \
  --header "Authorization: OAuth <токен>" \
  --header "X-Org-Id: <ID организации>"
```

Токен сохраняется в конфигурации Claude Code (для `--scope project` — в `.mcp.json` проекта, не коммитьте его). Проверка: `claude mcp list` или `/mcp` в сессии.

### Codex

В `~/.codex/config.toml`:

```toml
[mcp_servers.wiki]
url = "http://127.0.0.1:8767/mcp"
http_headers = { "X-Org-Id" = "<ID организации>" }
env_http_headers = { "Authorization" = "WIKI_AUTHORIZATION" }
```

Переменная `WIKI_AUTHORIZATION` со значением `OAuth <токен>` должна быть задана в окружении, из которого запускается Codex. `bearer_token_env_var` здесь не подходит для OAuth-токена: Codex отправит его как `Bearer`, то есть как IAM-токен. Сам `wiki-mcp` токен из окружения не читает — его отправляет Codex в заголовке.

### Другие клиенты

Укажите адрес `http://<хост>:8767/mcp` (транспорт Streamable HTTP) и заголовки `Authorization` и `X-Org-Id` (или `X-Cloud-Org-Id`). Большинство клиентов принимают такую запись:

```json
{
  "mcpServers": {
    "wiki": {
      "type": "http",
      "url": "http://127.0.0.1:8767/mcp",
      "headers": {
        "Authorization": "OAuth <токен>",
        "X-Org-Id": "<ID организации>"
      }
    }
  }
}
```

Браузерным клиентам (например, MCP Inspector) нужен ещё `--allowed-origin` с их origin, например `--allowed-origin http://localhost:6274`.

## Проблемы

| Симптом | Причина |
| --- | --- |
| `401` и `WWW-Authenticate: OAuth …` | Клиент не отправил `Authorization: OAuth <токен>` (или `Bearer <IAM-токен>`) или отправил его в другом формате. |
| `400` `missing_organization` | Нет ни `X-Org-Id`, ни `X-Cloud-Org-Id`, а сервер не запущен с `--org-id` / `--cloud-org-id`. |
| `400` `invalid_organization` | Оба заголовка организации сразу, повтор заголовка или недопустимый ID. |
| `403` `organization_not_allowed` | Сервер закреплён за другой организацией (`--org-id`, `--cloud-org-id`): уберите заголовок организации из клиента или исправьте его. |
| Инструмент вернул ошибку `HTTP 401` от Wiki | Токен отклонил Wiki: он истёк или отозван (IAM-токен живёт не больше 12 часов), либо OAuth-токен отправлен со схемой `Bearer`. Замените его в конфигурации клиента. |
| Инструмент вернул ошибку `HTTP 403` от Wiki | У пользователя токена нет доступа к организации или странице: проверьте ID и тип организации. |
| `403` `Host header is not allowed` | Клиент или proxy обращается по имени, которого нет в списке: добавьте `--allowed-host`. |
| `403` `Origin header is not allowed` | Запрос из браузера: добавьте его origin в `--allowed-origin`. |
| `413` | Тело запроса больше `--max-request-bytes`. |
| `405` на `/mcp` | Запрос `GET` или `DELETE`: сервер без сессий принимает только `POST`. |
| Клиент пишет `Unexpected content type` | В конфигурации указан `/health` вместо `/mcp`. |
