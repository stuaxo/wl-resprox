#!/bin/sh
# harness/'s `test crash --client=...` shlex.quote()s the whole client
# string as one token before handing it to the shell -- fine for a bare
# command like `gtk4-demo`, but means it can't run `python3
# dmabuf_gl.py` directly (that becomes one literal, space-containing
# "command name" bash can't resolve, "No such file or directory").
# This wrapper is itself the single token instead.
exec python3 "$(dirname "$0")/dmabuf_gl.py"
