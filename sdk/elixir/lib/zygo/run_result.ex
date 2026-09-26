# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.RunResult do
  @moduledoc """
  What a one-shot sandbox said.

  A non-zero `exit_code` is not an error: the sandbox ran, and this is what it
  reported. Only Zygo failing to run it at all is `{:error, _}`.

  `timed_out` and `oom_killed` are why it ended, which the exit code cannot
  carry: a deadline kill and an out-of-memory kill are both `SIGKILL`, so
  both are 137.

  `started` is whether the program ran at all. `false` means Zygo could not
  build the sandbox, and `phase` says how far it got (`"plan"`, `"start"`) —
  the host's problem, not the program's. An API one release behind does not
  send it; `started` then defaults to `true`, which is what those releases
  meant.

  `ok` is all of it at once: started, exit 0, not timed out, not killed.
  """
  import Zygo.Parse

  @known ~w(exit_code stdout stderr timed_out oom_killed peak_rss_kb wall_ms started phase)

  defstruct exit_code: -1,
            stdout: "",
            stderr: "",
            timed_out: false,
            oom_killed: false,
            peak_rss_kb: 0,
            wall_ms: 0.0,
            started: true,
            phase: "run",
            ok: false,
            extra: %{}

  @type t :: %__MODULE__{
          exit_code: integer(),
          stdout: String.t(),
          stderr: String.t(),
          timed_out: boolean(),
          oom_killed: boolean(),
          peak_rss_kb: non_neg_integer(),
          wall_ms: float(),
          started: boolean(),
          phase: String.t(),
          ok: boolean(),
          extra: map()
        }

  @doc false
  def parse(raw) when is_map(raw) do
    run = %__MODULE__{
      exit_code: int(raw, "exit_code", -1),
      stdout: str(raw, "stdout"),
      stderr: str(raw, "stderr"),
      timed_out: bool(raw, "timed_out"),
      oom_killed: bool(raw, "oom_killed"),
      peak_rss_kb: int(raw, "peak_rss_kb"),
      wall_ms: float(raw, "wall_ms"),
      started: bool(raw, "started", true),
      phase: str(raw, "phase", "run"),
      extra: extra(raw, @known)
    }

    %{run | ok: run.started and run.exit_code == 0 and not run.timed_out and not run.oom_killed}
  end
end
