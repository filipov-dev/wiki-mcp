#!/usr/bin/env python3
"""Сборка объединённой OpenAPI-спецификации Wiki API.

Берёт снимок официальной спецификации (wiki.upstream.json) и файл-дополнение
(overlay.json) и пишет wiki.openapi.json, из которой build.rs генерирует
MCP-инструменты.

    python3 crates/wiki-api/spec/build_openapi.py          # пересобрать
    python3 crates/wiki-api/spec/build_openapi.py --check  # сверить с файлом

Что делает дополнение с каждой операцией:

- operationId заменяется коротким именем инструмента (name);
- tags — один slug набора инструментов (toolset), список наборов — в
  верхнеуровневом tags с флагом x-default-enabled;
- x-access-level — read, write или destructive;
- parameter_descriptions — описания параметров по имени; общий словарь
  верхнего уровня дополнения действует на все операции, словарь операции
  его уточняет. Схемы параметров остаются из официальной спецификации;
- остальные поля операции из дополнения (summary, description, externalDocs,
  parameters, requestBody, responses и другие) заменяют поля официальной
  спецификации.

У каждой операции после объединения должны быть summary, description и
externalDocs.url.

Только стандартная библиотека Python 3.10+, без сети.
"""

from __future__ import annotations

import argparse
import copy
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
UPSTREAM = HERE / "wiki.upstream.json"
OVERLAY = HERE / "overlay.json"
OUT = HERE / "wiki.openapi.json"

METHODS = ("get", "post", "put", "patch", "delete")
ACCESS_LEVELS = ("read", "write", "destructive")
# Ключи дополнения, которые обрабатываются отдельно и в операцию не копируются.
SPECIAL_KEYS = {"name", "toolset", "parameter_descriptions"}
# Верхнеуровневые поля дополнения, которые заменяют поля официальной спецификации.
TOP_LEVEL_KEYS = ("info", "servers", "externalDocs", "security")
# Поля, без которых операция не попадёт в инструменты.
REQUIRED_FIELDS = ("summary", "description")


def fail(message: str) -> None:
    print(f"build_openapi.py: {message}", file=sys.stderr)
    sys.exit(1)


def build(upstream: dict, overlay: dict) -> dict:
    spec = copy.deepcopy(upstream)
    toolsets = overlay.get("toolsets", [])
    slugs = [toolset["name"] for toolset in toolsets]
    if len(slugs) != len(set(slugs)):
        fail("повторяется имя набора инструментов")
    spec["tags"] = toolsets

    # Общие компоненты (заголовки авторизации и прочее) добавляются к
    # официальным; совпадение имён — ошибка, чтобы не затереть схему молча.
    for section, items in overlay.get("components", {}).items():
        target = spec.setdefault("components", {}).setdefault(section, {})
        for name, value in items.items():
            if name in target:
                fail(f"components.{section}.{name} уже есть в официальной спецификации")
            target[name] = value
    for key in TOP_LEVEL_KEYS:
        if key in overlay:
            spec[key] = overlay[key]

    common_descriptions = overlay.get("parameter_descriptions", {})
    used_common = set()
    extra = overlay.get("operations", {})
    seen_keys = set()
    seen_names = set()
    for path, item in spec["paths"].items():
        for method in METHODS:
            op = item.get(method)
            if op is None:
                continue
            key = f"{method.upper()} {path}"
            seen_keys.add(key)
            patch = extra.get(key)
            if patch is None:
                fail(f"{key}: нет записи в overlay.json")
            name = patch.get("name", "")
            if not name or name in seen_names:
                fail(f"{key}: пустое или повторяющееся имя {name!r}")
            seen_names.add(name)
            if patch.get("toolset") not in slugs:
                fail(f"{key}: неизвестный набор {patch.get('toolset')!r}")
            if patch.get("x-access-level") not in ACCESS_LEVELS:
                fail(f"{key}: неизвестный x-access-level {patch.get('x-access-level')!r}")
            op["x-upstream-operation-id"] = op.get("operationId")
            op["operationId"] = name
            op["tags"] = [patch["toolset"]]
            for field, value in patch.items():
                if field not in SPECIAL_KEYS:
                    op[field] = value
            used_common |= describe_parameters(
                key, op, common_descriptions, patch.get("parameter_descriptions", {})
            )
            for field in REQUIRED_FIELDS:
                if not str(op.get(field, "")).strip():
                    fail(f"{key}: нет {field}")
            if not str(op.get("externalDocs", {}).get("url", "")).strip():
                fail(f"{key}: нет externalDocs.url")
    unknown = sorted(set(extra) - seen_keys)
    if unknown:
        fail(f"в overlay.json есть операции, которых нет в спецификации: {unknown}")
    unused = sorted(set(common_descriptions) - used_common)
    if unused:
        fail(f"общие описания параметров ни к чему не применились: {unused}")
    return spec


def describe_parameters(key: str, op: dict, common: dict, own: dict) -> set:
    """Проставляет описания параметров операции; возвращает применённые общие имена."""
    params = op.get("parameters", [])
    names = {param.get("name") for param in params}
    missing = sorted(set(own) - names)
    if missing:
        fail(f"{key}: в parameter_descriptions есть параметры, которых нет в операции: {missing}")
    used = set()
    for param in params:
        if "$ref" in param:
            fail(f"{key}: описание параметра по $ref не поддерживается")
        name = param["name"]
        if name in own:
            param["description"] = own[name]
        elif name in common:
            param["description"] = common[name]
            used.add(name)
    return used


def render(spec: dict) -> str:
    return json.dumps(spec, ensure_ascii=False, indent=2) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="не писать файл, а сверить его с пересборкой")
    args = parser.parse_args()

    upstream = json.loads(UPSTREAM.read_text(encoding="utf-8"))
    overlay = json.loads(OVERLAY.read_text(encoding="utf-8"))
    text = render(build(upstream, overlay))
    if args.check:
        current = OUT.read_text(encoding="utf-8") if OUT.exists() else ""
        if current != text:
            fail(f"{OUT.name} устарел: запустите build_openapi.py без --check")
        print(f"{OUT.name} совпадает с пересборкой", file=sys.stderr)
        return
    OUT.write_text(text, encoding="utf-8")
    print(f"записан {OUT.name}", file=sys.stderr)


if __name__ == "__main__":
    main()
