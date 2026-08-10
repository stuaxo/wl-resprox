# Known-bug reproduction scripts

One script per entry in `docs/KNOWN_BUGS.md` that has a scripted
reproduction. Not every entry has one yet -- see that file for which.

None of these self-verify: they automate getting into the state where
the bug is observable, not the observation itself (window grouping,
cursor visibility) since that needs eyes on a real screen. Each
script's own header says what to check afterward and how.

Destructive ones (anything that ends a login session) refuse to run
without an explicit `--yes` -- read the header before passing it.
