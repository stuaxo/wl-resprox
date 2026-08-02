"""`session start-host` / `session start-guest` -- replaces
start-host.sh/start-guest.sh."""

from __future__ import annotations

import os
import subprocess
import time
from pathlib import Path

import typer

from .. import common

app = typer.Typer(no_args_is_help=True, help="Start a host or guest Wayland session.")


@app.command("start-host")
def start_host(
    port: int = typer.Option(5900, "--port", help="VNC port to serve the host compositor on."),
) -> None:
    """Starts a headless labwc compositor on the HOST and serves it over VNC."""
    typer.echo("Starting headless host compositor...")
    env = os.environ.copy()
    env["WLR_BACKENDS"] = "headless"
    env["WLR_LIBINPUT_NO_DEVICES"] = "1"
    # Backgrounded and deliberately never reaped/waited on here, matching
    # today's un-trapped `&` -- don't add cleanup that wasn't there before.
    subprocess.Popen(["labwc"], env=env)

    runtime_dir = os.environ["XDG_RUNTIME_DIR"]
    sock = None
    for _ in range(10):
        matches = sorted(Path(runtime_dir).glob("wayland-*[0-9]"))
        if matches:
            sock = matches[0]
            break
        time.sleep(0.5)

    if sock is None:
        typer.echo("ERROR: labwc never created a Wayland socket -- check the errors above.", err=True)
        raise typer.Exit(1)

    display_name = sock.name
    typer.echo(f"Host compositor socket: {display_name}")
    typer.echo("Pass this to `wayland-headless-harness session start-guest`, e.g.:")
    typer.echo(f"  wayland-headless-harness session start-guest --host-socket={display_name}")
    typer.echo("")
    typer.echo(
        f"Starting wayvnc on 127.0.0.1:{port} "
        f"(tunnel with: ssh -L {port}:localhost:{port} <host>)"
    )
    vnc_env = os.environ.copy()
    vnc_env["WAYLAND_DISPLAY"] = display_name
    result = subprocess.run(["wayvnc", "127.0.0.1", str(port)], env=vnc_env)
    raise typer.Exit(result.returncode)


@app.command("start-guest")
def start_guest(
    wm: str = common.wm_option("Which compositor's container to enter."),
    host_socket: str = typer.Option(
        "wayland-0", "--host-socket", help="Host Wayland socket to hand the nested compositor -- match whatever `session start-host` printed."
    ),
) -> None:
    """Starts the nested compositor inside a WM container, then drops
    into an interactive shell for testing (gtk4-demo, wayland-info, ...)."""
    container = common.container_name(wm)

    typer.echo(f"Starting nested compositor in '{container}' (host display: {host_socket})...")
    common.check(
        common.run(
            common.sudo_podman(
                "exec",
                "--user",
                "dev:render",
                "-e",
                f"WAYLAND_DISPLAY={host_socket}",
                "-e",
                f"XDG_RUNTIME_DIR={common.CONTAINER_RUNTIME_DIR}",
                container,
                "bash",
                f"{common.HARNESS_CONTAINER_ROOT}/entrypoint.sh",
            )
        ),
        f"couldn't start the nested compositor in {container}",
    )

    typer.echo("")
    typer.echo(f"Entering '{container}' interactively for testing...")
    result = common.run(
        common.sudo_podman(
            "exec",
            "-it",
            "--user",
            "dev:render",
            "-e",
            f"WAYLAND_DISPLAY={host_socket}",
            "-e",
            f"XDG_RUNTIME_DIR={common.CONTAINER_RUNTIME_DIR}",
            "-w",
            common.HARNESS_CONTAINER_ROOT,
            container,
            "bash",
        )
    )
    raise typer.Exit(result.returncode)
