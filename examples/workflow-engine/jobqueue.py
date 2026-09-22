"""The workflow engine's half: a job queue, with no idea Zygo exists.

Windmill has this in Postgres (`v2_job_queue`, `SELECT … FOR UPDATE SKIP
LOCKED`), n8n has it in Redis through BullMQ, Temporal has it in its own
service. The shape is the same everywhere and it is the shape this file
implements in ninety lines of SQLite, so the example runs with nothing
installed:

* a job names a **script** — a language, a path, and a hash of its contents —
  and carries the **arguments** for one run;
* a worker claims a job, runs it, and writes back a result;
* the same script appears in thousands of jobs, which is the fact the whole
  integration turns on.

Nothing here is Zygo-specific on purpose. `worker.py` is the part that is.
"""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import time
from dataclasses import dataclass
from typing import Any, Optional

SCHEMA = """
CREATE TABLE IF NOT EXISTS jobs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    script      TEXT NOT NULL,   -- the script's name, stable across versions
    path        TEXT NOT NULL,   -- where its source is, on this host
    digest      TEXT NOT NULL,   -- of the source; a new one is a new version
    args        TEXT NOT NULL,   -- JSON, the arguments for this run
    state       TEXT NOT NULL DEFAULT 'queued',
    result      TEXT,
    error       TEXT,
    stdout      TEXT,
    stderr      TEXT,
    wall_ms     REAL,
    claimed_at  REAL
);
"""


@dataclass
class Job:
    """One run of one script."""

    id: int
    script: str
    path: str
    digest: str
    args: Any


class Queue:
    """A job queue. At-least-once, like every real one."""

    def __init__(self, path: str) -> None:
        self.db = sqlite3.connect(path, isolation_level=None)
        self.db.row_factory = sqlite3.Row
        self.db.executescript(SCHEMA)

    def close(self) -> None:
        self.db.close()

    def submit(self, script: str, path: str, args: Any) -> int:
        """Enqueue one run. The digest is taken here, as the engine would."""
        with open(path, "rb") as f:
            digest = hashlib.sha256(f.read()).hexdigest()[:16]
        cursor = self.db.execute(
            "INSERT INTO jobs (script, path, digest, args) VALUES (?, ?, ?, ?)",
            (script, os.path.abspath(path), digest, json.dumps(args)),
        )
        return int(cursor.lastrowid)

    def claim(self) -> Optional[Job]:
        """Take the oldest queued job, or `None` when there is nothing to do."""
        self.db.execute("BEGIN IMMEDIATE")
        try:
            row = self.db.execute(
                "SELECT * FROM jobs WHERE state = 'queued' ORDER BY id LIMIT 1"
            ).fetchone()
            if row is None:
                self.db.execute("COMMIT")
                return None
            self.db.execute(
                "UPDATE jobs SET state = 'running', claimed_at = ? WHERE id = ?",
                (time.time(), row["id"]),
            )
            self.db.execute("COMMIT")
        except BaseException:
            self.db.execute("ROLLBACK")
            raise
        return Job(
            id=row["id"],
            script=row["script"],
            path=row["path"],
            digest=row["digest"],
            args=json.loads(row["args"]),
        )

    def release(self, job: Job) -> None:
        """Put a claimed job back. A worker that could not run it says so."""
        self.db.execute(
            "UPDATE jobs SET state = 'queued', claimed_at = NULL WHERE id = ?", (job.id,)
        )

    def complete(
        self,
        job: Job,
        *,
        result: Any = None,
        error: Optional[str] = None,
        stdout: str = "",
        stderr: str = "",
        wall_ms: float = 0.0,
    ) -> None:
        self.db.execute(
            "UPDATE jobs SET state = ?, result = ?, error = ?, stdout = ?, stderr = ?, "
            "wall_ms = ? WHERE id = ?",
            (
                "failed" if error else "completed",
                json.dumps(result),
                error,
                stdout,
                stderr,
                wall_ms,
                job.id,
            ),
        )

    def summary(self) -> list[sqlite3.Row]:
        return list(
            self.db.execute(
                "SELECT script, state, count(*) AS n, round(avg(wall_ms), 2) AS avg_ms "
                "FROM jobs GROUP BY script, state ORDER BY script, state"
            )
        )
