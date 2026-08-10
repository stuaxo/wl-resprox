"""Shared helpers: path resolution, WM validation, podman/subprocess
wrappers. Kept here rather than duplicated per-command, the thing the
old Bash scripts' repeated ``for arg in "$@"`` loops never managed.
"""

from __future__ import annotations

import contextlib
import os
import subprocess
import sys
from pathlib import Path
from typing import Optional

import typer

WM_CHOICES = ["labwc", "sway", "kwin", "mutter"]
DEFAULT_WM = "labwc"
# gtk4-demo produced a false failure signal here 2026-08-10: it fails
# crash-recovery against kwin/sway with a compositor-side dmabuf-import
# rejection that scripts/gtk/dmabuf_gl.py -- this project's own real
# GL-rendered, dmabuf-backed test client -- does NOT hit, cleanly
# passing against all four compositors (3/3 on kwin, clean on sway,
# alongside its pre-existing mutter/labwc coverage). gtk4-demo's own
# rendering-path behavior is opaque (it silently fell back to X11 once
# before, see scripts/gtk/common.py's own doc comment) -- exactly the
# class of surprise these self-logging clients exist to avoid. The
# wrapper (not a bare `python3 ... .py`) is required: this module's own
# `--client` handling shlex.quotes the whole string as one token before
# handing it to the container's shell. "/workspace" literal, not
# HARNESS_CONTAINER_ROOT, since that's defined below this point in the
# file (see its own comment for why it isn't sourced from one place).
DEFAULT_CLIENT = "/workspace/gtk/run-dmabuf-gl.sh"

# Mirrors scripts/harness-paths.sh's HARNESS_CONTAINER_ROOT -- kept as a
# literal here rather than sourced from that file, since that file must
# stay Bash (diagnose.sh, unmodified, still sources it directly) and the
# value is static. If it ever changes, update both places.
HARNESS_CONTAINER_ROOT = "/workspace"

# Every container's `dev` user is created at build time with the host's
# *actual* uid (see env.py's setup()), but several call sites across the
# original Bash scripts hardcode this exact path for the container-side
# XDG_RUNTIME_DIR regardless -- a pre-existing assumption (uid 1000
# inside the container), preserved as-is here for behavioral parity, not
# a new one introduced by this port.
CONTAINER_RUNTIME_DIR = "/run/user/1000"

# --- path resolution: dev checkout vs. installed package -------------

PKG_DIR = Path(__file__).resolve().parent
LIB_DIR = PKG_DIR.parent
_repo_root_candidate = LIB_DIR.parent
if (_repo_root_candidate / "scripts" / "harness-paths.sh").is_file():
    # Dev checkout: harness/ sits beside scripts/ and Cargo.toml at the
    # repo root -- same "probe for a known sibling" style setup-env.sh's
    # own PROJECT_ROOT/Cargo.toml check already used.
    BASH_SCRIPT_DIR = _repo_root_candidate / "scripts"
    PROJECT_ROOT: Optional[Path] = _repo_root_candidate
else:
    # Installed package: build-harness-deb.sh stages the surviving
    # container-side .sh files into the same directory as this package.
    BASH_SCRIPT_DIR = LIB_DIR
    PROJECT_ROOT = None


def container_name(wm: str) -> str:
    return f"wayland-proxy-dev-{wm}"


def image_tag(wm: str) -> str:
    return f"wayland-proxy-dev-{wm}:latest"


# The shared base image every per-WM Containerfile builds FROM -- see
# scripts/containers/base/Containerfile's own comment for what it
# factors out and why group/user creation itself stays per-WM.
BASE_IMAGE_TAG = "wayland-proxy-dev-base:latest"


def wm_dir(wm: str) -> Path:
    return BASH_SCRIPT_DIR / "containers" / wm


def validate_wm(wm: str) -> None:
    if not (wm_dir(wm) / "Containerfile").is_file():
        typer.echo(f"ERROR: no {wm_dir(wm)}/Containerfile -- unknown --wm value '{wm}'?", err=True)
        raise typer.Exit(1)


def wm_option(help_text: str = "Which compositor's container to target.") -> str:
    """A single-value --wm option: default 'labwc', also settable via
    WAYLAND_HARNESS_WM. Deliberately not used by `test matrix`, whose
    repeatable --wm list always defaults to all four regardless."""
    return typer.Option(DEFAULT_WM, "--wm", envvar="WAYLAND_HARNESS_WM", help=help_text)


# --- subprocess wrappers ----------------------------------------------


def sudo_podman(*args: str) -> list[str]:
    return ["sudo", "podman", *args]


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    """Thin wrapper, no output capture by default -- streams live,
    matching the original scripts' visible build/run/exec progress."""
    return subprocess.run(cmd, **kwargs)


def check(result: subprocess.CompletedProcess, message: str) -> None:
    """Every subprocess call's exit status must be checked explicitly --
    these scripts use `-uo pipefail` with per-step checks deliberately,
    not a blanket `-e`, and subprocess.run() doesn't raise on its own."""
    if result.returncode != 0:
        typer.echo(f"ERROR: {message}", err=True)
        raise typer.Exit(1)


def container_exists(name: str) -> bool:
    result = run(sudo_podman("container", "exists", name), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return result.returncode == 0


def image_exists(tag: str) -> bool:
    result = run(sudo_podman("image", "exists", tag), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return result.returncode == 0


def container_running(name: str) -> bool:
    result = run(
        sudo_podman("inspect", "-f", "{{.State.Running}}", name),
        capture_output=True,
        text=True,
    )
    return result.returncode == 0 and result.stdout.strip() == "true"


def run_bash_snippet(
    snippet: str,
    *,
    container: Optional[str] = None,
    env: Optional[dict[str, str]] = None,
    detach: bool = False,
    user: str = "dev",
) -> subprocess.CompletedProcess:
    """Runs a literal Bash snippet either inside a container (via podman
    exec) or locally on the host, depending on whether `container` is
    given. Used for the pieces that must stay Bash because they source
    run-registry.sh/compositor-launch.sh (staying unmodified, per plan)
    -- no reason to hand-port that logic into Python on either side of
    the container boundary."""
    if container is not None:
        cmd = sudo_podman("exec", *(["-d"] if detach else []), "--user", user)
        for key, value in (env or {}).items():
            cmd += ["-e", f"{key}={value}"]
        cmd += [container, "bash", "-c", snippet]
    else:
        full_env = os.environ.copy()
        full_env.update(env or {})
        cmd = ["bash", "-c", snippet]
        return run(cmd, env=full_env)
    return run(cmd)


@contextlib.contextmanager
def tee_to_file(path):
    """OS-fd-level tee of this process's stdout+stderr to `path`, still
    streaming live -- the Python equivalent of `cmd 2>&1 | tee "$log"`.
    Needed because a pure Python-level redirect (contextlib.redirect_stdout)
    wouldn't capture the subprocess output most of these commands
    actually produce -- subprocess inherits the real OS file descriptor,
    not Python's sys.stdout object. Reuses the actual `tee` binary rather
    than reimplementing it -- already an implicit dependency of this
    project's own prior test-matrix.sh."""
    sys.stdout.flush()
    sys.stderr.flush()
    tee_proc = subprocess.Popen(["tee", str(path)], stdin=subprocess.PIPE)
    saved_stdout_fd = os.dup(1)
    saved_stderr_fd = os.dup(2)
    os.dup2(tee_proc.stdin.fileno(), 1)
    os.dup2(tee_proc.stdin.fileno(), 2)
    try:
        yield
    finally:
        sys.stdout.flush()
        sys.stderr.flush()
        os.dup2(saved_stdout_fd, 1)
        os.dup2(saved_stderr_fd, 2)
        os.close(saved_stdout_fd)
        os.close(saved_stderr_fd)
        tee_proc.stdin.close()
        tee_proc.wait()


def bash_capture(snippet: str, env: Optional[dict[str, str]] = None) -> str:
    """Runs a Bash snippet locally on the HOST, returning its stdout.
    For the socket-wait.sh/run-registry.sh calls in `test swap` that need
    a value back, not just an exit code (see run_bash_snippet for the
    fire-and-forget case)."""
    full_env = os.environ.copy()
    full_env.update(env or {})
    result = subprocess.run(["bash", "-c", snippet], capture_output=True, text=True, env=full_env)
    check(result, f"bash snippet failed (stderr: {result.stderr.strip()})")
    return result.stdout


def invoke(fn, **kwargs) -> int:
    """Calls a Typer-command function directly, in-process (not through
    Click), returning its exit code. Typer's Option()/Argument() default
    values are only resolved by Click's own invocation machinery, so
    every keyword argument the target function declares must be passed
    explicitly here -- never rely on one of its defaults when calling
    this way, it'll be the raw OptionInfo object, not the resolved value.
    """
    try:
        fn(**kwargs)
    except typer.Exit as e:
        return e.exit_code
    return 0


def fail(message: str) -> None:
    typer.echo(f"FAIL: {message}", err=True)
    raise typer.Exit(1)


def eprint(message: str) -> None:
    print(message, file=sys.stderr)
