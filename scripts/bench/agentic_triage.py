#!/usr/bin/env python3
"""Classify agentic benchmark failures from local run artifacts.

The script is intentionally heuristic: it names the failure shape so a red matrix
cell is not treated as model-quality evidence until delivery/setup/tooling causes
have been ruled out.
"""

from __future__ import annotations

import argparse
import csv
import json
import re
import sys
from pathlib import Path
from typing import Any


FIELDS = [
    "path",
    "class",
    "reason",
    "signals",
    "task",
    "agent",
    "model",
    "pass",
    "timeout",
    "rc",
]

MAX_TEXT_BYTES = 2_000_000
MAX_SOURCE_BYTES = 200_000

TOOL_ERROR_RE = re.compile(
    r'("is_error"\s*:\s*true|<tool_use_error>|tool_use_error|tool execution failed|error calling tool)',
    re.IGNORECASE,
)
SUCCESS_AFTER_ERROR_RE = re.compile(
    r"\b(all tests? (?:pass|passed|green)|success(?:fully)?|fixed|done|works|prints?\s+(?:olleh|35|14))\b",
    re.IGNORECASE,
)
MANIFEST_INVALID_RE = re.compile(
    r"(failed to parse manifest|failed to parse the edition key|this version of cargo is older|unknown edition)",
    re.IGNORECASE,
)
EDIT_REQUIRES_READ_RE = re.compile(r"file has not been read yet", re.IGNORECASE)
EDIT_OLD_STRING_RE = re.compile(r"string to replace not found", re.IGNORECASE)
COMPILE_ERROR_RE = re.compile(
    r"(error\[E[0-9]{4}\]|could not compile|^error: (?!test failed\b))",
    re.IGNORECASE | re.MULTILINE,
)
TIMEOUT_RE = re.compile(r"(RUN_TIMEOUT|timed out|timeout|max turns|error_max_turns)", re.IGNORECASE)

SOURCE_ARTIFACTS = [
    re.compile(r"\b(?:collect|args|else|if|match|return|let|fn|use|mod|impl|struct|enum)\d+\b"),
    re.compile(r"\bVec::new\d+\s*\("),
    re.compile(r"\.(?:unwrap|collect|parse|chars|rev);"),
    re.compile(r"(?m)^\s*\d+\s*;\s*$"),
]


def read_sample(path: Path, max_bytes: int = MAX_TEXT_BYTES) -> str:
    if not path.is_file():
        return ""
    try:
        size = path.stat().st_size
        with path.open("rb") as f:
            if size <= max_bytes:
                data = f.read()
            else:
                head_len = min(256_000, max_bytes // 4)
                tail_len = max_bytes - head_len
                head = f.read(head_len)
                f.seek(max(0, size - tail_len))
                tail = f.read(tail_len)
                data = head + b"\n... <truncated> ...\n" + tail
        return data.decode("utf-8", errors="replace")
    except OSError:
        return ""


def one_line(text: str, limit: int = 220) -> str:
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) <= limit:
        return text
    return text[: limit - 1].rstrip() + "..."


def result(
    path: Path | str,
    cls: str,
    reason: str,
    signals: list[str] | None = None,
    *,
    task: str = "",
    agent: str = "",
    model: str = "",
    passed: str = "",
    timeout: str = "",
    rc: str = "",
) -> dict[str, Any]:
    return {
        "path": str(path),
        "class": cls,
        "reason": one_line(reason),
        "signals": signals or [],
        "task": task,
        "agent": agent,
        "model": model,
        "pass": passed,
        "timeout": timeout,
        "rc": rc,
    }


def read_meta(workdir: Path) -> dict[str, str]:
    meta: dict[str, str] = {}
    for name in ("agentic.meta", "agentic.env"):
        text = read_sample(workdir / name, 32_000)
        for line in text.splitlines():
            if "=" not in line or line.lstrip().startswith("#"):
                continue
            key, value = line.split("=", 1)
            meta[key.strip()] = value.strip()
    return meta


def src_is_stub(path: Path) -> bool:
    text = read_sample(path, 16_000)
    return "Hello, world!" in text and len(text.splitlines()) <= 8


def source_text(workdir: Path) -> str:
    chunks: list[str] = []
    candidates = []
    src = workdir / "src"
    if src.is_dir():
        candidates.extend(sorted(src.glob("*.rs")))
    candidates.extend(sorted(workdir.glob("*.rs")))
    for path in candidates:
        chunks.append(f"\n--- {path.name} ---\n{read_sample(path, MAX_SOURCE_BYTES)}")
    return "\n".join(chunks)


def first_match_line(text: str, pattern: re.Pattern[str]) -> str:
    match = pattern.search(text)
    if not match:
        return ""
    start = text.rfind("\n", 0, match.start()) + 1
    end = text.find("\n", match.end())
    if end < 0:
        end = len(text)
    return one_line(text[start:end])


def last_tool_error_then_success(log_text: str) -> bool:
    matches = list(TOOL_ERROR_RE.finditer(log_text))
    if not matches:
        return False
    last_error_end = matches[-1].end()
    tail = log_text[last_error_end : last_error_end + 8_000]
    return bool(SUCCESS_AFTER_ERROR_RE.search(tail))


def classify_workdir(path: Path, explicit_log: Path | None = None) -> dict[str, Any]:
    meta = read_meta(path)
    task = meta.get("task", "")
    agent = meta.get("agent", "")
    model = meta.get("model", "")
    rc = meta.get("rc", "")
    timeout = meta.get("timeout", "")
    passed = meta.get("pass", "")

    log_path = explicit_log or path / "agent.log"
    log_text = read_sample(log_path)
    verify_text = read_sample(path / "verify.out", 200_000)
    cargo_text = read_sample(path / "cargo.err", 200_000)
    run_text = read_sample(path / "run.err", 200_000)
    cargo_toml_text = read_sample(path / "Cargo.toml", 200_000)
    combined = "\n".join([log_text, verify_text, cargo_text, run_text, cargo_toml_text])

    if passed == "1" or (verify_text and "FAIL" not in verify_text and "PASS" in verify_text):
        return result(path, "pass", "verification passed", ["verify_pass"], task=task, agent=agent, model=model, passed="1", timeout=timeout, rc=rc)

    needs_rust_project = task != "greet"
    cargo_toml = path / "Cargo.toml"
    src_dir = path / "src"
    src_files = sorted(src_dir.glob("*.rs")) if src_dir.is_dir() else []
    root_rs = sorted(path.glob("*.rs"))
    main_rs = src_dir / "main.rs"
    main_stub = main_rs.is_file() and src_is_stub(main_rs)

    if needs_rust_project and not cargo_toml.is_file():
        return result(
            path,
            "missing_project_files",
            "Cargo.toml is missing, so this is a delivery/setup failure before Rust quality can be judged",
            ["missing_cargo_toml"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    if root_rs and (not main_rs.is_file() or main_stub):
        rels = ", ".join(p.name for p in root_rs[:4])
        signal = "src_main_stub" if main_stub else "src_main_missing"
        return result(
            path,
            "wrong_entrypoint",
            f"root-level Rust file(s) {rels} exist, but Cargo runs src/main.rs",
            [signal, "root_level_rs"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    if needs_rust_project and not src_files:
        return result(
            path,
            "missing_project_files",
            "no src/*.rs files exist, so Cargo has no binary/library source to build",
            ["missing_src_rs"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    if MANIFEST_INVALID_RE.search(combined):
        return result(
            path,
            "manifest_invalid",
            f"Cargo manifest is invalid; inspect {cargo_toml}",
            ["cargo_manifest_parse_error"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    if EDIT_REQUIRES_READ_RE.search(log_text):
        return result(
            path,
            "edit_requires_read",
            "agent attempted Edit before satisfying the required Read-before-Edit protocol",
            ["edit_without_read"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    if EDIT_OLD_STRING_RE.search(log_text):
        return result(
            path,
            "edit_old_string_miss",
            "agent used an Edit.old_string that did not match the current file",
            ["edit_old_string_miss"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    if last_tool_error_then_success(log_text):
        return result(
            path,
            "false_success_after_error",
            "last tool failure is followed by a success claim in the assistant log",
            ["tool_error", "success_claim"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    src_text = source_text(path)
    for pattern in SOURCE_ARTIFACTS:
        line = first_match_line(src_text, pattern)
        if line:
            return result(
                path,
                "source_syntax_artifact",
                f"source contains shell/edit corruption artifact: {line}",
                ["source_artifact"],
                task=task,
                agent=agent,
                model=model,
                passed="0",
                timeout=timeout,
                rc=rc,
            )

    if COMPILE_ERROR_RE.search(combined):
        line = first_match_line(combined, COMPILE_ERROR_RE) or "compiler error found in run artifacts"
        return result(
            path,
            "compile_error",
            line,
            ["compiler_error"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    if verify_text and "FAIL" in verify_text:
        fail_lines = [line.strip() for line in verify_text.splitlines() if "FAIL" in line]
        reason = fail_lines[0] if fail_lines else "verification failed after delivery/setup checks passed"
        return result(
            path,
            "verifier_mismatch",
            reason,
            ["verify_fail"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout=timeout,
            rc=rc,
        )

    if timeout == "1" or TIMEOUT_RE.search(combined):
        return result(
            path,
            "timeout",
            "run reached the agentic timeout or max-turn ceiling",
            ["timeout"],
            task=task,
            agent=agent,
            model=model,
            passed="0",
            timeout="1",
            rc=rc,
        )

    return result(
        path,
        "unknown_failed",
        "no concrete delivery signal found in local artifacts; treat as model-quality evidence only after rerun/triage",
        ["unknown"],
        task=task,
        agent=agent,
        model=model,
        passed=passed or "0",
        timeout=timeout,
        rc=rc,
    )


def normalize_bool(value: str | None) -> str:
    value = (value or "").strip().lower()
    if value in {"1", "true", "yes"}:
        return "1"
    if value in {"0", "false", "no"}:
        return "0"
    return ""


def classify_per_run_csv(path: Path) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    try:
        with path.open(newline="", encoding="utf-8") as f:
            reader = csv.DictReader(f)
            for idx, row in enumerate(reader, start=2):
                passed = normalize_bool(row.get("pass"))
                timeout = normalize_bool(row.get("timeout"))
                rc = row.get("rc", "") or ""
                row_path = f"{path}:{idx}"
                if passed == "1":
                    cls = "pass"
                    reason = "per-run row passed"
                    signals = ["per_run_pass"]
                elif timeout == "1":
                    cls = "timeout"
                    reason = "per-run row timed out; pass the kept workdir or agent.log for deeper triage"
                    signals = ["per_run_timeout", "no_workdir_path"]
                else:
                    cls = "unknown_failed"
                    reason = "per-run row failed, but CSV does not record the kept workdir path"
                    signals = ["per_run_fail", "no_workdir_path"]
                out.append(
                    result(
                        row_path,
                        cls,
                        reason,
                        signals,
                        task=row.get("task", "") or "",
                        agent=row.get("agent", "") or "",
                        model=row.get("model", "") or "",
                        passed=passed,
                        timeout=timeout,
                        rc=rc,
                    )
                )
    except OSError as exc:
        out.append(result(path, "unreadable", str(exc), ["io_error"]))
    return out


def classify_path(path: Path) -> list[dict[str, Any]]:
    if path.is_file():
        if path.name == "per-run.csv":
            return classify_per_run_csv(path)
        if path.name == "agent.log":
            return [classify_workdir(path.parent, path)]
        return [result(path, "unknown_failed", "file type is not a recognized agentic artifact", ["unknown_file"])]

    if path.is_dir():
        per_run = path / "per-run.csv"
        if per_run.is_file():
            return classify_per_run_csv(per_run)
        return [classify_workdir(path)]

    return [result(path, "unreadable", "path does not exist", ["missing_path"])]


def format_text(rows: list[dict[str, Any]]) -> str:
    lines = [f"{'CLASS':<28} {'TASK':<8} {'PATH':<64} REASON"]
    for row in rows:
        lines.append(f"{row['class']:<28} {row.get('task', ''):<8} {one_line(str(row['path']), 64):<64} {row['reason']}")
    return "\n".join(lines)


def csv_row(row: dict[str, Any]) -> dict[str, str]:
    converted: dict[str, str] = {}
    for field in FIELDS:
        value = row.get(field, "")
        if isinstance(value, list):
            converted[field] = ";".join(str(x) for x in value)
        else:
            converted[field] = "" if value is None else str(value)
    return converted


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", default=["."], help="result dir, kept workdir, agent.log, or per-run.csv")
    parser.add_argument("--root", default=".", help="base directory for relative paths")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--json", action="store_true", help="write a JSON array")
    group.add_argument("--csv", action="store_true", help="write stable CSV")
    group.add_argument("--brief", action="store_true", help="write 'class: reason' for the first run")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    root = Path(args.root).expanduser().resolve()
    rows: list[dict[str, Any]] = []
    for raw in args.paths:
        path = Path(raw).expanduser()
        if not path.is_absolute():
            path = root / path
        rows.extend(classify_path(path.resolve()))

    if args.brief:
        if rows:
            first = rows[0]
            print(f"{first['class']}: {first['reason']}")
        return 0

    if args.json:
        print(json.dumps(rows, indent=2, ensure_ascii=False))
        return 0

    if args.csv:
        writer = csv.DictWriter(sys.stdout, fieldnames=FIELDS, extrasaction="ignore")
        writer.writeheader()
        for row in rows:
            writer.writerow(csv_row(row))
        return 0

    print(format_text(rows))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
