#!/usr/bin/env python3
"""Проверить автоматический RAG реальными хуками Codex и локальной ONNX-моделью."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
import statistics
import tempfile
import threading
import time

import check_codex_hooks as hooks


RECORDS = (
    ("delivery", "Порядок доставки", "Отправляй посылки покупателям через почтовое отделение."),
    ("leave", "Vacation request", "Submit annual leave through the human resources portal."),
    ("whales", "Whale watching", "Hydrophones measure underwater vocalizations from marine mammals."),
    ("maintenance", "Обслуживание", "Очисти фильтр кондиционера и проверь компрессор."),
)
RU_QUERY = "Как переслать заказ клиенту?"
EN_QUERY = "Where do I book time off work?"


def verify(binary: Path, codex: Path, model: Path, runtime: Path, root: Path, samples: int) -> dict:
    workspace, state, profile, vault = (root / name for name in ("workspace", "state", "codex", "vault"))
    workspace.mkdir()
    (vault / ".obsidian").mkdir(parents=True)
    env = hooks.fixture_environment(root, profile, state)
    report = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "records": len(RECORDS),
              "model": "multilingual-e5-small", "queries": [RU_QUERY, EN_QUERY], "samples_ms": []}
    with hooks.ResponsesProvider(("127.0.0.1", 0), hooks.ResponsesHandler) as provider:
        hooks.configure(profile, env, workspace, state, provider.server_port)
        installed = hooks.install_plugin(binary, codex, workspace, env, root)
        hooks.trust_hooks(codex, workspace, env, root, profile)
        provider.binary, provider.state, provider.workspace, provider.env = installed, state, workspace, env
        provider.scenario = hooks.Scenario((), {})
        hooks.call(binary, ["init", "--vault", str(vault), "--project", "semantic"], workspace, env)
        operations = [{"operation_id": f"fixture:{key}", "action": "remember", "candidate": {
            "kind": "decision", "title": title, "body": body, "source": "Вымышленная проверка автопоиска",
        }} for key, title, body in RECORDS]
        receipts = hooks.call(binary, ["commit"], workspace, env, {"operations": operations})["receipts"]
        hooks.require(all(receipt["status"] == "persisted" for receipt in receipts), "Не сохранён корпус")
        ids = {record[0]: receipt["record_id"] for record, receipt in zip(RECORDS, receipts)}
        for query in (RU_QUERY, EN_QUERY):
            lexical = hooks.call(binary, ["search", query, "--mode", "lexical"], workspace, env)
            hooks.require(not lexical["hits"], "Контрольный запрос уже находится по словам")
        hooks.call(binary, ["model", "install", "--from", str(model), "--runtime", str(runtime)], workspace, env)
        endpoint = state / "semantic-worker" / "endpoint"
        before = hooks.outbox(state)
        serving = threading.Thread(target=provider.serve_forever, daemon=True)
        serving.start()
        try:
            with hooks.AppServer(codex, workspace, env, root / "codex.log") as server:
                server.initialize(time.monotonic() + 30)

                def submit(query: str, mode: str = "default") -> tuple[str, dict]:
                    scenario = hooks.Scenario(("final",) if mode == "plan" else ("final", "final"), {})
                    turn = hooks.run_turn(server, provider, scenario, workspace, mode=mode, prompt=query)
                    hooks.assert_final(turn)
                    if mode == "default":
                        hooks.assert_recovery(turn)
                    sync = [run for run in hooks.hook_runs(turn, "userPromptSubmit") if run["executionMode"] == "sync"]
                    hooks.require(len(sync) == 1 and sync[0]["status"] == "completed", "Не завершился синхронный recall")
                    return "\n".join(hooks.model_inputs(scenario.requests[0])), sync[0]

                submit(RU_QUERY, "plan")
                hooks.require(not endpoint.exists(), "Plan запустил semantic worker")
                report["plan_did_not_start_worker"] = True
                started = time.monotonic()

                def await_recall(query: str, identifier: str, evidence: str = "") -> tuple[str, dict]:
                    deadline = time.monotonic() + 35
                    while True:
                        context, run = submit(query)
                        if identifier in context and evidence in context:
                            return context, run
                        hooks.require(time.monotonic() < deadline, "Автоматический индекс не вернул ожидаемую запись")
                        time.sleep(0.1)

                await_recall(RU_QUERY, ids["delivery"])
                report["first_semantic_context_seconds"] = round(time.monotonic() - started, 3)
                hooks.require(endpoint.exists(), "Нет endpoint работающего worker")
                owner = endpoint.read_bytes()
                await_recall(EN_QUERY, ids["leave"])
                for _ in range(samples):
                    context, run = submit(RU_QUERY)
                    hooks.require(ids["delivery"] in context, "Прогретый хук потерял результат поиска")
                    hooks.require(endpoint.read_bytes() == owner, "Обычный запрос перезапустил worker")
                    report["samples_ms"].append(run["durationMs"])

                directory = vault / "Codex Memory" / "Projects" / "semantic" / "Records" / ids["delivery"]
                matches = list(directory.glob("*.md"))
                hooks.require(len(matches) == 1, "Нет единственной заметки для внешней правки")
                record = matches[0]
                raw = record.read_text(encoding="utf-8")
                boundary = raw.index("\n---\n") + 5
                revised = "Отправляй посылки покупателям через курьерскую службу. Проверяй адрес перед отправлением."
                record.write_text(raw[:boundary] + "\n" + revised + "\n", encoding="utf-8")
                edited_at = time.monotonic()
                context, _ = await_recall(RU_QUERY, ids["delivery"], "курьерскую службу")
                hooks.require("почтовое отделение" not in context, "В контексте осталась прежняя редакция")
                report["external_edit_seconds"] = round(time.monotonic() - edited_at, 3)
                report["same_worker_after_edit"] = endpoint.read_bytes() == owner
                hooks.require(report["same_worker_after_edit"], "Внешняя правка перезапустила worker")

                hooks.call(binary, ["commit"], workspace, env, {"operations": [{
                    "operation_id": "fixture:forget-delivery", "action": "forget", "id": ids["delivery"],
                }]})
                for _ in range(2):
                    context, _ = submit(RU_QUERY)
                    hooks.require(ids["delivery"] not in context, "Забытая запись вернулась в контекст")
                report["forgotten_record_absent"] = True
                after = hooks.outbox(state)
                hooks.require(len(after) == len(before) + 1, "Тестовые ответы добавили знания")
                report["unexpected_writes"] = 0
        finally:
            provider.scenario.release.set()
            provider.shutdown()
            serving.join(timeout=5)
    durations = report["samples_ms"]
    report["median_ms"] = statistics.median(durations)
    report["p95_ms"] = sorted(durations)[math.ceil(len(durations) * 0.95) - 1]
    report["measurement"] = "Native Codex sync hook duration: wrapper, process, IPC, local retrieval; excludes model generation"
    hooks.require(report["binary_sha256"] == hashlib.sha256(binary.read_bytes()).hexdigest(), "Бинарник изменился во время проверки")
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fmem", type=Path, default=Path("target/release/fmem"))
    parser.add_argument("--codex", type=Path, required=True)
    parser.add_argument("--model-source", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--scratch-dir", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--samples", type=int, default=11)
    args = parser.parse_args()
    if args.samples < 5:
        parser.error("Нужно не менее пяти измерений")
    with tempfile.TemporaryDirectory(prefix="fmem-semantic-hooks-", dir=args.scratch_dir) as temporary:
        root = Path(temporary).resolve()
        try:
            result = verify(args.fmem.resolve(strict=True), args.codex.resolve(strict=True),
                            args.model_source.resolve(strict=True), args.runtime.resolve(strict=True), root, args.samples)
        except (RuntimeError, TimeoutError, OSError) as error:
            print("Semantic hooks: " + hooks.GRANT.sub("<grant>", str(error)))
            log = root / "codex.log"
            if log.exists():
                print(hooks.GRANT.sub("<grant>", log.read_text(errors="replace")[-2000:]))
            return 1
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
