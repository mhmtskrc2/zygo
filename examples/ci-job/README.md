# A CI job

`zygo run` is a one-shot sandbox with Docker's shape and none of its
orchestration: no daemon, no root, a cold start measured at 18 ms with the
image cached. That makes it a fit for the thing CI does a thousand times a
day — run a repository's tests somewhere they cannot hurt anything.

```bash
# The repository is mounted read-only; the tests get a writable /tmp and
# nothing else. No network at all, so a test that phones home fails here
# rather than in production.
zygo run --mount ./repo:/src:ro --mem 512M --timeout 10m \
    python:3.12-slim python3 -m unittest discover -s /src -v
```

Tests that need packages get a venv built inside the image, once, and shared
by every job that lists the same requirements against the same image:

```bash
zygo run --mount ./repo:/src:ro --requirements ./repo/requirements.txt \
    --net egress --allow pypi.org:443 --allow files.pythonhosted.org:443 \
    python:3.12-slim python3 -m pytest /src
```

The exit code is the program's. A test run that overruns `--timeout` exits
137 — the same code an OOM kill produces, deliberately: both mean "the sandbox
ended this, not the program".

[`run.sh`](run.sh) is the whole job, for copying into a pipeline.
