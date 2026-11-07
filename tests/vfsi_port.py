#!/usr/bin/env python3
"""Compare real CLI predicates/formatting and assert VFSI traversal completed."""
import os
from pathlib import Path
import subprocess
import tempfile
binary = Path(__file__).resolve().parents[1] / "target/debug/find"
with tempfile.TemporaryDirectory(prefix="vfsi-find-") as directory:
    root = Path(directory)
    (root / "sub").mkdir()
    (root / "sub/file").write_text("data")
    (root / "alias").symlink_to("sub")
    for expression in [["-printf", "%p %D %F %i %U %G %m %n %s\\n"], ["-maxdepth", "0"], ["-depth"], ["-name", "sub", "-prune", "-o", "-print"], ["-size", "4c"]]:
        results = []
        for mode in ["off", "dummy"]:
            result = subprocess.run([str(binary), str(root), *expression], capture_output=True,
                env=dict(os.environ, LC_ALL="C", VNFS_IMPL=mode, VNFS_PROFILE="1"))
            assert result.returncode == 0, result.stderr
            if mode == "dummy": assert b"find_walk_completed" in result.stderr
            results.append(sorted(result.stdout.splitlines()))
        assert results[0] == results[1], (expression,results)
print("find VFSI metadata, pruning, and activation checks passed")

if os.geteuid() == 0:
    print("Permission-denied CLI regression requires an unprivileged user")
else:
    with tempfile.TemporaryDirectory(prefix="vfsi-find-recovery-") as directory:
        root = Path(directory) / "tree"
        denied = root / "a-denied"
        readable = root / "z-readable"
        denied.mkdir(parents=True)
        readable.mkdir()
        (denied / "hidden").write_text("not visible")
        (readable / "file").write_text("visible")
        denied.chmod(0)
        try:
            for expression in [[], ["-depth"], ["-exec", "sh", "-c",
                    'printf "%s\\n" "$1" >> "$2"', "sh", "{}", "LOG", ";", "-print"]]:
                outputs = []
                for mode in ["off", "dummy"]:
                    log = Path(directory) / f"effects-{mode}"
                    args = [str(log) if x == "LOG" else x for x in expression]
                    result = subprocess.run([str(binary), str(root), *args], capture_output=True,
                        env=dict(os.environ, LC_ALL="C", VNFS_IMPL=mode, VNFS_PROFILE="1"),
                        timeout=15)
                    assert result.returncode == 1, (mode, expression, result.stderr)
                    assert os.fsencode(denied) in result.stderr, result.stderr
                    assert os.fsencode(readable / "file") in result.stdout, result.stdout
                    if mode == "dummy":
                        assert b"find_walk_completed" in result.stderr, result.stderr
                    outputs.append((sorted(result.stdout.splitlines()),
                        sorted(log.read_bytes().splitlines()) if log.exists() else []))
                assert outputs[0] == outputs[1], (expression, outputs)
            print("Unreadable child, readable sibling, postorder and matcher side-effect parity passed")
        finally:
            denied.chmod(0o700)
