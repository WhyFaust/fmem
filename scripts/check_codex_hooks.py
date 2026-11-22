#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from uuid import uuid4

from fixtures.codex_app_server import AppServer

MODEL = "gpt-5.4"
PLUGIN = "fmem@fmem"
FINAL = "Отчёты о прогонах прикладываем к задаче."
SEED = "Артефакты прогонов"
STAGE_FILE = "АБСОЛЮТНЫЙ_ПУТЬ"
GRANT = re.compile(r"[0-9a-f]{64}\.[0-9a-f]{32}")
QUOTED = r"'(?:[^'\r\n]|'\\'')+'"
STAGE = re.compile(
    rf"({QUOTED} --state-dir {QUOTED} --project-dir {QUOTED} "
    rf"codex stage --turn {GRANT.pattern} --file {QUOTED})"
)
EXPECTED_HOOKS = {"userPromptSubmit", "subagentStart", "postToolUse", "stop", "subagentStop", "interrupt"}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def encode(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def literal(value: Path) -> str:
    return "'" + str(value).replace("'", "'\\''") + "'"


def model_inputs(request: dict) -> list[str]:
    return [
        part["text"]
        for item in request["input"]
        if item.get("role") != "assistant"
        for part in item.get("content", [])
        if part.get("type") == "input_text"
    ]


def staged_candidate(title: str) -> dict:
    return {
        "operations": [
            {
                "action": "remember",
                "candidate": {
                    "kind": "decision",
                    "title": title,
                    "body": "Результаты прогона прикладываем к задаче после проверки.",
                    "source": "Проверяемое основание из тестового сообщения",
                },
            }
        ]
    }


@dataclass
class Scenario:
    steps: tuple[str, ...]
    payload: dict
    requests: list[dict] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    waiting: threading.Event = field(default_factory=threading.Event)
    release: threading.Event = field(default_factory=threading.Event)
    stage_file: Path | None = None
    staged_stdout: str | None = None
    initial_outbox: dict[str, tuple[str, str, int]] = field(default_factory=dict)


class ResponsesProvider(ThreadingHTTPServer):
    daemon_threads = True
    scenario: Scenario
    binary: Path
    state: Path
    workspace: Path
    env: dict[str, str]


class ResponsesHandler(BaseHTTPRequestHandler):
    server: ResponsesProvider

    def log_message(self, *_args: object) -> None:
        pass

    def do_POST(self) -> None:
        if self.path != "/v1/responses":
            self.send_error(404)
            return
        scenario = self.server.scenario
        try:
            size = int(self.headers["Content-Length"])
            require(0 < size <= 2_000_000, "Слишком большой запрос к локальному провайдеру")
            request = json.loads(self.rfile.read(size))
            index = len(scenario.requests)
            step = scenario.steps[index]
            scenario.requests.append(request)
            if index and scenario.steps[index - 1] == "stage":
                self.accepted(request, scenario)
            if step == "wait":
                scenario.waiting.set()
                scenario.release.wait(timeout=45)
            item = self.stage(request, scenario) if step == "stage" else self.final()
            self.respond(item)
        except (BrokenPipeError, ConnectionResetError):
            if not scenario.waiting.is_set():
                scenario.errors.append("Codex оборвал ответ провайдера до завершения сценария")
        except (IndexError, KeyError, ValueError, RuntimeError) as exc:
            scenario.errors.append(str(exc))
            self.send_error(500)

    def stage(self, request: dict, scenario: Scenario) -> dict:
        commands = [command for text in model_inputs(request) for command in STAGE.findall(text)]
        require(bool(commands), "Провайдер не получил скрытую инструкцию codex stage")
        command = shlex.split(commands[-1])
        expected = [
            str(self.server.binary),
            "--state-dir",
            str(self.server.state),
            "--project-dir",
            str(self.server.workspace),
            "codex",
            "stage",
            "--turn",
        ]
        require(command[:-3] == expected and GRANT.fullmatch(command[-3]) is not None
                and command[-2:] == ["--file", STAGE_FILE],
                "Хук передал неверный путь runtime или грант")
        tools = self.tools(request)
        name = next((name for name in ("exec_command", "shell_command", "shell") if name in tools), None)
        require(name is not None, f"Codex не предложил провайдеру командный инструмент: {list(tools)}")
        assert scenario.stage_file is not None
        shell = commands[-1].removesuffix(literal(Path(STAGE_FILE))) + literal(scenario.stage_file)
        namespace, tool = tools[name]
        parameters = tool["parameters"]["properties"]
        args = {"cmd" if "cmd" in parameters else "command": shell}
        sequence = uuid4().hex
        item = {
            "type": "function_call",
            "id": f"fc_{sequence}",
            "call_id": f"call_{sequence}",
            "name": name,
            "arguments": encode(args),
            "status": "completed",
        }
        if namespace is not None:
            item["namespace"] = namespace
        return item

    @staticmethod
    def tools(request: dict) -> dict[str, tuple[str | None, dict]]:
        tools = {}
        for tool in request.get("tools", []):
            if tool["type"] == "namespace":
                tools.update((inner["name"], (tool["name"], inner)) for inner in tool["tools"] if inner["type"] == "function")
            elif tool["type"] == "function":
                tools[tool["name"]] = (None, tool)
        return tools

    def accepted(self, request: dict, scenario: Scenario) -> None:
        outputs = [item.get("output") for item in request["input"] if item.get("type") == "function_call_output"]
        require(bool(outputs), "Провайдер не получил результат командного инструмента")
        output = outputs[-1]
        if isinstance(output, list):
            output = "\n".join(part["text"] for part in output if part.get("type") in ("input_text", "output_text"))
        require(isinstance(output, str), "Командный инструмент вернул неожиданный результат")
        for line in reversed(output.splitlines()):
            if line.startswith("{"):
                try:
                    receipt = json.loads(line)
                except json.JSONDecodeError:
                    continue
                stage = receipt.get("fmem_stage", {})
                if receipt.get("validated") is True and stage.get("file") == str(scenario.stage_file):
                    canonical = stage["request"]
                    if "operations" in scenario.payload:
                        item = canonical["operations"][0]
                        expected = scenario.payload["operations"][0]
                        require(item["operation_id"].startswith("codex:")
                                and {key: item[key] for key in expected} == expected,
                                "Квитанция staging изменила UTF-8 запрос")
                    else:
                        require(canonical == scenario.payload, "Неверная квитанция skip")
                    scenario.staged_stdout = line
                    break
        require(scenario.staged_stdout is not None,
                "Workspace-write не получил нормализованную квитанцию staging: " + GRANT.sub("<grant>", output[-1500:]))
        observed = call(self.server.binary, ["codex", "diagnostics"], self.server.workspace, self.server.env)
        require(observed.get("phase") == "stage" and observed.get("staged") == 1,
                f"PostToolUse не принял staging до Stop: {observed}")
        require(outbox(self.server.state) == scenario.initial_outbox, "Staging применил операцию до Stop")
        assert scenario.stage_file is not None
        scenario.stage_file.unlink()

    @staticmethod
    def final() -> dict:
        return {
            "type": "message",
            "id": f"msg_{uuid4().hex}",
            "role": "assistant",
            "status": "completed",
            "phase": "final_answer",
            "content": [{"type": "output_text", "text": FINAL, "annotations": []}],
        }

    def respond(self, item: dict) -> None:
        response = {
            "id": f"resp_{uuid4().hex}",
            "object": "response",
            "status": "completed",
            "output": [item],
        }
        events = (
            {"type": "response.created", "response": {**response, "status": "in_progress", "output": []}},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": response},
        )
        body = "".join(f"event: {event['type']}\ndata: {encode(event)}\n\n" for event in events).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def fixture_environment(root: Path, profile: Path, state: Path) -> dict[str, str]:
    user = root / "user"
    cache = user / "cache"
    config = user / ".config"
    for directory in (user, cache, config, profile, state):
        directory.mkdir(exist_ok=True)
    inherited = {"PATH", "LANG", "LC_ALL"}
    env = {key: value for key, value in os.environ.items() if key in inherited}
    env.update(
        HOME=str(user),
        CODEX_HOME=str(profile),
        XDG_CONFIG_HOME=str(config),
        XDG_CACHE_HOME=str(cache),
        FMEM_STATE_DIR=str(state),
        PYTHONIOENCODING="utf-8",
        NO_COLOR="1",
    )
    return env


def configure(profile: Path, env: dict[str, str], workspace: Path, state: Path, port: int) -> None:
    cache = env["XDG_CACHE_HOME"]
    (profile / "config.toml").write_text(
        f'''
model = {encode(MODEL)}
model_provider = "fixture"
approval_policy = "never"
sandbox_mode = "workspace-write"
check_for_update_on_startup = false
web_search = "disabled"
project_doc_max_bytes = 0
cli_auth_credentials_store = "file"

[features]
apps = false
hooks = true
memory_tool = false
remote_plugin = false

[model_providers.fixture]
name = "Local Responses fixture"
base_url = "http://127.0.0.1:{port}/v1"
wire_api = "responses"
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0

[projects.{encode(str(workspace))}]
trust_level = "trusted"

[sandbox_workspace_write]
network_access = false
exclude_tmpdir_env_var = true
exclude_slash_tmp = true

[shell_environment_policy]
inherit = "core"

[shell_environment_policy.set]
CODEX_HOME = {encode(str(profile))}
FMEM_STATE_DIR = {encode(str(state))}
XDG_CACHE_HOME = {encode(cache)}
'''.lstrip(),
        encoding="utf-8",
    )


def call(executable: Path, args: list[str], workspace: Path, env: dict[str, str],
         payload: dict | None = None) -> dict:
    result = subprocess.run(
        [executable, *args],
        cwd=workspace,
        env=env,
        input=encode(payload) if payload is not None else "",
        capture_output=True,
        encoding="utf-8",
        timeout=60,
    )
    require(result.returncode == 0, f"{args[0]} завершился с ошибкой: {result.stderr.strip()[:1800]}")
    return json.loads(result.stdout)


def install_plugin(binary: Path, codex: Path, workspace: Path, env: dict[str, str], root: Path) -> Path:
    repository = Path(__file__).resolve().parents[1]
    package = root / "package"
    package.mkdir()
    for name in (".codex-plugin", ".agents", "hooks"):
        shutil.copytree(repository / name, package / name)
    shutil.copy2(binary, package / "fmem")
    installed = call(binary, ["codex", "install", "--package", str(package), "--codex", str(codex)], workspace, env)
    require(installed["plugin"].get("plugin", PLUGIN) == PLUGIN, "Codex установил другой плагин")
    return (Path(installed["plugin"]["installedPath"]) / "fmem").resolve(strict=True)


def list_hooks(server: AppServer, workspace: Path, deadline: float) -> list[dict]:
    groups = server.request("hooks/list", {"cwds": [str(workspace)]}, deadline)["data"]
    hooks = [hook for group in groups for hook in group["hooks"]]
    require(
        {hook["eventName"] for hook in hooks} == EXPECTED_HOOKS
        and all(hook["pluginId"] == PLUGIN and hook["enabled"] for hook in hooks),
        "Codex не обнаружил ожидаемые хуки установленного fmem",
    )
    prompts = [hook for hook in hooks if hook["eventName"] == "userPromptSubmit"]
    require(len(prompts) == 2 and {hook.get("async") for hook in prompts} == {False, True},
            "Codex должен обнаружить отдельные синхронный recall и асинхронный worker")
    return hooks


def trust_hooks(codex: Path, workspace: Path, env: dict[str, str], root: Path, profile: Path) -> None:
    with AppServer(codex, workspace, env, root / "discovery.log") as server:
        deadline = time.monotonic() + 30
        server.initialize(deadline)
        hooks = list_hooks(server, workspace, deadline)
    with (profile / "config.toml").open("a", encoding="utf-8") as config:
        for hook in hooks:
            require(hook["trustStatus"] == "untrusted", "Тест ожидал новый профиль без доверенных хуков")
            declared, separator, content = hook["currentHash"].partition("/")
            config.write(f'\n[hooks.state.{encode(hook["key"])}]\ntrusted_hash = {encode(declared)}\n')
            if separator:
                config.write(f'trusted_content_hash = {encode(content)}\n')


@dataclass
class Turn:
    thread: str
    id: str
    hooks: list[dict]
    items: list[dict]


def run_turn(server: AppServer, provider: ResponsesProvider, scenario: Scenario, workspace: Path,
             *, mode: str = "default", interrupted: bool = False, thread: str | None = None,
             prompt: str = "Артефакты прогонов: уточни, куда прикладывать результаты проверки.") -> Turn:
    provider.scenario = scenario
    scenario.initial_outbox = outbox(provider.state)
    if "stage" in scenario.steps:
        scenario.stage_file = workspace / f"память {uuid4().hex}.json"
        scenario.stage_file.write_text(encode(scenario.payload), encoding="utf-8")
    deadline = time.monotonic() + 45
    if thread is None:
        started = server.request("thread/start", {
            "cwd": str(workspace),
            "model": MODEL,
            "modelProvider": "fixture",
            "ephemeral": False,
            "approvalPolicy": "never",
            "sandbox": "workspace-write",
            "allowProviderModelFallback": False,
        }, deadline)
        thread = started["thread"]["id"]
        sandbox = started.get("sandbox") or {}
        require(started.get("approvalPolicy") == "never"
                and sandbox.get("type") == "workspaceWrite" and sandbox.get("networkAccess") is False
                and sandbox.get("excludeSlashTmp") is True and sandbox.get("excludeTmpdirEnvVar") is True,
                f"Codex не подтвердил ограниченный профиль workspace-write: {sandbox}")
    turn = server.request("turn/start", {
        "threadId": thread,
        "input": [{"type": "text", "text": prompt}],
        "collaborationMode": {
            "mode": mode,
            "settings": {"model": MODEL, "developer_instructions": None, "reasoning_effort": "low"},
        },
    }, deadline)["turn"]
    id = turn["id"]
    hooks, items, errors = [], [], []
    try:
        if interrupted:
            require(scenario.waiting.wait(timeout=15), "Провайдер не дождался окончания staging до Interrupt")
            server.request("turn/interrupt", {"threadId": thread, "turnId": id}, deadline)
        while True:
            message = server.next_notification(deadline)
            params = message.get("params", {})
            if params.get("threadId") != thread or params.get("turnId") not in (None, id):
                continue
            match message.get("method"):
                case "hook/completed":
                    hooks.append(params["run"])
                case "item/completed":
                    items.append(params["item"])
                case "error":
                    errors.append(params)
                case "turn/completed":
                    turn = params["turn"]
                    status = turn["status"]
                    require(not scenario.errors, f"Ошибка локального провайдера: {scenario.errors}")
                    require(status == ("interrupted" if interrupted else "completed"),
                            f"Неожиданное завершение хода: {status}, {turn.get('error')}, {errors}")
                    require(len(scenario.requests) == len(scenario.steps),
                            f"Провайдер получил {len(scenario.requests)} запросов вместо {len(scenario.steps)}")
                    require(bool(scenario.requests) and prompt in model_inputs(scenario.requests[0]),
                            "Провайдер не получил тестовое сообщение")
                    return Turn(thread, id, hooks, items)
    finally:
        scenario.release.set()


def hook_runs(turn: Turn, event: str) -> list[dict]:
    return [hook for hook in turn.hooks if hook["eventName"] == event and hook["source"] == "plugin"]


def assert_context(turn: Turn, scenario: Scenario, seed_id: str, *, stage: bool) -> None:
    prompts = [hook for hook in hook_runs(turn, "userPromptSubmit") if hook["executionMode"] == "sync"]
    require(len(prompts) == 1 and prompts[0]["status"] == "completed",
            f"App-server не подтвердил UserPromptSubmit fmem: {prompts}")
    context = "\n".join(model_inputs(scenario.requests[0]))
    require("Память fmem" in context and SEED in context and seed_id in context,
            "Провайдер не получил контекст из уже сохранённой записи")
    require(bool(STAGE.search(context)) == stage, "Неверная доступность staging в режиме хода")


def assert_final(turn: Turn) -> None:
    answers = [item["text"] for item in turn.items if item["type"] == "agentMessage"]
    require(bool(answers) and all(answer == FINAL for answer in answers),
            "Служебный вывод попал в ответы или отсутствует итоговый ответ")


def assert_recovery(turn: Turn) -> None:
    stops = hook_runs(turn, "stop")
    recovery = [
        item for hook in stops for item in hook.get("entries", [])
        if isinstance(item["text"], str) and STAGE.search(item["text"])
    ]
    require(len(stops) == 2 and len(recovery) == 1,
            f"Ожидались два Stop и ровно одно внутреннее восстановление: {[hook['status'] for hook in stops]}")


def assert_staged(turn: Turn, scenario: Scenario) -> None:
    post_tool = hook_runs(turn, "postToolUse")
    require(scenario.staged_stdout is not None and bool(post_tool)
            and all(hook["status"] == "completed" for hook in post_tool),
            f"Не подтверждён staging через PostToolUse: {[hook['status'] for hook in post_tool]}")


def outbox(state: Path) -> dict[str, tuple[str, str, int]]:
    path = state / "operations.sqlite3"
    with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
        rows = db.execute("SELECT operation_id, receipt, status, attempts FROM operations").fetchall()
    return {
        operation_id: (json.loads(receipt)["record_id"], status, attempts)
        for operation_id, receipt, status, attempts in rows
    }


def assert_applied(before: dict, state: Path, records: Path, title: str, turn: Turn) -> None:
    after = outbox(state)
    created = after.keys() - before.keys()
    require(len(created) == 1, f"Stop должен сохранить одну операцию, получено {len(created)}")
    id, status, attempts = after[next(iter(created))]
    require((status, attempts) == ("persisted", 1), f"Неверная квитанция Stop: {(status, attempts)}")
    matches = list((records / id).glob("*.md"))
    require(len(matches) == 1, "Запись должна иметь один Markdown в каталоге ID")
    note = matches[0].read_text(encoding="utf-8")
    require(all(value in note for value in (title, "Результаты прогона", turn.id, turn.thread)),
            "Markdown потерял UTF-8 содержание или данные сессии и хода")


def check(binary: Path, codex: Path, root: Path) -> None:
    workspace = root / "workspace"
    state = root / "state"
    profile = root / "codex"
    vault = root / "vault"
    workspace.mkdir()
    (vault / ".obsidian").mkdir(parents=True)
    env = fixture_environment(root, profile, state)
    with ResponsesProvider(("127.0.0.1", 0), ResponsesHandler) as provider:
        configure(profile, env, workspace, state, provider.server_port)
        installed = install_plugin(binary, codex, workspace, env, root)
        trust_hooks(codex, workspace, env, root, profile)
        provider.binary, provider.state, provider.workspace, provider.env = installed, state, workspace, env
        provider.scenario = Scenario((), {})
        call(binary, ["init", "--vault", str(vault), "--project", "fixture", "--mode", "auto"], workspace, env)
        candidate = staged_candidate(SEED)
        candidate["operations"][0]["operation_id"] = "fixture-seed"
        seed = call(binary, ["commit"], workspace, env, candidate)["receipts"][0]
        require(seed["status"] == "persisted", f"Не сохранена исходная запись: {seed}")
        seed_id = seed["record_id"]
        records = vault / "Codex Memory" / "Projects" / "fixture" / "Records"
        serving = threading.Thread(target=provider.serve_forever, daemon=True)
        serving.start()
        try:
            with AppServer(codex, workspace, env, root / "codex.log") as server:
                deadline = time.monotonic() + 30
                server.initialize(deadline)
                hooks = list_hooks(server, workspace, deadline)
                require(all(hook["trustStatus"] == "trusted" for hook in hooks),
                        "Доверие к установленным хукам не применилось")

                before = outbox(state)
                plan = Scenario(("final",), {})
                turn = run_turn(server, provider, plan, workspace, mode="plan")
                assert_context(turn, plan, seed_id, stage=False)
                assert_final(turn)
                require(outbox(state) == before, "Ход в Plan изменил журнал записей")
                print("Plan: recall передан, новых операций нет", flush=True)

                ignored = Scenario(("final", "final"), {})
                turn = run_turn(server, provider, ignored, workspace)
                assert_context(turn, ignored, seed_id, stage=True)
                assert_recovery(turn)
                assert_final(turn)
                require(outbox(state) == before, "Несохранённый ход изменил журнал")
                print("Stop: забытая запись получила одну попытку восстановления", flush=True)

                normal_title = "Условия прикрепления отчёта"
                normal = Scenario(("stage", "final"), staged_candidate(normal_title))
                turn = run_turn(server, provider, normal, workspace)
                assert_context(turn, normal, seed_id, stage=True)
                assert_staged(turn, normal)
                assert_final(turn)
                require(len(hook_runs(turn, "stop")) == 1, "В обычном ходе ожидался один Stop")
                assert_applied(before, state, records, normal_title, turn)
                print("Stop: auto staging применён один раз", flush=True)

                before = outbox(state)
                restored_title = "Проверка статуса отчёта"
                restored = Scenario(("final", "stage", "final"), staged_candidate(restored_title))
                turn = run_turn(server, provider, restored, workspace)
                assert_context(turn, restored, seed_id, stage=True)
                assert_staged(turn, restored)
                assert_recovery(turn)
                assert_final(turn)
                assert_applied(before, state, records, restored_title, turn)
                print("Stop: staging из внутреннего восстановления применён один раз", flush=True)

                before = outbox(state)
                cancelled_title = "Отмена перед завершением"
                cancelled = Scenario(("stage", "wait"), staged_candidate(cancelled_title))
                turn = run_turn(server, provider, cancelled, workspace, interrupted=True)
                assert_context(turn, cancelled, seed_id, stage=True)
                assert_staged(turn, cancelled)
                require(bool(hook_runs(turn, "interrupt")), "App-server не запустил Interrupt fmem")
                require(outbox(state) == before, "Interrupt применил staged операцию")
                skipped = Scenario(("stage", "final"), {"action": "skip"})
                after = run_turn(server, provider, skipped, workspace, thread=turn.thread)
                assert_staged(after, skipped)
                assert_final(after)
                require(outbox(state) == before, "Обычный Stop применил отменённую запись")
                print("Interrupt: staged запись отменена и не воспроизводится", flush=True)
        finally:
            provider.scenario.release.set()
            provider.shutdown()
            serving.join(timeout=5)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Проверяет хуки fmem через настоящий Codex app-server и локальную заглушку Responses.",
        epilog=(
            "Linux, Python 3.10+, Codex с app-server и поддерживаемая песочница команд. "
            "Сначала собери fmem командой cargo build --release --locked. "
            "Пример: scripts/check_codex_hooks.py --fmem target/release/fmem --codex /opt/codex/bin/codex. "
            "Профиль, vault и HTTP-провайдер создаются локально на время теста."
        ),
    )
    parser.add_argument("--fmem", type=Path, default=Path("target/release/fmem"), help="путь к собранному бинарнику fmem")
    parser.add_argument("--codex", type=Path, default=Path(shutil.which("codex") or "codex"), help="путь к бинарнику Codex")
    parser.add_argument("--scratch-dir", type=Path, default=os.environ.get("RUNNER_TEMP"),
                        help="каталог для временного тестового профиля вне репозитория")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="fmem-codex-hooks-", dir=args.scratch_dir) as temporary:
        root = Path(temporary).resolve()
        try:
            check(args.fmem.resolve(strict=True), args.codex.resolve(strict=True), root)
        except (RuntimeError, TimeoutError, OSError, ValueError, KeyError, AssertionError) as exc:
            print("Проверка хуков Codex завершилась ошибкой: " + GRANT.sub("<grant>", str(exc)[:2200]), file=sys.stderr)
            for log in (root / "discovery.log", root / "codex.log"):
                if log.is_file():
                    tail = log.read_text(encoding="utf-8", errors="replace")[-2600:].strip()
                    if tail:
                        print(GRANT.sub("<grant>", tail), file=sys.stderr)
            return 1
    print("Проверка хуков Codex прошла.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
