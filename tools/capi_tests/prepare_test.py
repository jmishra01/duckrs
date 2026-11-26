#!/usr/bin/env python3
"""Copies a C API test file of DuckDB, leaving out the test cases excluded.txt lists for it.

usage: prepare_test.py <excluded.txt> <test.cpp> <out.cpp>

A test case is left out only when it cannot compile without DuckDB's C++ internals (it builds a DuckDB C++
object, reads a C++ wrapper behind a handle, ...). Each line of excluded.txt is
`<file>|<test case name>|<why>`; a line starting with # is a comment.
"""
import re
import sys


def excluded_cases(path, test_file):
    cases = {}
    with open(path) as f:
        for line in f:
            line = line.rstrip("\n")
            if not line.strip() or line.startswith("#"):
                continue
            file, name, why = line.split("|", 2)
            if file == test_file:
                cases[name] = why
    return cases


def remove_case(text, name):
    """Removes `TEST_CASE("name", ...) { ... }` from text: from its start to the brace that closes its body."""
    marker = re.search(r'TEST_CASE\(\s*"' + re.escape(name) + '"', text)
    if not marker:
        raise SystemExit(f"test case {name!r} not found")
    start = marker.start()
    brace = text.index("{", marker.end())
    depth = 0
    i = brace
    while i < len(text):
        c = text[i]
        if c == '"' or c == "'":
            # skip a string or character literal
            quote = c
            i += 1
            while text[i] != quote:
                i += 2 if text[i] == "\\" else 1
        elif c == "/" and text[i + 1] == "/":
            i = text.index("\n", i)
            continue
        elif c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return text[:start] + f"// duckrs: test case {name!r} left out, see excluded.txt\n" + text[i + 1:]
        i += 1
    raise SystemExit(f"test case {name!r} has no end")


def main():
    excluded, source, target = sys.argv[1:4]
    test_file = source.rsplit("/", 1)[-1]
    with open(source) as f:
        text = f.read()
    for name in excluded_cases(excluded, test_file):
        text = remove_case(text, name)
    with open(target, "w") as f:
        f.write(text)


main()
