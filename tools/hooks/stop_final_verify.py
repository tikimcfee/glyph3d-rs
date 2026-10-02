#!/usr/bin/env python3
"""Stop hook: Verifies that tests are green before allowing the agent to terminate."""
import sys
import json
import subprocess

def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        print(json.dumps({"decision": "allow"}))
        return

    import os
    env = os.environ.copy()
    env["CI"] = "1"
    res = subprocess.run(
        ["cargo", "nextest", "run", "-E", "not test(a_seeded_prefix)"],
        capture_output=True,
        text=True,
        env=env,
    )

    if res.returncode != 0:
        # Prevent stopping and inject error back to agent
        err_msg = res.stderr.strip() or res.stdout.strip()
        print(json.dumps({
            "decision": "continue",
            "reason": f"Cannot finish: cargo nextest failed with exit code {res.returncode}. Please resolve test failures before concluding.\nOutput:\n{err_msg[-1000:]}"
        }))
        return

    print(json.dumps({"decision": "allow"}))

if __name__ == "__main__":
    main()
