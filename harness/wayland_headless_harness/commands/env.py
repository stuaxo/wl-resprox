"""`env setup` / `env teardown` -- replaces setup-env.sh/teardown-env.sh."""

from __future__ import annotations

import glob
import grp
import os
from pathlib import Path
from typing import Optional

import typer

from .. import common

app = typer.Typer(no_args_is_help=True, help="Build/start or stop/remove a WM container.")


@app.command("setup")
def setup(
    wm: str = common.wm_option("Which compositor's container to build."),
    proxy_deb: Optional[str] = typer.Option(
        None,
        "--proxy-deb",
        help="A .deb to install into the container -- doesn't have to be "
        "wayland-proxy, or built from this repo. Without it: auto-detects a "
        "wayland-proxy checkout one directory up and builds+installs that; "
        "if there isn't one, the container is provisioned without a proxy.",
    ),
) -> None:
    common.validate_wm(wm)
    container = common.container_name(wm)
    image = common.image_tag(wm)
    wm_dir = common.wm_dir(wm)

    host_uid = os.getuid()
    host_gid = os.getgid()
    try:
        host_video_gid = grp.getgrnam("video").gr_gid
        host_render_gid = grp.getgrnam("render").gr_gid
    except KeyError:
        typer.echo(
            "ERROR: couldn't resolve the host's video/render group GIDs "
            "(getent group video render).",
            err=True,
        )
        raise typer.Exit(1)

    host_runtime_dir = f"/run/user/{host_uid}"
    if not Path(host_runtime_dir).is_dir():
        typer.echo(
            f"ERROR: {host_runtime_dir} doesn't exist. Needs a real login "
            "session (systemd-logind/PAM) on this host.",
            err=True,
        )
        raise typer.Exit(1)

    typer.echo(
        f"Building {image} (uid={host_uid} gid={host_gid} "
        f"video={host_video_gid} render={host_render_gid})..."
    )
    common.check(
        common.run(
            common.sudo_podman(
                "build",
                "--network",
                "host",
                "-t",
                image,
                "--build-arg",
                f"USER_UID={host_uid}",
                "--build-arg",
                f"USER_GID={host_gid}",
                "--build-arg",
                f"VIDEO_GID={host_video_gid}",
                "--build-arg",
                f"RENDER_GID={host_render_gid}",
                "-f",
                str(wm_dir / "Containerfile"),
                str(wm_dir),
            )
        ),
        f"podman build failed for {image}",
    )

    if common.container_exists(container):
        typer.echo(f"Removing existing {container} container...")
        common.check(common.run(common.sudo_podman("rm", "-f", container)), f"couldn't remove {container}")

    typer.echo(f"Starting {container}...")
    common.check(
        common.run(
            common.sudo_podman(
                "run",
                "-d",
                "--name",
                container,
                "--init",
                "--network",
                "host",
                "--group-add",
                str(host_video_gid),
                "--group-add",
                str(host_render_gid),
                "--device",
                "/dev/dri",
                "-v",
                f"{common.BASH_SCRIPT_DIR}:{common.HARNESS_CONTAINER_ROOT}",
                "-v",
                "/dev:/dev",
                "-v",
                f"{host_runtime_dir}:{host_runtime_dir}",
                image,
                "sleep",
                "infinity",
            )
        ),
        f"podman run failed for {container}",
    )

    typer.echo("Verifying passwordless sudo and render group membership...")
    verify_snippet = """
set -e
sudo -n true
echo "sudo: passwordless OK"
groups | grep -qw render && groups | grep -qw video
echo "groups: $(groups)"
"""
    common.check(
        common.run_bash_snippet(verify_snippet, container=container, user="dev"),
        f"passwordless-sudo/render-group verification failed in {container}",
    )

    # Which proxy gets installed is an explicit input, not an assumed
    # sibling (see --proxy-deb's help above). ADR-0003: the harness never
    # invokes `cargo build`/`cargo deb` against the proxy's own source
    # from inside a container -- only ever here, once, on the host.
    deb_path: Optional[Path] = None
    if proxy_deb:
        candidate = Path(proxy_deb)
        if not candidate.is_file():
            typer.echo(f"ERROR: --proxy-deb='{proxy_deb}' not found.", err=True)
            raise typer.Exit(1)
        deb_path = candidate.resolve()
    elif common.PROJECT_ROOT and (common.PROJECT_ROOT / "Cargo.toml").is_file():
        typer.echo("Building wayland-proxy .deb (cargo deb)...")
        common.check(
            common.run(["cargo", "deb", "--quiet"], cwd=common.PROJECT_ROOT),
            "cargo deb failed",
        )
        candidates = glob.glob(str(common.PROJECT_ROOT / "target" / "debian" / "wayland-proxy_*.deb"))
        if not candidates:
            typer.echo("ERROR: cargo deb succeeded but produced no .deb?", err=True)
            raise typer.Exit(1)
        deb_path = Path(max(candidates, key=lambda p: Path(p).stat().st_mtime))
    else:
        typer.echo(
            f"No --proxy-deb given and no wayland-proxy checkout found at "
            f"{common.PROJECT_ROOT} -- provisioning {container} without a proxy installed."
        )

    if deb_path is not None:
        remote_path = f"/tmp/{deb_path.name}"
        typer.echo(f"Installing {deb_path.name} into {container}...")
        common.check(
            common.run(common.sudo_podman("cp", str(deb_path), f"{container}:{remote_path}")),
            f"podman cp of {deb_path.name} failed",
        )
        common.check(
            common.run(common.sudo_podman("exec", "--user", "dev", container, "sudo", "dpkg", "-i", remote_path)),
            f"dpkg -i {deb_path.name} failed in {container}",
        )

    typer.echo("")
    typer.echo("======================================")
    typer.echo("Environment successfully provisioned!")
    typer.echo("To start testing, run:")
    typer.echo(f"  wayland-headless-harness session start-guest --wm={wm}")
    typer.echo("(needs a host Wayland session first -- see: wayland-headless-harness session start-host)")
    typer.echo("======================================")


@app.command("teardown")
def teardown(
    wm: str = common.wm_option("Which compositor's container to remove."),
    image: bool = typer.Option(False, "--image", help="Also remove the built image."),
) -> None:
    container = common.container_name(wm)
    image_ref = common.image_tag(wm)

    if common.container_exists(container):
        typer.echo(f"Stopping and removing {container}...")
        common.check(common.run(common.sudo_podman("rm", "-f", container)), f"couldn't remove {container}")
    else:
        typer.echo(f"{container}: no container to remove.")

    if image:
        if common.image_exists(image_ref):
            typer.echo(f"Removing {image_ref}...")
            common.check(common.run(common.sudo_podman("rmi", image_ref)), f"couldn't remove {image_ref}")
        else:
            typer.echo(f"{image_ref}: no image to remove.")

    typer.echo("Done.")
