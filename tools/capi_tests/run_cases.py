#!/usr/bin/env python3
"""Runs every Catch2 test case of each test binary in a process of its own, and sums up the results per binary.

usage: run_cases.py <results.md> <binary>...

A test case passes when its process exits with 0. One that crashes or runs past the time limit fails, and its
assertions are not counted. The summary goes to stdout and to results.md; the output of each failing case goes to
<binary>.failures.txt.
"""
import os
import re
import shutil
import subprocess
import sys

TIMEOUT = int(os.environ.get("CAPI_TEST_TIMEOUT", "120"))


def test_cases(binary):
    out = subprocess.run([binary, "--list-test-names-only"], capture_output=True, text=True).stdout
    return [line.strip() for line in out.splitlines() if line.strip()]


def spec(name):
    # Catch2 test specs treat these characters specially
    return re.sub(r"([\\,\[\]*\"])", r"\\\1", name)


def assertions(output):
    m = re.search(r"assertions:\s*(\d+)\s*\|\s*(\d+) passed(?:\s*\|\s*(\d+) failed)?", output)
    if m:
        return int(m.group(2)), int(m.group(3) or 0)
    m = re.search(r"assertions:\s*(\d+)\s*\|\s*(\d+) failed", output)
    if m:
        return 0, int(m.group(2))
    m = re.search(r"All tests passed \((\d+) assertions?", output)
    if m:
        return int(m.group(1)), 0
    return 0, 0


def run(binary):
    cases = test_cases(binary)
    passed = failed = crashed = 0
    asserts_passed = asserts_failed = 0
    failures = []
    for case in cases:
        # each case starts with an empty test directory, as DuckDB's test runner gives it
        shutil.rmtree(os.environ.get("DUCKRS_CAPI_TEST_DIR", "duckrs_capi_test_dir"), ignore_errors=True)
        try:
            proc = subprocess.run([binary, spec(case)], capture_output=True, text=True, timeout=TIMEOUT,
                                  errors="replace")
            output = proc.stdout + proc.stderr
            code = proc.returncode
        except subprocess.TimeoutExpired as e:
            output = f"timed out after {TIMEOUT}s\n" + (e.stdout or b"").decode(errors="replace")
            code = None
        ok, bad = assertions(output)
        if code == 0:
            passed += 1
        elif code is None or code < 0 or code > 128 or "No test cases matched" in output:
            crashed += 1
            failures.append((case, f"[crashed or timed out, exit {code}]\n" + output))
        else:
            failed += 1
            failures.append((case, output))
        asserts_passed += ok
        asserts_failed += bad
    with open(binary + ".failures.txt", "w") as f:
        for case, output in failures:
            f.write(f"===== {case}\n{output}\n")
    return {
        "cases": len(cases),
        "passed": passed,
        "failed": failed,
        "crashed": crashed,
        "asserts_passed": asserts_passed,
        "asserts_failed": asserts_failed,
    }


def main():
    target, binaries = sys.argv[1], sys.argv[2:]
    lines = [
        "| test file | test cases | passed | failed | crashed | assertions passed | assertions failed |",
        "|---|---|---|---|---|---|---|",
    ]
    total = {k: 0 for k in ["cases", "passed", "failed", "crashed", "asserts_passed", "asserts_failed"]}
    for binary in binaries:
        r = run(binary)
        for k in total:
            total[k] += r[k]
        name = os.path.basename(binary) + ".cpp"
        line = (f"| {name} | {r['cases']} | {r['passed']} | {r['failed']} | {r['crashed']} | {r['asserts_passed']} |"
                f" {r['asserts_failed']} |")
        print(line, flush=True)
        lines.append(line)
    lines.append(f"| total | {total['cases']} | {total['passed']} | {total['failed']} | {total['crashed']} |"
                 f" {total['asserts_passed']} | {total['asserts_failed']} |")
    print(lines[-1])
    with open(target, "w") as f:
        f.write("\n".join(lines) + "\n")


main()
