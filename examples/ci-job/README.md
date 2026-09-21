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

Note that `--requirements` does not need the network *after the first job*:
the venv is keyed on the image's digest and the file's bytes, so a second job
with the same two reuses it and can run with `--net none`. The warm path
(`zygo serve`) shares that cache, so a project that does both builds it once.

The exit code is the program's. A test run that overruns `--timeout` exits
137 — the same code an OOM kill produces, because both are a `SIGKILL` and the
wait status cannot say more. When the difference matters, `--outcome` writes
it to a file:

```bash
zygo run --outcome /tmp/why.json --mem 512M --timeout 10m \
    python:3.12-slim python3 -m unittest discover -s /src
cat /tmp/why.json
# {"exit_code":137,"timed_out":true,"oom_killed":false,"peak_rss_kb":91204,"wall_ms":600031.2}
```

Standard output belongs to the program, which is why this goes to a file
rather than into the stream a test wrote to. The same three fields come back
from `POST /run` on the HTTP API.

[`run.sh`](run.sh) is the whole job, for copying into a pipeline.
