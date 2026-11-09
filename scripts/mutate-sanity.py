"""Мутации по проверяемым контрактам: каждая обязана дать красный на своём тесте.

Откат — обратной правкой, а не `git checkout`: файл в индексе не лежит, и checkout
на неотслеживаемом файле молча ничего не делает.
"""

import re
import subprocess
import sys

REPO = "/home/avostrikov/DevOps/tools/tasq"
DOCKER = [
    "docker", "run", "--rm",
    "-v", f"{REPO}:/src",
    "-v", "/home/avostrikov/.cache/tasq/cargo:/cache/cargo",
    "-v", "/home/avostrikov/.cache/tasq/target:/cache/target",
    "-v", "/home/avostrikov/.cache/tasq/home:/cache/home",
    "-u", f"{subprocess.run(['id','-u'],capture_output=True,text=True).stdout.strip()}:"
          f"{subprocess.run(['id','-g'],capture_output=True,text=True).stdout.strip()}",
    "-w", "/src",
    "-e", "CARGO_HOME=/cache/cargo",
    "-e", "CARGO_TARGET_DIR=/cache/target",
    "-e", "HOME=/cache/home",
    "tasq-builder:1.99.1",
]

MUTATIONS = [
    {
        "file": "src/db.rs",
        "old": "    Ok(log - copied)",
        "new": "    Ok(0i64)",
        "why": "остаток кадров всегда ноль (окно потерь объявлено закрытым)",
        "tests": ["db::tests::a_held_reader_leaves_pending_frames"],
    },
    {
        "file": "src/db.rs",
        "old": 'conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {',
        "new": 'conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {',
        "why": "журнал не усекается (PASSIVE вместо TRUNCATE)",
        "tests": ["db::tests::checkpoint_moves_frames_into_the_database"],
    },
    {
        "file": "src/error.rs",
        "old": '            Error::LeaseRequired => "LEASE_REQUIRED",',
        "new": '            Error::LeaseRequired => "NO_PROJECT",',
        "why": "два разных отказа получили один код",
        "tests": ["error::tests::every_error_has_a_unique_machine_code"],
    },
    {
        "file": "src/error.rs",
        "old": '            Error::NoProject => "run `tasq init` to register a project",',
        "new": '            Error::NoProject => "",',
        "why": "подсказка у варианта отключена",
        "tests": ["error::tests::every_error_carries_a_hint"],
    },
    {
        "file": "src/mcp/mod.rs",
        "old": "if self.silent_ticks >= Self::TICKS_BEFORE_WARNING && !self.warned {",
        "new": "if self.silent_ticks > Self::TICKS_BEFORE_WARNING && !self.warned {",
        "why": "порог тиков сдвинут с >= на >",
        "tests": ["mcp::tests::a_long_stall_warns_once"],
    },
    {
        "file": "src/mcp/mod.rs",
        "old": "            self.warned = true;\n            return Some(",
        "new": "            self.warned = false;\n            return Some(",
        "why": "предупреждение не помечается выданным",
        "tests": ["mcp::tests::a_long_stall_warns_once"],
    },
]


def run_tests(names):
    cmd = DOCKER + ["cargo", "test", "--lib", "--"] + names
    out = subprocess.run(cmd, capture_output=True, text=True, cwd=REPO)
    return out.returncode, out.stdout + out.stderr


def main():
    failures = 0
    for index, mutation in enumerate(MUTATIONS, start=1):
        path = f"{REPO}/{mutation['file']}"
        original = open(path, encoding="utf-8").read()
        if mutation["old"] not in original:
            print(f"[{index}] МУТАЦИЯ НЕ ЛЕГЛА: не найдено {mutation['old']!r}")
            failures += 1
            continue
        mutated = original.replace(mutation["old"], mutation["new"], 1)
        assert mutated != original, "замена не изменила файл"
        open(path, "w", encoding="utf-8").write(mutated)

        # Проверка, что мутация действительно на диске.
        landed = mutation["new"] in open(path, encoding="utf-8").read()
        code, log = run_tests(mutation["tests"])
        compiled_error = "could not compile" in log
        red = code != 0 and not compiled_error

        # Откат обратной правкой + проверка, что мутации больше нет.
        open(path, "w", encoding="utf-8").write(original)
        gone = open(path, encoding="utf-8").read() == original

        status = "УБИТА" if red else ("НЕ СОБРАЛАСЬ" if compiled_error else "ВЫЖИЛА")
        print(f"[{index}] {status}: {mutation['why']}  (легла={landed}, снята={gone})")
        if not red:
            failures += 1
            tail = [line for line in log.splitlines() if "test result" in line or "error[" in line]
            print("     ", " | ".join(tail[:3]))

    print()
    print(f"итог: {len(MUTATIONS) - failures} из {len(MUTATIONS)} убиты")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
