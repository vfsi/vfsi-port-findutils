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
