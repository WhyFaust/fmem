from __future__ import annotations

import json
import os
import queue
import signal
import subprocess
import threading
import time
from collections import deque
from pathlib import Path


class AppServer:
    def __init__(self, executable: Path, cwd: Path, env: dict[str, str], log: Path) -> None:
        self.log = log
        with self.log.open("w", encoding="utf-8") as stderr:
            self.process = subprocess.Popen(
                [executable, "app-server", "--listen", "stdio://"],
                cwd=cwd,
                env=env,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=stderr,
                text=True,
                encoding="utf-8",
                start_new_session=True,
            )
        self.messages: queue.Queue[dict | None] = queue.Queue()
        self.notifications: deque[dict] = deque()
        self.serial = 0
        self.reader = threading.Thread(target=self.read_stdout, daemon=True)
        self.reader.start()

    def __enter__(self) -> AppServer:
        return self

    def __exit__(self, *_args: object) -> None:
        self.close()

    def read_stdout(self) -> None:
        assert self.process.stdout is not None
        try:
            for line in self.process.stdout:
                try:
                    self.messages.put(json.loads(line))
                except json.JSONDecodeError:
                    self.messages.put({"protocol_error": "Ответ Codex app-server не является JSON"})
        finally:
            self.messages.put(None)

    def send(self, message: dict) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
        self.process.stdin.flush()

    def receive(self, deadline: float) -> dict:
        try:
            message = self.messages.get(timeout=max(0, deadline - time.monotonic()))
        except queue.Empty as exc:
            raise TimeoutError("Истекло время ожидания Codex app-server") from exc
        if message is None:
            raise RuntimeError("Codex app-server завершился")
        if "protocol_error" in message:
            raise RuntimeError(message["protocol_error"])
        if "method" in message and "id" in message:
            self.send({"id": message["id"], "error": {"code": -32601, "message": "Unsupported in fixture"}})
            raise RuntimeError(f"Codex запросил интерактивное действие {message['method']}")
        return message

    def request(self, method: str, params: dict, deadline: float) -> dict:
        self.serial += 1
        self.send({"id": self.serial, "method": method, "params": params})
        while True:
            reply = self.receive(deadline)
            if reply.get("id") != self.serial:
                self.notifications.append(reply)
                continue
            if "error" in reply:
                raise RuntimeError(f"{method} вернул ошибку: {reply['error']}")
            return reply["result"]

    def initialize(self, deadline: float) -> None:
        self.request(
            "initialize",
            {
                "clientInfo": {"name": "fmem_hooks_fixture", "version": "1.0.0"},
                "capabilities": {"experimentalApi": True},
            },
            deadline,
        )
        self.send({"method": "initialized", "params": {}})

    def next_notification(self, deadline: float) -> dict:
        if self.notifications:
            return self.notifications.popleft()
        return self.receive(deadline)

    def close(self) -> None:
        if self.process.poll() is None:
            try:
                os.killpg(self.process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)
        self.reader.join(timeout=5)
        for stream in (self.process.stdin, self.process.stdout):
            if stream:
                stream.close()
