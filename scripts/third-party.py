#!/usr/bin/env python3
"""Writes the licence texts of every crate linked into the tern binary to one file.

    scripts/third-party.py <out.txt>

MIT, Apache-2.0 and BSD ask that their notice travel with the binary, so the bundle carries
this file beside LICENSE and NOTICE.md. Only normal dependencies for the macOS arm64 target
are listed (build scripts, proc macros' own build deps and dev-dependencies do not ship).
Reads crate sources from cargo's registry and vendor/; needs no network.
"""
import json
import os
import subprocess
import sys

TARGET = "aarch64-apple-darwin"
LICENCE_FILES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT", "UNLICENSE")


def main(out_path):
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET]
        )
    )
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    workspace = set(meta["workspace_members"])
    root = next(i for i in workspace if packages[i]["name"] == "tern")

    shipped, todo = set(), [root]
    while todo:
        node = nodes[todo.pop()]
        for dep in node["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]) and dep["pkg"] not in shipped:
                shipped.add(dep["pkg"])
                todo.append(dep["pkg"])

    crates = sorted(
        (packages[i] for i in shipped if i not in workspace),
        key=lambda p: (p["name"], p["version"]),
    )
    # Many crates carry the same MIT or Apache text: each distinct text is written once, with
    # every crate that ships it.
    texts, missing = {}, []
    for p in crates:
        src = os.path.dirname(p["manifest_path"])
        names = sorted(
            f
            for f in os.listdir(src)
            if f.upper().startswith(LICENCE_FILES) and os.path.isfile(os.path.join(src, f))
        )
        if p.get("license_file"):
            names.append(p["license_file"])
        if not names:
            missing.append(p)
        for name in dict.fromkeys(names):
            with open(os.path.join(src, name), encoding="utf-8", errors="replace") as f:
                text = f.read().strip()
            texts.setdefault(text, []).append(f"{p['name']} {p['version']}")
    with open(out_path, "w", encoding="utf-8") as out:
        out.write("Third-party software in tern\n")
        out.write("============================\n\n")
        out.write("tern itself is GPL-3.0-or-later (LICENSE). Code copied from other projects is\n")
        out.write("listed in NOTICE.md. The crates below are linked into the binary.\n\n")
        for p in crates:
            out.write(f"{p['name']} {p['version']}: {p.get('license') or 'see below'}\n")
        if missing:
            out.write("\nThese ship no licence file; their terms are the SPDX licence named, whose\n")
            out.write("standard text appears below, held by the authors listed:\n\n")
            for p in missing:
                authors = ", ".join(p.get("authors") or []) or "the crate's authors"
                out.write(f"{p['name']} {p['version']} ({p.get('license')}): {authors}\n")
        for text, users in texts.items():
            out.write(f"\n\n{'-' * 78}\nUsed by: {', '.join(users)}\n\n{text}\n")
    print(f"{len(crates)} crates written to {out_path}", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
