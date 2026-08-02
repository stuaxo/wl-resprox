"""`test crash` / `test swap` / `test smoke` / `test matrix`.

Named testing.py, not test.py -- a flat `test.py` on sys.path would
shadow/collide with CPython's own stdlib `test` package namespace.
"""

from __future__ import annotations

import os
import shlex
import shutil
import subprocess
import tempfile
import time
from datetime import datetime
from pathlib import Path
from typing import List, Optional

import typer

from .. import common
from . import diagnose as diagnose_cmd
from . import env

app = typer.Typer(no_args_is_help=True, help="Crash/reconnect testing.")


@app.command("crash")
def crash(
    wm: str = common.wm_option("Which compositor's container to run the check in."),
    client: str = typer.Option(common.DEFAULT_CLIENT, "--client", help="Client application to test with."),
    verify_recovery: bool = typer.Option(
        False,
        "--verify-recovery",
        help="Also restart the compositor afterward and verify protocol-level "
        "recovery from the proxy's log, not just that the client survived.",
    ),
) -> None:
    """Crashes the compositor inside a running container and confirms
    the client survives (optionally verifying full protocol recovery)."""
    container = common.container_name(wm)
    inner_args = ["--l1"] if verify_recovery else []
    inner_args.append(client)
    inner_cmd = "cd " + shlex.quote(common.HARNESS_CONTAINER_ROOT) + " && bash test-crash.sh " + " ".join(
        shlex.quote(a) for a in inner_args
    )
    result = common.run_bash_snippet(
        inner_cmd,
        container=container,
        env={"XDG_RUNTIME_DIR": common.CONTAINER_RUNTIME_DIR},
        user="dev",
    )
    raise typer.Exit(result.returncode)


class _SwapFail(Exception):
    pass


@app.command("swap")
def swap(
    from_wm: str = typer.Option(..., "--from", help="Compositor that starts first and gets crashed."),
    to_wm: str = typer.Option(..., "--to", help="Compositor started fresh afterward, on the same freed socket."),
    client: str = typer.Option(common.DEFAULT_CLIENT, "--client", help="Client application to test with."),
) -> None:
    """Cross-compositor swap check: crashes --from's compositor and
    brings up --to's in its place, in a different container. Both
    containers must already exist and be running (`env setup`)."""
    from_container = common.container_name(from_wm)
    to_container = common.container_name(to_wm)
    for c in (from_container, to_container):
        if not common.container_exists(c):
            common.fail(f"container '{c}' doesn't exist -- run `wayland-headless-harness env setup --wm=<name>` first.")
        if not common.container_running(c):
            common.fail(f"container '{c}' exists but isn't running -- `env setup --wm=<name>` starts it.")

    runtime_dir = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime_dir:
        common.fail("XDG_RUNTIME_DIR must be set -- run this from the host, not inside a container")

    proxy_display = "wayland-proxy-0"  # fixed name the proxy binds by default, see src/cli.rs

    # Per ADR-0003, the harness never builds the proxy from source
    # itself. On the host (unlike inside a container) that means the
    # release binary `cargo deb` already produced as a build byproduct --
    # both containers already had to exist (checked above), which means
    # `env setup` already ran `cargo deb` for at least one of them.
    if common.PROJECT_ROOT is None:
        common.fail(
            "no wayland-proxy checkout found -- `test swap`'s host-launched proxy needs "
            "target/release/wayland-proxy, a cargo-deb build byproduct (run `env setup --wm=<name>` first)"
        )
    proxy_bin = common.PROJECT_ROOT / "target" / "release" / "wayland-proxy"
    if not os.access(proxy_bin, os.X_OK):
        common.fail(f"{proxy_bin} not found -- run `env setup --wm=<name>` first (it builds this as a byproduct of packaging the .deb)")

    # run-registry.sh/socket-wait.sh stay Bash, unmodified -- diagnose.sh
    # (also unmodified) reads run-registry.sh's on-disk pid-file format
    # directly, so nothing here reimplements any of that logic, it just
    # invokes the same functions the container-side pieces below do,
    # now also for this host-side proxy.
    run_registry_sh = common.BASH_SCRIPT_DIR / "run-registry.sh"
    socket_wait_sh = common.BASH_SCRIPT_DIR / "socket-wait.sh"

    run_dir = common.bash_capture(
        f"source {shlex.quote(str(run_registry_sh))} && run_dir_init >/dev/null && echo \"$RUN_DIR\""
    ).strip()
    if not run_dir:
        common.fail("couldn't determine RUN_DIR from run_dir_init")
    typer.echo(f"Run directory: {run_dir}")

    def registry_call(fn_call: str) -> None:
        common.check(
            common.run_bash_snippet(
                f"source {shlex.quote(str(run_registry_sh))} && {fn_call}",
                env={"RUN_DIR": run_dir},
            ),
            f"run-registry call failed: {fn_call}",
        )

    def registry_check(fn_call: str) -> bool:
        result = subprocess.run(
            ["bash", "-c", f"source {shlex.quote(str(run_registry_sh))} && {fn_call}"],
            env={**os.environ, "RUN_DIR": run_dir},
        )
        return result.returncode == 0

    proxy_log_fd, proxy_log = tempfile.mkstemp()
    os.close(proxy_log_fd)
    proxy_proc: Optional[subprocess.Popen] = None
    exit_status = 1

    def cat_container_file(container: str, path: str) -> str:
        result = common.run(
            common.sudo_podman("exec", container, "cat", path),
            capture_output=True,
            text=True,
        )
        return result.stdout

    def cleanup() -> None:
        typer.echo("")
        typer.echo("Cleaning up...")
        registry_call("run_cleanup")
        if proxy_proc is not None and proxy_proc.poll() is None:
            proxy_proc.kill()
        Path(proxy_log).unlink(missing_ok=True)
        if exit_status == 0:
            shutil.rmtree(run_dir, ignore_errors=True)
        else:
            typer.echo(f"Run directory kept for inspection: {run_dir}")

    def fail_swap(message: str) -> None:
        typer.echo(f"FAIL: {message}", err=True)
        typer.echo("--- proxy log (host) ---")
        typer.echo(Path(proxy_log).read_text(errors="replace") if Path(proxy_log).exists() else "")
        typer.echo(f"--- {from_container} compositor log ---")
        typer.echo(cat_container_file(from_container, "/tmp/swap-compositor.log"))
        typer.echo(f"--- {to_container} compositor log ---")
        typer.echo(cat_container_file(to_container, "/tmp/swap-compositor.log"))
        typer.echo(f"--- {to_container} client log ---")
        typer.echo(cat_container_file(to_container, "/tmp/swap-client.log"))
        raise _SwapFail(message)

    try:
        typer.echo(f"== Starting {from_wm}'s compositor in {from_container} ==")
        before = common.bash_capture(f"source {shlex.quote(str(socket_wait_sh))} && snapshot_live_sockets")
        launch_snippet = (
            "SCRIPT_DIR=/workspace\n"
            "source /workspace/run-registry.sh\n"
            "source /workspace/compositor-launch.sh\n"
            f"launch_compositor {shlex.quote(from_wm)} /tmp/swap-compositor.log {shlex.quote(f'compositor-{from_wm}')}\n"
        )
        result = common.run_bash_snippet(
            launch_snippet,
            container=from_container,
            env={"XDG_RUNTIME_DIR": common.CONTAINER_RUNTIME_DIR, "RUN_DIR": run_dir},
            detach=True,
        )
        if result.returncode != 0:
            fail_swap(f"couldn't start {from_wm} in {from_container}")

        from_display = common.bash_capture(
            f"source {shlex.quote(str(socket_wait_sh))} && wait_for_new_socket {shlex.quote(before)}",
            env={"RUNTIME_DIR": runtime_dir, "PROXY_DISPLAY": proxy_display},
        ).strip()
        if not from_display:
            fail_swap(f"{from_wm} never created a socket in {from_container}")
        registry_call(f"run_link_socket {shlex.quote(f'compositor-{from_wm}')} {shlex.quote(f'{runtime_dir}/{from_display}')}")
        typer.echo(f"{from_wm} socket: {from_display}")

        typer.echo(f"== Starting proxy on host (-> {from_display}) ==")
        Path(runtime_dir, proxy_display).unlink(missing_ok=True)
        proxy_env = os.environ.copy()
        proxy_env["WAYLAND_DISPLAY"] = from_display
        proxy_log_f = open(proxy_log, "wb")
        proxy_proc = subprocess.Popen([str(proxy_bin)], stdout=proxy_log_f, stderr=subprocess.STDOUT, env=proxy_env)
        registry_call(f"run_track proxy {proxy_proc.pid}")

        proxy_sock_path = Path(runtime_dir, proxy_display)
        for _ in range(20):
            if proxy_sock_path.exists():
                break
            if proxy_proc.poll() is not None:
                fail_swap("proxy exited before creating its socket")
            time.sleep(0.25)
        if not proxy_sock_path.exists():
            fail_swap(f"proxy never created {proxy_display}")
        registry_call(f"run_link_socket proxy {shlex.quote(str(proxy_sock_path))}")
        typer.echo(f"Proxy socket: {proxy_display} (pid {proxy_proc.pid}, on host)")

        typer.echo(f"== Starting client ({client}) in {to_container}, through the proxy ==")
        client_snippet = (
            "source /workspace/run-registry.sh\n"
            f"{shlex.quote(client)} > /tmp/swap-client.log 2>&1 &\n"
            'run_track client "$!"\n'
        )
        result = common.run_bash_snippet(
            client_snippet,
            container=to_container,
            env={
                "XDG_RUNTIME_DIR": common.CONTAINER_RUNTIME_DIR,
                "WAYLAND_DISPLAY": proxy_display,
                "RUN_DIR": run_dir,
            },
            detach=True,
        )
        if result.returncode != 0:
            fail_swap(f"couldn't start {client} in {to_container}")

        time.sleep(2)
        if not registry_check("run_is_alive client"):
            fail_swap(f"{client} exited before the crash even happened")
        typer.echo(f"Client is up in {to_container}. Giving it a moment to settle...")
        time.sleep(1)

        typer.echo(f"== Crashing {from_wm} (in {from_container}) ==")
        pidfile_lines = Path(run_dir, f"compositor-{from_wm}.pid").read_text().splitlines()
        from_compositor_pid = pidfile_lines[1]
        common.run(
            common.sudo_podman("exec", from_container, "kill", "-9", from_compositor_pid),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        time.sleep(2)

        typer.echo(f"== Starting {to_wm}'s compositor in {to_container} (expecting it to reclaim {from_display}) ==")
        before2 = common.bash_capture(f"source {shlex.quote(str(socket_wait_sh))} && snapshot_live_sockets")
        launch_snippet_to = (
            "SCRIPT_DIR=/workspace\n"
            "source /workspace/run-registry.sh\n"
            "source /workspace/compositor-launch.sh\n"
            f"launch_compositor {shlex.quote(to_wm)} /tmp/swap-compositor.log {shlex.quote(f'compositor-{to_wm}')}\n"
        )
        result = common.run_bash_snippet(
            launch_snippet_to,
            container=to_container,
            env={"XDG_RUNTIME_DIR": common.CONTAINER_RUNTIME_DIR, "RUN_DIR": run_dir},
            detach=True,
        )
        if result.returncode != 0:
            fail_swap(f"couldn't start {to_wm} in {to_container}")

        to_display = common.bash_capture(
            f"source {shlex.quote(str(socket_wait_sh))} && wait_for_new_socket {shlex.quote(before2)}",
            env={"RUNTIME_DIR": runtime_dir, "PROXY_DISPLAY": proxy_display},
        ).strip()
        if not to_display:
            fail_swap(f"{to_wm} never created a socket in {to_container}")
        registry_call(f"run_link_socket {shlex.quote(f'compositor-{to_wm}')} {shlex.quote(f'{runtime_dir}/{to_display}')}")
        typer.echo(f"{to_wm} socket: {to_display}")

        if to_display != from_display:
            fail_swap(
                f"{to_wm} landed on {to_display}, not the freed {from_display} -- the proxy is still pointed "
                f"at {from_display} and will never see it. Socket auto-selection didn't reuse the slot this time."
            )

        time.sleep(2)

        typer.echo("")
        if registry_check("run_is_alive client"):
            typer.echo(f"SUCCESS: {client} survived the {from_wm} -> {to_wm} swap.")
            exit_status = 0
        else:
            typer.echo(f"FAIL: {client} did not survive the {from_wm} -> {to_wm} swap.", err=True)
            typer.echo("--- proxy log (host) ---")
            typer.echo(Path(proxy_log).read_text(errors="replace"))
            typer.echo(f"--- {to_container} client log ---")
            typer.echo(cat_container_file(to_container, "/tmp/swap-client.log"))
            exit_status = 1
    except _SwapFail:
        exit_status = 1
    finally:
        cleanup()

    raise typer.Exit(exit_status)


@app.command("smoke")
def smoke(
    wm: str = common.wm_option("Which compositor to run the smoke test against."),
) -> None:
    """setup -> crash --verify-recovery -> diagnose --errors-only -> teardown,
    tearing down whether it passed or failed."""
    exit_status = 1
    try:
        typer.echo(f"== [{wm}] Building and starting container ==")
        rc = common.invoke(env.setup, wm=wm, proxy_deb=None)
        if rc != 0:
            typer.echo(f"FAIL [{wm}]: env setup", err=True)
            raise typer.Exit(1)

        typer.echo(f"== [{wm}] Running test crash --verify-recovery ==")
        rc = common.invoke(crash, wm=wm, client=common.DEFAULT_CLIENT, verify_recovery=True)
        if rc != 0:
            typer.echo(f"FAIL [{wm}]: test crash --verify-recovery", err=True)
            raise typer.Exit(1)

        typer.echo(f"== [{wm}] Running diagnose --errors-only ==")
        rc = common.invoke(
            diagnose_cmd.diagnose, verbose=False, errors_only=True, host_only=False, wm=wm, container=None
        )
        if rc != 0:
            typer.echo(f"FAIL [{wm}]: diagnose --errors-only", err=True)
            raise typer.Exit(1)

        typer.echo(f"SUCCESS [{wm}]: setup + verify-recovery crash/reconnect + diagnose all clean.")
        exit_status = 0
    finally:
        typer.echo(f"== [{wm}] Tearing down ==")
        common.invoke(env.teardown, wm=wm, image=False)

    raise typer.Exit(exit_status)


@app.command("matrix")
def matrix(
    wm: List[str] = typer.Option(
        [], "--wm", help="Compositor(s) to test (repeatable). Defaults to all four if none given."
    ),
) -> None:
    """Runs `test smoke` over a set of compositors, writing a pass/fail
    results.md table."""
    wms = list(wm) if wm else list(common.WM_CHOICES)

    results_dir = common.PROJECT_ROOT if common.PROJECT_ROOT else Path.cwd()
    results_file = results_dir / "results.md"
    log_dir = Path(tempfile.mkdtemp())

    lines = [
        "# Phase 9/10 test matrix results",
        "",
        f"Run: {datetime.now().astimezone().isoformat()}",
        "",
        "| WM | Result | Log |",
        "|---|---|---|",
    ]

    overall = 0
    for w in wms:
        typer.echo("")
        typer.echo("############################################")
        typer.echo(f"# {w}")
        typer.echo("############################################")
        log_path = log_dir / f"{w}.log"
        with common.tee_to_file(log_path):
            rc = common.invoke(smoke, wm=w)
        if rc == 0:
            lines.append(f"| {w} | PASS | `{log_path}` |")
        else:
            lines.append(f"| {w} | **FAIL** | `{log_path}` |")
            overall = 1

    lines.append("")
    if overall == 0:
        lines.append("All WMs passed.")
    else:
        lines.append(
            "One or more WMs failed -- see their log for the FAIL line and full output "
            "(env setup, test crash --verify-recovery, or diagnose)."
        )

    results_file.write_text("\n".join(lines) + "\n")

    typer.echo("")
    typer.echo("== Results ==")
    typer.echo(results_file.read_text())
    typer.echo(f"Full logs kept at: {log_dir}")
    typer.echo(f"Results table written to: {results_file}")

    raise typer.Exit(overall)
