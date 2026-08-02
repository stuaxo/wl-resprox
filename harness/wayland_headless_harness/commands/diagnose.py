"""`diagnose` -- thin wrapper around the untouched scripts/diagnose.sh.

diagnose.sh itself gets zero changes: it already does its own host/guest
branching (re-exec'ing itself inside the container via podman exec) and
its own flag parsing. This command only translates --wm=<name> into the
container name it already expects as a positional argument, and
forwards everything else straight through.
"""

from __future__ import annotations

from typing import Optional

import typer

from .. import common


def diagnose(
    verbose: bool = typer.Option(False, "--verbose", help="Also dump full file-descriptor listings per process."),
    errors_only: bool = typer.Option(
        False, "--errors-only", help="Print nothing on a clean run; only environment problems. For scripting."
    ),
    host_only: bool = typer.Option(False, "--host-only", help="Don't exec into the container at all."),
    wm: str = common.wm_option("Which compositor's container to inspect."),
    container: Optional[str] = typer.Option(
        None, "--container", help="Inspect this exact container name instead of deriving it from --wm."
    ),
) -> None:
    target = container or common.container_name(wm)
    flags = []
    if verbose:
        flags.append("--verbose")
    if errors_only:
        flags.append("--errors-only")
    if host_only:
        flags.append("--host-only")

    result = common.run(["bash", str(common.BASH_SCRIPT_DIR / "diagnose.sh"), *flags, target])
    raise typer.Exit(result.returncode)
