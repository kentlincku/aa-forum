"""Run the preserved reviewer probes with the native Darwin EPERM interposer.
Usage: python3 run_probes.py --output <scratch directory> [--repeat 3]
Builds real test executables, never injects into Cargo or an unrelated process.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--repeat", type=int, default=3)
args = parser.parse_args()
assert platform.system() == "Darwin", "native interposer is Darwin-only"
root = Path(__file__).resolve().parents[6]
fixtures = Path(__file__).resolve().parent
args.output.mkdir(parents=True, exist_ok=True)


def build(package, test):
    result = subprocess.run(["cargo", "test", "-p", package, "--test", test,
                             "--no-run", "--message-format=json"], cwd=root,
                            capture_output=True, text=True, check=True)
    binaries = [row["executable"] for line in result.stdout.splitlines()
                if (row := json.loads(line)).get("reason") == "compiler-artifact"
                and row.get("executable") and row["target"]["name"] == test]
    assert len(binaries) == 1, binaries
    return binaries[0]


binary = build("aaf-feature-context", "native_faults")
owned = build("aaf-runtime", "owned_stop")
shim = args.output / "deny_group.dylib"
subprocess.run(["clang", "-dynamiclib", "-o", str(shim), str(fixtures / "deny_group.c")], check=True)
results = []
probes = [
    (binary, "native_idle_recorder_fault_blocks_before_next_effect", None, False),
    (binary, "native_steering_recorder_failure_must_not_ack", None, False),
    (binary, "native_steering_rotation_counts_input_once", None, False),
    (binary, "native_native_acp_group_failure_refuses_switch", "1", True),
    (binary, "native_native_acp_group_failure_refuses_switch", "0", True),
    (owned, "native_eperm_retains_connection_registry_and_unreaped_leader", "1", True),
]
for exe, name, inject, ignored in probes:
    for index in range(args.repeat):
        env = dict(os.environ)
        if inject is not None:
            env.update(AAF_REVIEW_DENY_GROUP=inject, DYLD_INSERT_LIBRARIES=str(shim))
        command = [exe, "--exact", name, "--nocapture"] + (["--ignored"] if ignored else [])
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=30)
        text = result.stdout + result.stderr
        log = args.output / f"{name}-inject{inject}-{index}.log"
        log.write_text(text)
        row = {"test": name, "injected": inject, "run": index, "exit": result.returncode, "log": str(log)}
        results.append(row)
        (args.output / "results.json").write_text(json.dumps(results, indent=2))
        assert result.returncode == 0 and "test result: ok." in text, text
        if inject == "1":
            assert "REVIEW_INJECT killpg(" in text, "interposer was not exercised"
        if name == "native_native_acp_group_failure_refuses_switch":
            assert "NATIVE_CLEANUP child_birth_after=None" in text, text
print(json.dumps({"runs": len(results), "failed": sum(r["exit"] != 0 for r in results)}, indent=2))
