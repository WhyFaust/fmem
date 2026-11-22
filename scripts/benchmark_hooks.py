"""Измерить полный локальный цикл хуков на синтетическом vault без модели и сети."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import tempfile
import time


def summarize(samples):
    return {
        "samples_ms": samples,
        "median_ms": round(statistics.median(samples), 3),
        "p95_ms": sorted(samples)[math.ceil(len(samples) * 0.95) - 1],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--records", type=int, default=1000)
    parser.add_argument("--samples", type=int, default=11)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.records < 0 or args.samples < 5:
        parser.error("Нужны неотрицательное число записей и не менее пяти замеров")
    binary = args.binary.resolve(strict=True)
    fingerprint = hashlib.sha256(binary.read_bytes()).hexdigest()
    timings = {name: [] for name in ("prompt", "stage_skip", "stop")}
    with tempfile.TemporaryDirectory(prefix="fmem-hooks-benchmark-") as temporary:
        root = Path(temporary).resolve()
        state, project, vault = [root / name for name in ("state", "project", "vault")]
        project.mkdir()
        (vault / ".obsidian").mkdir(parents=True)
        env = dict(os.environ)
        for name, relative in {
            "CODEX_HOME": "profile", "HOME": "user", "USERPROFILE": "user",
            "APPDATA": "config", "LOCALAPPDATA": "cache",
            "XDG_CONFIG_HOME": "config", "XDG_CACHE_HOME": "cache",
        }.items():
            env[name] = str(root / relative)
        env.pop("FMEM_STATE_DIR", None)
        command = [str(binary), "--state-dir", str(state), "--project-dir", str(project)]

        def call(arguments, value=None):
            result = subprocess.run(
                command + list(map(str, arguments)),
                input=None if value is None else json.dumps(value, ensure_ascii=False).encode(),
                cwd=project, env=env, capture_output=True, timeout=30, check=False,
            )
            if result.returncode:
                raise RuntimeError(result.stderr.decode("utf-8", errors="replace"))
            return json.loads(result.stdout)

        call(["init", "--vault", vault, "--project", "benchmark"])
        for start in range(0, args.records, 64):
            call(["commit"], {"operations": [
                {"operation_id": f"fixture-{index}", "action": "remember", "candidate": {
                    "kind": "decision", "title": f"Компонент {index:04d}",
                    "body": f"Для компонента {index:04d} результаты тестов хранятся в отчёте CI.",
                    "source": "Синтетический корпус benchmark_hooks.py",
                }} for index in range(start, min(start + 64, args.records))
            ]})
        call(["index"])
        records = vault / "Codex Memory/Projects/benchmark/Records"
        if len(list(records.glob("*/*.md"))) != args.records:
            raise RuntimeError("Синтетический корпус сохранился не полностью")
        skip = project / "action.json"
        skip.write_text('{"action":"skip"}', encoding="utf-8")
        for sample in range(args.samples + 1):
            identity = {"session_id": "benchmark", "turn_id": f"sample-{sample}",
                        "cwd": str(project), "collaboration_mode": "default"}
            started = time.perf_counter_ns()
            prompt = call(["codex", "hook", "UserPromptSubmit"], {
                **identity, "hook_event_name": "UserPromptSubmit",
                "prompt": "Где хранятся результаты тестов компонента?",
            })
            prompt_ms = (time.perf_counter_ns() - started) / 1_000_000
            context = prompt.get("hookSpecificOutput", {}).get("additionalContext", "")
            token = re.search(r"stage --turn ([0-9a-f]{64}\.[0-9a-f]{32})", context)
            if token is None or "systemMessage" in prompt or len(context.encode()) > 16_000:
                raise RuntimeError("Хук не выдал корректный ограниченный контекст")
            if args.records and "результаты тестов" not in context:
                raise RuntimeError("Контрольная запись не попала в recall")
            started = time.perf_counter_ns()
            receipt = call(["codex", "stage", "--turn", token[1], "--file", skip])
            stage_ms = (time.perf_counter_ns() - started) / 1_000_000
            if receipt.get("staged") is not True:
                raise RuntimeError("Прямой запуск не сохранил пропуск")
            started = time.perf_counter_ns()
            stopped = call(["codex", "hook", "Stop"], {
                **identity, "hook_event_name": "Stop", "stop_hook_active": False,
                "last_assistant_message": "Готово.",
            })
            stop_ms = (time.perf_counter_ns() - started) / 1_000_000
            if stopped != {}:
                raise RuntimeError(f"Подтверждённый пропуск не завершился тихо: {stopped}")
            if sample:
                for key, value in zip(timings, (prompt_ms, stage_ms, stop_ms)):
                    timings[key].append(round(value, 3))
        if len(list(records.glob("*/*.md"))) != args.records:
            raise RuntimeError("Пропуск создал лишнюю запись")
    if fingerprint != hashlib.sha256(binary.read_bytes()).hexdigest():
        raise RuntimeError("Бинарник изменился во время измерения")
    data = {
        "schema": 1, "kind": "fmem-native-hooks", "binary_sha256": fingerprint,
        "environment": {"system": platform.system(), "machine": platform.machine(),
                        "release": platform.release(), "python": platform.python_version()},
        "settings": {"records": args.records, "samples": args.samples, "warmups": 1,
                     "cache": "existing FTS index; OS cache not cleared",
                     "included": "native process, JSON, recall and durable turn state",
                     "excluded": "Codex, shell wrapper, model and setup"},
        "results": {name: summarize(samples) for name, samples in timings.items()},
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(data["results"], ensure_ascii=False))


if __name__ == "__main__":
    main()
