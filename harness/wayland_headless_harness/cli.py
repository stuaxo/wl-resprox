"""Top-level Typer app for wayland-headless-harness."""

from __future__ import annotations

import typer

from .commands import diagnose, env, session, testing

app = typer.Typer(
    no_args_is_help=True,
    add_completion=False,
    help="Headless multi-compositor Wayland test harness.",
)

app.add_typer(env.app, name="env")
app.add_typer(session.app, name="session")
app.add_typer(testing.app, name="test")
app.command("diagnose")(diagnose.diagnose)


if __name__ == "__main__":
    app()
