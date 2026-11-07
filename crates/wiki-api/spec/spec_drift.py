#!/usr/bin/env python3
"""Не отстал ли снимок официальной спецификации Wiki API от самого API.

    python3 crates/wiki-api/spec/spec_drift.py [--spec <файл>] [--llms <файл> | --skip-llms]

Скачивает официальную спецификацию https://api.wiki.yandex.net/v1/openapi.json
и оглавление документации https://yandex.ru/support/wiki/ru/llms.txt (обе
публичные, токен не нужен) и печатает в stdout, чем они отличаются от файлов
в репозитории:

- операции официальной спецификации относительно снимка wiki.upstream.json:
  новые (без записи в overlay.json — отдельным разделом: без неё сборка
  спецификации упадёт), удалённые и изменённые (параметры, тело, ответы,
  описания, схемы из components, на которые ссылается операция);
- схемы components и разделы спецификации вне операций (info, servers и т. п.);
- страницы операций справочника Wiki API из llms.txt относительно ссылок
  externalDocs.url в overlay.json: новые страницы, на которые не ссылается ни
  одна операция, и страницы из дополнения, которых в справочнике больше нет.

Без изменений stdout пуст. Код выхода: 0 — изменений нет, 3 — есть
расхождения, 1 — источник недоступен или отдал не то, что ожидается,
2 — неверные аргументы. --spec и --llms берут уже скачанные файлы вместо
сети; --skip-llms не сравнивает страницы справочника. Файлы репозитория
скрипт не меняет; как обновить снимок — README.md рядом.

Только стандартная библиотека Python 3.10+.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
UPSTREAM = HERE / "wiki.upstream.json"
OVERLAY = HERE / "overlay.json"

SPEC_URL = "https://api.wiki.yandex.net/v1/openapi.json"
LLMS_URL = "https://yandex.ru/support/wiki/ru/llms.txt"
# Страницы справочника API; ссылки в llms.txt — на markdown-версии (.md).
DOCS_BASE = "https://yandex.ru/support/wiki/ru/api-ref/"

METHODS = ("get", "post", "put", "patch", "delete")
EXIT_OK, EXIT_ERROR, EXIT_DRIFT = 0, 1, 3

# Поля операции в отчёте об изменениях.
FIELD_NAMES = {
    "operationId": "operationId",
    "summary": "заголовок",
    "description": "описание",
    "tags": "теги",
    "parameters": "параметры",
    "requestBody": "тело запроса",
    "responses": "ответы",
    "security": "авторизация",
    "deprecated": "deprecated",
}


class SourceError(Exception):
    """Источник недоступен или отдал не то, что ожидается."""


def fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "wiki-spec-drift"})
    with urllib.request.urlopen(req, timeout=60) as resp:
        return resp.read().decode("utf-8")


def read_source(path: str | None, url: str) -> str:
    """Файл, если он задан, иначе — скачанный адрес."""
    if path:
        return Path(path).read_text(encoding="utf-8")
    return fetch(url)


def parse_spec(text: str) -> dict:
    try:
        spec = json.loads(text)
    except ValueError as err:
        raise SourceError(f"спецификация — не JSON: {err}") from err
    # Вместо спецификации могла прийти страница ошибки или пустой объект: пустые
    # paths выглядели бы как удаление всех операций.
    if not isinstance(spec, dict) or not str(spec.get("openapi", "")).startswith("3."):
        raise SourceError("ответ не похож на спецификацию OpenAPI 3")
    if not isinstance(spec.get("paths"), dict) or not spec["paths"]:
        raise SourceError("в спецификации нет операций (paths)")
    return spec


def normalize_page(url: str) -> str:
    """Адрес страницы без .md, якоря, параметров и конечной косой черты."""
    url = url.split("#", 1)[0].split("?", 1)[0].rstrip("/")
    return url.removesuffix(".md")


def is_operation_page(url: str) -> bool:
    """Страница операции: <раздел>/<имя>, не обзор раздела и не общие страницы."""
    if not url.startswith(DOCS_BASE):
        return False
    rest = url[len(DOCS_BASE) :]
    parts = rest.split("/")
    return len(parts) == 2 and all(parts) and parts[1] != "index"


def parse_llms(text: str) -> set[str]:
    urls = re.findall(re.escape(DOCS_BASE) + r"[^\s)\]]+\.md", text)
    if not urls:
        # Вместо оглавления пришло что-то другое (капча, страница ошибки).
        raise SourceError("в llms.txt нет страниц справочника API")
    pages = {normalize_page(url) for url in urls}
    return {page for page in pages if is_operation_page(page)}


def operations(spec: dict) -> dict[str, dict]:
    """Операции по ключу «МЕТОД путь», как в overlay.json."""
    out = {}
    for path, item in spec.get("paths", {}).items():
        if not isinstance(item, dict):
            continue
        for method in METHODS:
            op = item.get(method)
            if isinstance(op, dict):
                out[f"{method.upper()} {path}"] = op
    return out


def components(spec: dict) -> dict[str, object]:
    """Компоненты по ключу «раздел/имя», например «schemas/PageSchema»."""
    out = {}
    for section, items in spec.get("components", {}).items():
        if isinstance(items, dict):
            for name, value in items.items():
                out[f"{section}/{name}"] = value
    return out


def refs(value: object, comps: dict[str, object]) -> set[str]:
    """Компоненты, на которые value ссылается через $ref, вместе с вложенными."""
    found: set[str] = set()
    stack = [value]
    while stack:
        node = stack.pop()
        if isinstance(node, dict):
            ref = node.get("$ref")
            if isinstance(ref, str) and ref.startswith("#/components/"):
                key = ref[len("#/components/") :]
                if key not in found:
                    found.add(key)
                    if key in comps:
                        stack.append(comps[key])
            stack.extend(node.values())
        elif isinstance(node, list):
            stack.extend(node)
    return found


def param_key(param: dict) -> str:
    if "$ref" in param:
        return "ref " + str(param["$ref"]).rsplit("/", 1)[-1]
    return f"{param.get('in')} {param.get('name')}"


def param_changes(old: list, new: list) -> list[str]:
    a = {param_key(p): p for p in old if isinstance(p, dict)}
    b = {param_key(p): p for p in new if isinstance(p, dict)}
    out = []
    for key in sorted(set(a) | set(b)):
        if key not in a:
            required = ", обязательный" if b[key].get("required") else ""
            out.append(f"+ {key}{required}")
        elif key not in b:
            out.append(f"- {key}")
        elif a[key] != b[key]:
            what = [
                label
                for field, label in (("required", "обязательность"), ("schema", "схема"), ("description", "описание"))
                if a[key].get(field) != b[key].get(field)
            ]
            others = set(a[key]) | set(b[key])
            if any(a[key].get(f) != b[key].get(f) for f in others - {"required", "schema", "description"}):
                what.append("другое")
            out.append(f"~ {key}: {', '.join(what)}")
    return out


def op_title(key: str, op: dict) -> str:
    summary = str(op.get("summary", "")).strip()
    title = f"{key} ({op.get('operationId', '?')})"
    return f"{title} — {summary}" if summary else title


def drift(old: dict, new: dict, overlay: dict, llms_pages: set[str] | None) -> list[str]:
    """Отчёт о расхождениях; пустой список — изменений нет."""
    lines: list[str] = []

    def section(title: str, items: list[str]) -> None:
        if items:
            lines.append(f"{title} ({len(items)}):")
            lines.extend(f"  {item}" for item in items)

    patches = overlay.get("operations", {})
    old_ops, new_ops = operations(old), operations(new)
    old_comps, new_comps = components(old), components(new)
    changed_comps = {
        key for key in set(old_comps) | set(new_comps) if old_comps.get(key) != new_comps.get(key)
    }

    added = sorted(new_ops.keys() - old_ops.keys())
    # Без записи в дополнении build_openapi.py не соберёт спецификацию из нового
    # снимка: такие операции требуют решения (имя, набор, уровень доступа).
    section(
        "Новые операции без записи в overlay.json",
        [f"+ {op_title(k, new_ops[k])}" for k in added if k not in patches],
    )
    section(
        "Новые операции (запись в overlay.json уже есть)",
        [f"+ {op_title(k, new_ops[k])}" for k in added if k in patches],
    )

    removed = []
    for key in sorted(old_ops.keys() - new_ops.keys()):
        name = patches.get(key, {}).get("name")
        note = f"; инструмент {name}" if name else ""
        removed.append(f"- {key} ({old_ops[key].get('operationId', '?')}{note})")
    section("Удалённые операции", removed)

    changed = []
    for key in sorted(old_ops.keys() & new_ops.keys()):
        a, b = old_ops[key], new_ops[key]
        what = []
        for field in sorted(set(a) | set(b)):
            if a.get(field) == b.get(field):
                continue
            label = FIELD_NAMES.get(field, field)
            if field == "parameters":
                label += " (" + "; ".join(param_changes(a.get(field) or [], b.get(field) or [])) + ")"
            elif field == "operationId":
                label += f" {a.get(field)} -> {b.get(field)}"
            what.append(label)
        schemas = sorted((refs(a, old_comps) | refs(b, new_comps)) & changed_comps)
        if schemas:
            what.append("схемы: " + ", ".join(s.rsplit("/", 1)[-1] for s in schemas))
        if what:
            name = patches.get(key, {}).get("name", b.get("operationId", "?"))
            changed.append(f"~ {key} ({name}): " + ", ".join(what))
    section("Изменённые операции", changed)

    comp_lines = []
    for key in sorted(changed_comps):
        mark = "+" if key not in old_comps else "-" if key not in new_comps else "~"
        comp_lines.append(f"{mark} components/{key}")
    section("Компоненты", comp_lines)

    rest = sorted(
        k for k in (set(old) | set(new)) - {"paths", "components"} if old.get(k) != new.get(k)
    )
    section("Изменены разделы вне операций", rest)

    if llms_pages is not None:
        linked: dict[str, list[str]] = {}
        for key, patch in patches.items():
            url = patch.get("externalDocs", {}).get("url", "")
            page = normalize_page(str(url))
            if is_operation_page(page):
                linked.setdefault(page, []).append(patch.get("name", key))
        section(
            "Новые страницы справочника, на которые не ссылается ни одна операция overlay.json",
            [f"+ {page}" for page in sorted(llms_pages - linked.keys())],
        )
        section(
            "Страницы из overlay.json, которых нет в llms.txt",
            [f"- {page} ({', '.join(sorted(linked[page]))})" for page in sorted(linked.keys() - llms_pages)],
        )
    return lines


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        epilog=(
            f"Код выхода: {EXIT_OK} — изменений нет, {EXIT_DRIFT} — есть расхождения, "
            f"{EXIT_ERROR} — источник недоступен или не разобрался."
        ),
    )
    parser.add_argument("--spec", help=f"файл официальной спецификации вместо {SPEC_URL}")
    parser.add_argument("--llms", help=f"файл llms.txt вместо {LLMS_URL}")
    parser.add_argument("--skip-llms", action="store_true", help="не сравнивать страницы справочника")
    args = parser.parse_args()
    if args.llms and args.skip_llms:
        parser.error("--llms и --skip-llms несовместимы")

    try:
        snapshot = json.loads(UPSTREAM.read_text(encoding="utf-8"))
        overlay = json.loads(OVERLAY.read_text(encoding="utf-8"))
    except (OSError, ValueError) as err:
        print(f"spec_drift.py: не удалось прочитать файлы спецификации: {err}", file=sys.stderr)
        return EXIT_ERROR

    try:
        fresh = parse_spec(read_source(args.spec, SPEC_URL))
        llms_pages = None if args.skip_llms else parse_llms(read_source(args.llms, LLMS_URL))
    except (OSError, UnicodeDecodeError, SourceError) as err:
        # urllib.error.URLError и тайм-ауты — подклассы OSError.
        print(f"spec_drift.py: источник недоступен или не разобрался: {err}", file=sys.stderr)
        return EXIT_ERROR

    lines = drift(snapshot, fresh, overlay, llms_pages)
    pages = "" if llms_pages is None else f", страниц операций в справочнике {len(llms_pages)}"
    if not lines:
        print(
            f"spec_drift.py: изменений нет (операций {len(operations(fresh))}{pages})",
            file=sys.stderr,
        )
        return EXIT_OK
    source = args.spec or SPEC_URL
    print(f"Снимок wiki.upstream.json расходится с {source}{'' if llms_pages is None else ' и llms.txt'}:")
    for line in lines:
        print(line)
    print(
        "Обновите снимок и дополнение обычным PR: crates/wiki-api/spec/README.md, "
        "раздел «Обновление снимка»."
    )
    return EXIT_DRIFT


if __name__ == "__main__":
    sys.exit(main())
