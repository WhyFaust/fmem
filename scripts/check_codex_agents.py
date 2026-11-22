#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from uuid import uuid4

import check_codex_hooks as hooks
from fixtures.codex_app_server import AppServer

MARKERS = tuple(f"FMEM_NATIVE_AGENT_DEPTH_{depth}" for depth in range(3))
DELEGATED_POLICY = "Сохраняй только новое решение проекта с проверяемым источником."
ROOT_POLICY = "scope=shared используй только"


class Provider(hooks.ResponsesProvider):
    def __init__(self, address: tuple[str, int]) -> None:
        super().__init__(address, Handler)
        self.lock = threading.Lock()
        self.requests: list[list[dict]] = [[], [], []]
        self.grants: dict[int, str] = {}
        self.children: dict[int, str] = {}
        self.errors: list[str] = []


class Handler(hooks.ResponsesHandler):
    server: Provider

    def do_POST(self) -> None:
        if self.path != "/v1/responses":
            self.send_error(404)
            return
        try:
            size = int(self.headers["Content-Length"])
            hooks.require(0 < size <= 2_000_000, "Слишком большой запрос к локальному провайдеру")
            request = json.loads(self.rfile.read(size))
            texts = hooks.model_inputs(request)
            depths = [depth for depth, marker in enumerate(MARKERS) if any(marker in text for text in texts)]
            hooks.require(len(depths) == 1, f"Не определён единственный агент запроса: {depths}")
            depth = depths[0]
            with self.server.lock:
                index = len(self.server.requests[depth])
                self.server.requests[depth].append(request)
            maximum = 2 if depth == 2 else 4
            hooks.require(index < maximum, f"Агент глубины {depth} превысил одну попытку восстановления")
            self.check_context(request, depth, index)
            if depth < 2 and index == 0:
                item = self.function(request, "spawn_agent", {
                    "message": f"{MARKERS[depth + 1]}: выполни только изолированный сценарий проверки и заверши ответ.",
                })
            elif depth < 2 and index == 1:
                child = spawned_id(request)
                with self.server.lock:
                    self.server.children[depth + 1] = child
                item = self.function(request, "wait_agent", {"targets": [child], "timeout_ms": 10000})
            else:
                if depth < 2 and index == 2:
                    waited = last_tool_output(request)
                    child = self.server.children[depth + 1]
                    hooks.require(waited.get("timed_out") is False
                                  and isinstance(waited.get("status", {}).get(child, {}).get("completed"), str),
                                  f"Агент глубины {depth} не дождался ребёнка: {waited}")
                item = self.final()
            self.respond(item)
        except (BrokenPipeError, ConnectionResetError, IndexError, KeyError, OSError,
                subprocess.TimeoutExpired, TypeError, ValueError, RuntimeError) as exc:
            with self.server.lock:
                self.server.errors.append(str(exc))
            try:
                self.send_error(500)
            except (BrokenPipeError, ConnectionResetError):
                pass

    def function(self, request: dict, name: str, args: dict) -> dict:
        available = self.tools(request)
        hooks.require(name in available, f"Codex не предложил {name}")
        namespace, _ = available[name]
        sequence = uuid4().hex
        item = {
            "type": "function_call", "id": f"fc_{sequence}", "call_id": f"call_{sequence}",
            "name": name, "arguments": hooks.encode(args), "status": "completed",
        }
        if namespace is not None:
            item["namespace"] = namespace
        return item

    def check_context(self, request: dict, depth: int, index: int) -> None:
        text = "\n".join(hooks.model_inputs(request))
        commands = hooks.STAGE.findall(text)
        hooks.require(bool(commands), f"Агент глубины {depth} не получил скрытую инструкцию staging")
        parts = shlex.split(commands[-1])
        expected = [str(self.server.binary), "--state-dir", str(self.server.state),
                    "--project-dir", str(self.server.workspace), "codex", "stage", "--turn"]
        hooks.require(parts[:-3] == expected and hooks.GRANT.fullmatch(parts[-3]) is not None
                      and parts[-2:] == ["--file", hooks.STAGE_FILE], "Неверная буквальная команда staging")
        grant = parts[-3]
        with self.server.lock:
            if index == 0:
                self.server.grants[depth] = grant
            hooks.require(self.server.grants[depth] == grant, "Восстановление изменило грант текущего хода")
        if depth:
            hooks.require(DELEGATED_POLICY in text and ROOT_POLICY not in text,
                          f"Агент глубины {depth} получил права root вместо ограниченного capture")
            if index == 0:
                rejected = subprocess.run(
                    [self.server.binary, "--state-dir", self.server.state, "codex", "stage", "--turn", grant],
                    cwd=self.server.workspace, env=self.server.env,
                    input=hooks.encode({"operations": [{"action": "remember", "scope": "shared", "candidate": {
                        "kind": "preference", "title": "Запрещённое общее предпочтение",
                        "body": "Дочерний агент не должен сохранять эту запись.", "source": "Изолированная проверка запрета",
                    }}]}),
                    capture_output=True, encoding="utf-8", timeout=10,
                )
                hooks.require(rejected.returncode != 0 and "delegated" in rejected.stderr,
                              "Native child grant не отклонил запись общей настройки по ограничению роли")


def last_tool_output(request: dict) -> dict:
    outputs = [item["output"] for item in request["input"] if item.get("type") == "function_call_output"]
    hooks.require(bool(outputs), "Отсутствует настоящий ответ инструмента Codex")
    value = outputs[-1]
    if isinstance(value, list):
        value = "\n".join(part.get("text", "") for part in value if isinstance(part, dict))
    hooks.require(isinstance(value, str), "Неверный формат ответа инструмента")
    parsed = json.loads(value)
    hooks.require(isinstance(parsed, dict), "Ответ инструмента не является объектом")
    return parsed


def spawned_id(request: dict) -> str:
    identifier = last_tool_output(request).get("agent_id")
    hooks.require(isinstance(identifier, str) and bool(identifier), "spawn_agent не вернул agent_id")
    return identifier


def assert_no_writes(state: Path, vault: Path) -> None:
    database = state / "operations.sqlite3"
    if database.exists():
        with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
            count = connection.execute("SELECT COUNT(*) FROM operations").fetchone()[0]
        hooks.require(count == 0, "Сценарий без staging записал операцию в долговечный журнал")
    hooks.require(not list((vault / "Codex Memory").glob("**/Records/**/*.md")),
                  "Подагент сохранил запрещённую запись в vault")


def check(binary: Path, codex: Path, root: Path) -> None:
    workspace, state, profile, vault = (root / name for name in ("workspace", "state", "codex", "vault"))
    workspace.mkdir()
    (vault / ".obsidian").mkdir(parents=True)
    env = hooks.fixture_environment(root, profile, state)
    with Provider(("127.0.0.1", 0)) as provider:
        hooks.configure(profile, env, workspace, state, provider.server_port)
        config = profile / "config.toml"
        config.write_text(config.read_text(encoding="utf-8").replace("hooks = true", "hooks = true\nmulti_agent = true")
                          + "\n[agents]\nmax_depth = 2\n", encoding="utf-8")
        installed = hooks.install_plugin(binary, codex, workspace, env, root)
        hooks.trust_hooks(codex, workspace, env, root, profile)
        hooks.call(installed, ["init", "--vault", str(vault), "--project", "fixture", "--mode", "auto"], workspace, env)
        provider.binary, provider.state, provider.workspace, provider.env = installed, state, workspace, env
        assert_no_writes(state, vault)
        serving = threading.Thread(target=provider.serve_forever, daemon=True)
        serving.start()
        events: list[dict] = []
        try:
            with AppServer(codex, workspace, env, root / "codex.log") as server:
                deadline = time.monotonic() + 45
                server.initialize(deadline)
                declared = hooks.list_hooks(server, workspace, deadline)
                hooks.require(all(hook["trustStatus"] == "trusted" for hook in declared), "Хуки не получили доверие")
                started = server.request("thread/start", {
                    "cwd": str(workspace), "model": hooks.MODEL, "modelProvider": "fixture", "ephemeral": False,
                    "approvalPolicy": "never", "sandbox": "workspace-write", "allowProviderModelFallback": False,
                }, deadline)
                thread = started["thread"]["id"]
                turn = server.request("turn/start", {
                    "threadId": thread, "input": [{"type": "text", "text": f"{MARKERS[0]}: проверь вложенное делегирование."}],
                    "collaborationMode": {"mode": "default", "settings": {
                        "model": hooks.MODEL, "developer_instructions": None, "reasoning_effort": "low",
                    }},
                }, deadline)["turn"]
                while True:
                    message = server.next_notification(deadline)
                    events.append(message)
                    if message.get("method") == "turn/completed" and message.get("params", {}).get("threadId") == thread:
                        hooks.require(not provider.errors, f"Ошибки локального провайдера: {provider.errors}")
                        hooks.require(message["params"]["turn"]["id"] == turn["id"]
                                      and message["params"]["turn"]["status"] == "completed", "Корневой ход не завершился")
                        break
        finally:
            provider.shutdown()
            serving.join(timeout=5)
        hooks.require(not provider.errors, f"Ошибки локального провайдера: {provider.errors}")
        hooks.require([len(requests) for requests in provider.requests] == [4, 4, 2],
                      "Не подтверждены отдельные ходы root, child и grandchild с одной попыткой восстановления")
        hooks.require(len(set(provider.grants.values())) == 3, "Разные агенты получили общий грант записи")
        threads = {0: thread, **provider.children}
        hooks.require(len(set(threads.values())) == 3, "Codex не создал двух отдельных дочерних агентов")
        for depth, identifier in threads.items():
            runs = [event["params"]["run"] for event in events if event.get("method") == "hook/completed"
                    and event["params"]["threadId"] == identifier and event["params"]["run"]["source"] == "plugin"]
            prompts = [run for run in runs if run["eventName"] == "userPromptSubmit" and run["executionMode"] == "sync"]
            hooks.require(len(prompts) == 1 and prompts[0]["status"] == "completed", "Не подтверждён sync prompt hook агента")
            if depth:
                starts = [run for run in runs if run["eventName"] == "subagentStart"]
                hooks.require(len(starts) == 1 and starts[0]["status"] == "completed", "Не подтверждён native SubagentStart")
            stops = [run for run in runs if run["eventName"] == ("subagentStop" if depth else "stop")]
            feedback = [entry for run in stops for entry in run.get("entries", []) if hooks.STAGE.search(entry.get("text", ""))]
            hooks.require([run["status"] for run in stops] == ["blocked", "completed"] and len(feedback) == 1,
                          f"Агент глубины {depth} не получил ровно одно ограниченное восстановление")
        assert_no_writes(state, vault)
        diagnostics = hooks.call(installed, ["codex", "diagnostics"], workspace, env)
        hooks.require(diagnostics["staged"] == diagnostics["pending"] == 0, "После завершения осталось необработанное staging")
        print("Native root → child → grandchild: отдельные гранты, ограниченные права и одно восстановление; записей нет.")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Проверяет native subagent хуки fmem через настоящий Codex и локальный Responses stub.",
        epilog="Linux, Python 3.10+, Codex с app-server и subagents. Сценарий не обращается к LLM и не вызывает shell tool.",
    )
    parser.add_argument("--fmem", type=Path, default=Path("target/release/fmem"))
    parser.add_argument("--codex", type=Path, default=Path(shutil.which("codex") or "codex"))
    parser.add_argument("--scratch-dir", type=Path, default=os.environ.get("RUNNER_TEMP"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="fmem-codex-agents-", dir=args.scratch_dir) as temporary:
        root = Path(temporary).resolve()
        try:
            check(args.fmem.resolve(strict=True), args.codex.resolve(strict=True), root)
        except (RuntimeError, TimeoutError, OSError, ValueError, KeyError, TypeError, AssertionError) as exc:
            print("Проверка подагентов Codex завершилась ошибкой: " + hooks.GRANT.sub("<grant>", str(exc)[:2200]), file=sys.stderr)
            for log in (root / "discovery.log", root / "codex.log"):
                if log.is_file():
                    tail = log.read_text(encoding="utf-8", errors="replace")[-2600:].strip()
                    if tail:
                        print(hooks.GRANT.sub("<grant>", tail), file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
