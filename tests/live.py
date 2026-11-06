"""Opt-in end-to-end tests of the real agent binary in disposable workspaces."""
import argparse
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT.parent / "llm-api" / "tests"))
import live as api_live
import codex_session

SECRET_VARS = ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY",
               "CODEX_ACCESS_TOKEN", "CODEX_ACCOUNT_ID")


def credentials(provider, model):
    loaded = api_live.environment()
    if provider == "codex":
        loaded = codex_session.environment(loaded)
    if provider == "openrouter":
        entry = next(m for m in api_live.models(provider, loaded) if m["id"] == model)
        if not api_live.free(entry):
            raise RuntimeError("refusing an OpenRouter model not verified as :free with zero prices")
    env = {key: value for key, value in os.environ.items() if key not in SECRET_VARS}
    names = {"openai": ("OPENAI_API_KEY",), "anthropic": ("ANTHROPIC_API_KEY",),
             "openrouter": ("OPENROUTER_API_KEY",),
             "codex": ("CODEX_ACCESS_TOKEN", "CODEX_ACCOUNT_ID")}[provider]
    env.update({name: loaded[name] for name in names})
    return env


def sanitized(value, env):
    for name in SECRET_VARS:
        if env.get(name):
            value = value.replace(env[name], "[REDACTED]")
    return value


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", required=True, choices=["openai", "anthropic", "openrouter", "codex"])
    parser.add_argument("--model", required=True)
    parser.add_argument("--scenario", choices=["read", "deny", "approve", "code"], default="read")
    args = parser.parse_args()
    env = credentials(args.provider, args.model)
    binary = ROOT / "target" / "release" / "agent"
    marker = "AGENT_LIVE_" + secrets.token_hex(8)
    with tempfile.TemporaryDirectory(prefix="agent-live-") as directory:
        workspace = Path(directory)
        (workspace / "fixture.txt").write_text(marker + "\n")
        if args.scenario == "read":
            prompt = ("Use only read_file to read fixture.txt, then return its exact contents. "
                      "Do not invoke any other tool and do not change files.")
        elif args.scenario == "deny":
            prompt = ("Call write_file exactly once to create blocked.txt containing forbidden. "
                      "If approval is denied, do not retry or use other tools; reply DENIED and stop.")
        elif args.scenario == "code":
            if not sys.stdin.isatty():
                raise RuntimeError("code scenario requires a real terminal; never pass --yes")
            source = ("fn add(a: i32, b: i32) -> i32 { a - b }\n"
                      "#[test] fn positive() { assert_eq!(add(2, 3), 5); }\n"
                      "#[test] fn negative() { assert_eq!(add(-2, 5), 3); }\n")
            (workspace / "calc.rs").write_text(source)
            compile_command = ["rustc", "--test", "calc.rs", "-o", "calc_tests"]
            subprocess.run(compile_command, cwd=directory, check=True, capture_output=True)
            baseline = subprocess.run(["./calc_tests"], cwd=directory, capture_output=True, text=True)
            assert baseline.returncode == 101 and "2 failed" in baseline.stdout, "fixture must start failing"
            print(json.dumps({"scenario": "code", "baseline_exit": baseline.returncode, "failing_tests": 2}))
            prompt = ("Fix the bug in calc.rs so its existing tests pass. Read it first. "
                      "Use edit_file for the smallest fix; do not modify tests or create other source files. "
                      "Use shell with exactly: rustc --test calc.rs -o calc_tests && ./calc_tests. "
                      "Run that command before and after your edit, so you observe the failure and prove the fix. "
                      "Use normal approval prompts, no other shell commands. Stop after verification.")
        else:
            if not sys.stdin.isatty():
                raise RuntimeError("approve scenario requires a real terminal; never pass --yes")
            prompt = ("Create smoke.txt using write_file, containing exactly alpha followed by a newline. "
                      "Read it with read_file. Use edit_file to replace alpha with beta exactly once. "
                      "Then use shell with exactly: test \"$(cat smoke.txt)\" = beta && printf AGENT_VERIFY_OK. "
                      "Use the normal approval prompts. Do not invoke other shell commands or tools. "
                      "After the successful verification, stop.")
        command = [str(binary), "--provider", args.provider, "--model", args.model,
                   "--max-turns", "8" if args.scenario == "code" else "6" if args.scenario == "approve" else "3", prompt]
        if args.scenario == "code":
            result = subprocess.run(command, cwd=directory, env=env, timeout=240)
            assert result.returncode == 0, "agent exited with an error"
            assert (workspace / "calc.rs").read_text() == source.replace("a - b", "a + b"), "unexpected source or test changes"
            assert set(p.name for p in workspace.iterdir()) == {"fixture.txt", "calc.rs", "calc_tests"}, "unexpected files"
            # Recompile independently: do not trust the model's claimed test outcome or executable.
            subprocess.run(compile_command, cwd=directory, check=True, capture_output=True)
            checked = subprocess.run(["./calc_tests"], cwd=directory, capture_output=True, text=True)
            assert checked.returncode == 0 and "2 passed" in checked.stdout, "independent verification failed"
            print(json.dumps({"scenario": "code", "verification_exit": checked.returncode, "passing_tests": 2}))
        elif args.scenario == "approve":
            result = subprocess.run(command, cwd=directory, env=env, timeout=240)
            assert result.returncode == 0, "agent exited with an error"
            assert (workspace / "smoke.txt").read_text() == "beta\n", "wrong edit result"
            assert set(p.name for p in workspace.iterdir()) == {"fixture.txt", "smoke.txt"}, "unexpected files"
        else:
            result = subprocess.run(command, cwd=directory, env=env, stdin=subprocess.DEVNULL,
                                    capture_output=True, text=True, timeout=240)
            if result.returncode:
                raise RuntimeError(sanitized(result.stderr[:1500] + result.stdout[:500], env))
            if args.scenario == "read":
                assert marker in result.stdout, "final answer did not contain the unknown fixture value"
                assert "1\t" + marker in result.stderr, "no real read_file result observed"
                assert "approval requires a terminal" not in result.stderr, "unexpected mutating tool attempt"
            else:
                assert "write_file" in result.stderr, "write was not attempted"
                assert "approval requires a terminal" in result.stderr, "approval was not denied"
                assert "DENIED" in result.stdout, "model did not acknowledge denial"
            assert set(p.name for p in workspace.iterdir()) == {"fixture.txt"}, "workspace changed"
        assert (workspace / "fixture.txt").read_text() == marker + "\n", "fixture changed"
        print(json.dumps({"provider": args.provider, "model": args.model, "scenario": args.scenario,
                          "result": "PASS", "agent_exit": result.returncode,
                          "workspace": "disposable", "unrestricted_yes_flag": False}))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"result": "FAIL", "error": str(error)}))
        raise SystemExit(1)
