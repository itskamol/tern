#!/usr/bin/env python3
"""Direct stdio smoke test and TERN_LOG=info probe for `tern mcp`.

Examples:
  scripts/mcp-smoke.py --tools-only
  scripts/mcp-smoke.py --query prod
  scripts/mcp-smoke.py --run demo "sleep 70" --timeout 100 --progress-token p1
  scripts/mcp-smoke.py --run demo "sleep 30" --cancel-after 3
  scripts/mcp-smoke.py --debug --tools-only
"""

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time


def resolve_binary(args: argparse.Namespace) -> str:
    if args.bin:
        return args.bin
    target_dir = os.environ.get("CARGO_TARGET_DIR", "target")
    if args.debug:
        return os.path.join(target_dir, "debug", "tern")
    if args.release:
        return os.path.join(target_dir, "release", "tern")
    return "/Applications/tern.app/Contents/MacOS/tern"


def has_closed_additional_properties(schema: object) -> bool:
    if not isinstance(schema, dict):
        return False
    if schema.get("additionalProperties") is False:
        return True
    for value in schema.values():
        if has_closed_additional_properties(value):
            return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description="Smoke-test `tern mcp` over stdio with TERN_LOG.")
    parser.add_argument("--bin", help="Path to tern executable (default: /Applications/tern.app/...)")
    parser.add_argument("--debug", action="store_true", help="Use target/debug/tern")
    parser.add_argument("--release", action="store_true", help="Use target/release/tern")
    parser.add_argument("--log-level", default="info", help="TERN_LOG filter (default: info)")
    parser.add_argument(
        "--tools-only",
        action="store_true",
        help="Run only initialize + tools/list (works even when tern GUI is closed)",
    )
    parser.add_argument("--query", help="Optional query argument for list_hosts")
    parser.add_argument("--run", nargs=2, metavar=("HOST", "CMD"), help="Run one run_command call")
    parser.add_argument("--timeout", type=int, help="Optional timeout_secs for --run")
    parser.add_argument("--progress-token", help="Optional _meta.progressToken for --run")
    parser.add_argument(
        "--cancel-after",
        type=float,
        help="Send notifications/cancelled after N seconds during --run",
    )
    parser.add_argument(
        "--eof-after",
        type=float,
        help="Close stdin (EOF) after N seconds during --run",
    )
    args = parser.parse_args()

    binary = resolve_binary(args)
    if not os.path.exists(binary):
        print(f"mcp-smoke: binary not found: {binary}", file=sys.stderr)
        return 1

    env = os.environ.copy()
    env["TERN_LOG"] = args.log_level

    proc = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=env,
    )
    assert proc.stdin is not None and proc.stdout is not None and proc.stderr is not None

    stdout_q: queue.Queue[str | None] = queue.Queue()
    stderr_lines: list[str] = []

    def pump_stdout() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            stdout_q.put(line)
        stdout_q.put(None)

    def pump_stderr() -> None:
        assert proc.stderr is not None
        for line in proc.stderr:
            stderr_lines.append(line.rstrip("\n"))

    t_out = threading.Thread(target=pump_stdout, daemon=True)
    t_err = threading.Thread(target=pump_stderr, daemon=True)
    t_out.start()
    t_err.start()

    def send(payload: dict) -> None:
        assert proc.stdin is not None
        proc.stdin.write(json.dumps(payload) + "\n")
        proc.stdin.flush()

    def recv_for(req_id: int, wait_secs: float = 15.0) -> dict:
        deadline = time.monotonic() + wait_secs
        while True:
            remaining = max(0.05, deadline - time.monotonic())
            try:
                raw = stdout_q.get(timeout=remaining)
            except queue.Empty:
                raise TimeoutError(f"timed out waiting for response id={req_id}")
            if raw is None:
                raise EOFError(f"tern mcp closed stdout before response id={req_id}")
            msg = json.loads(raw)
            if msg.get("method") == "notifications/progress":
                p = msg.get("params", {})
                print(f"[progress #{p.get('progress')}] {p.get('message')}")
                continue
            if msg.get("id") == req_id:
                return msg

    exit_code = 0
    try:
        send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "clientInfo": {"name": "mcp-smoke", "version": "1.0"},
                },
            }
        )
        init = recv_for(1)
        info = init.get("result", {}).get("serverInfo", {})
        proto = init.get("result", {}).get("protocolVersion")
        print(f"initialize: {info.get('name')} {info.get('version')} (protocol {proto})")

        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        tools_resp = recv_for(2)
        tools = tools_resp.get("result", {}).get("tools", [])
        tool_names = [t.get("name") for t in tools]
        print(f"tools/list: {', '.join(tool_names)}")

        for tool in tools:
            out_schema = tool.get("outputSchema")
            if has_closed_additional_properties(out_schema):
                print(
                    f"ERROR: {tool.get('name')} outputSchema sets additionalProperties=false",
                    file=sys.stderr,
                )
                exit_code = 1

        list_tool = next((t for t in tools if t.get("name") == "list_hosts"), None)
        host_props = (
            (list_tool or {})
            .get("outputSchema", {})
            .get("properties", {})
            .get("hosts", {})
            .get("items", {})
            .get("properties", {})
        )
        if "group" not in host_props or "tags" not in host_props:
            print(
                f"ERROR: list_hosts outputSchema missing group/tags: {sorted(host_props)}",
                file=sys.stderr,
            )
            exit_code = 1
        else:
            print(f"list_hosts output fields: {', '.join(sorted(host_props))}")

        if not args.tools_only:
            lh_args = {"query": args.query} if args.query else {}
            send(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {"name": "list_hosts", "arguments": lh_args},
                }
            )
            lh_resp = recv_for(3, wait_secs=15.0)
            result = lh_resp.get("result", {})
            if result.get("isError"):
                print(f"list_hosts error: {result.get('structuredContent')}", file=sys.stderr)
                exit_code = 1
            else:
                hosts = result.get("structuredContent", {}).get("hosts", [])
                print(f"list_hosts: {len(hosts)} host(s)")
                for h in hosts[:5]:
                    grp = f" group={h['group']}" if "group" in h else ""
                    print(f"  - {h['alias']} ({h['user']}@{h['host']}:{h['port']}){grp} tags={h['tags']}")

        if args.run:
            host, cmd = args.run
            rc_args: dict[str, object] = {"host": host, "command": cmd}
            if args.timeout is not None:
                rc_args["timeout_secs"] = args.timeout
            params: dict[str, object] = {"name": "run_command", "arguments": rc_args}
            if args.progress_token:
                params["_meta"] = {"progressToken": args.progress_token}
            started = time.monotonic()
            send({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": params})

            if args.cancel_after is not None:
                time.sleep(args.cancel_after)
                send(
                    {
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": {"requestId": 4},
                    }
                )
                print(f"sent notifications/cancelled after {time.monotonic() - started:.2f}s")
                send({"jsonrpc": "2.0", "id": 5, "method": "ping"})
                recv_for(5, wait_secs=5.0)
            elif args.eof_after is not None:
                time.sleep(args.eof_after)
                proc.stdin.close()
                print(f"closed stdin (EOF) after {time.monotonic() - started:.2f}s")
            else:
                wait_limit = float((args.timeout or 30) + 75)
                rc_resp = recv_for(4, wait_secs=wait_limit)
                elapsed = time.monotonic() - started
                res = rc_resp.get("result", {})
                sc = res.get("structuredContent", {})
                print(f"run_command ({elapsed:.2f}s, isError={res.get('isError')}): {json.dumps(sc)}")
    except Exception as exc:
        print(f"mcp-smoke failed: {exc}", file=sys.stderr)
        exit_code = 1
    finally:
        if not proc.stdin.closed:
            proc.stdin.close()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        t_out.join(timeout=1)
        t_err.join(timeout=1)

    if stderr_lines:
        print(f"stderr logs ({len(stderr_lines)}):")
        for line in stderr_lines:
            print(f"  {line}")
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
