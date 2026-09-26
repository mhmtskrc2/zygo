// SPDX-License-Identifier: Apache-2.0
//! One warm function, whichever way it is warm.
//!
//! The supervisor, the CLI and the benchmarks see [`Function`] and nothing
//! below it: a function is called, paused, woken and stopped the same way
//! whether an agent forks for it or a fresh process is entered into its
//! sandbox. Every method here forwards to one of the two shapes.

use std::collections::BTreeMap;

use super::request::{Call, ChunkSink};
use super::timing::{CallTiming, CpuAccounting};
#[cfg(target_os = "linux")]
use super::warm_exec::WarmExec;
use super::{Outcome, Status, WarmFn};
use crate::error::Result;
use crate::sandbox::SandboxState;

/// One warm function, whichever way it is warm.
///
/// The supervisor, the CLI and the benchmarks see this and nothing below it:
/// a function is called, paused, woken and stopped the same way whether an
/// agent forks for it or a fresh process is entered into its sandbox.
pub enum Function {
    /// An agent in the box, forking per request.
    Agent(Box<WarmFn>),
    /// A held sandbox, entered per request.
    #[cfg(target_os = "linux")]
    Exec(Box<WarmExec>),
}

// Both arms are boxed. An enum is as large as its largest arm, and these two
// differ by a factor of two, so unboxed every agent would carry a held
// sandbox's worth of padding. The cost is one allocation per *served
// function* — not per request, and nothing on the request path follows the
// pointer more than once.

macro_rules! each {
    ($self:expr, $f:ident => $body:expr) => {
        match $self {
            Function::Agent($f) => $body,
            #[cfg(target_os = "linux")]
            Function::Exec($f) => $body,
        }
    };
}

impl Function {
    pub fn name(&self) -> &str {
        each!(self, f => f.name())
    }

    /// Host pid of the sandbox's first process, whichever kind it is.
    pub fn init_pid(&self) -> u32 {
        each!(self, f => f.init_pid())
    }

    pub fn status(&self) -> Status {
        each!(self, f => f.status())
    }

    pub fn state(&self) -> SandboxState {
        each!(self, f => f.state())
    }

    pub fn is_healthy(&self) -> bool {
        each!(self, f => f.is_healthy())
    }

    pub fn timeout(&self) -> std::time::Duration {
        each!(self, f => f.timeout())
    }

    pub fn set_secrets(&self, values: BTreeMap<String, String>) {
        each!(self, f => f.set_secrets(values))
    }

    pub fn pause(&self) -> Result<()> {
        each!(self, f => f.pause())
    }

    pub fn resume(&self) -> Result<()> {
        each!(self, f => f.resume())
    }

    pub fn cpu_accounting(&self) -> Option<CpuAccounting> {
        each!(self, f => f.cpu_accounting())
    }

    /// Stop a request this function is running. `None` if it is not here.
    ///
    /// The supervisor asks every function and pool in turn, because request
    /// ids are unique across the host and a second index of "which function
    /// holds id X" would be a thing that could disagree with this one.
    pub fn cancel(&self, id: &str, caller: Option<&str>) -> Option<bool> {
        each!(self, f => f.cancel(id, caller))
    }

    /// Ids this function is running.
    pub fn in_flight_ids(&self) -> Vec<String> {
        each!(self, f => f.in_flight_ids())
    }

    pub fn call(&self, event: serde_json::Value) -> Result<Outcome> {
        self.call_with_timeout(event, self.timeout())
    }

    pub fn call_with_timeout(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<Outcome> {
        self.call_timed(event, timeout).map(|(o, _)| o)
    }

    /// The same, under a name the caller chose, streaming if asked.
    ///
    /// See `InFlight::key` and [`WarmFn::call_streaming`]. Only the agent
    /// shape carries either: a warm-exec function has no agent to tell, and
    /// its output is collected from pipes at the end. That is not a limitation
    /// worth closing until something asks — the pool shape is what an
    /// embedder's long requests run on, and it is the one with an agent.
    pub fn call_keyed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
        key: Option<&str>,
        sink: Option<ChunkSink<'_>>,
    ) -> Result<Outcome> {
        self.call_full(Call {
            key,
            sink,
            ..Call::new(event, timeout)
        })
        .map(|(o, _)| o)
    }

    pub fn call_timed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        each!(self, f => f.call_timed(event, timeout))
    }

    /// Serve one request against a script that did not come with the zygote.
    ///
    /// The runtime-pool shape, and there are two of them. An **agent** pool
    /// loads the script in the forked child, under the child's own seccomp
    /// filter, and can stream, take a workspace and be narrowed per tenant. A
    /// **warm-exec** pool `execve`s a program the operator named with the
    /// script as its last argument — right for a language that starts in
    /// under a millisecond, and without any of those four things, because
    /// there is no protocol between the supervisor and the program to carry
    /// them.
    pub fn call_script_with_timeout(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
    ) -> Result<Outcome> {
        self.call_script_for(event, script, timeout, None)
            .map(|(outcome, _)| outcome)
    }

    /// The same, for a named tenant and under a name they chose.
    ///
    /// See [`WarmFn::call_script_for`] and `InFlight::key`.
    pub fn call_script_as(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
        sink: Option<ChunkSink<'_>>,
    ) -> Result<Outcome> {
        self.call_script_streaming(event, script, timeout, caller, key, sink)
            .map(|(outcome, _)| outcome)
    }

    /// The same, reporting where the request's time went.
    ///
    /// What `zygo bench warm --pool` measures: the phases are the same three
    /// a function's request has, so the two shapes can be compared line by
    /// line rather than only at the total.
    pub fn call_script_timed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_for(event, script, timeout, None)
    }

    /// The same, recording who the request is for. See
    /// [`WarmFn::call_script_for`].
    pub fn call_script_for(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_keyed(event, script, timeout, caller, None)
    }

    /// The same, under a name the caller chose. See `InFlight::key`.
    pub fn call_script_keyed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_streaming(event, script, timeout, caller, key, None)
    }

    /// The same, streaming output as it is produced.
    ///
    /// See [`WarmFn::call_streaming`].
    pub fn call_script_streaming(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
        sink: Option<ChunkSink<'_>>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_full(Call {
            script,
            caller,
            key,
            sink,
            ..Call::new(event, timeout)
        })
    }

    /// The whole of what one request can carry. Everything above narrows it.
    ///
    /// See [`Call`] for what each field means; `secrets` is the one a runtime
    /// pool's request brings of its own.
    pub fn call_full(&self, call: Call<'_>) -> Result<(Outcome, CallTiming)> {
        match self {
            Function::Agent(f) => f.call_streaming(call),
            // A warm-exec pool takes a script as the last word of its
            // command line; a warm-exec *function* has an `entry`
            // of its own and is not asked for one. Either way there is no
            // agent here, so `sink`, `workspace` and `tenant_limits` have
            // nowhere to go — see the note on `Function::call_full`.
            #[cfg(target_os = "linux")]
            Function::Exec(f) => {
                f.call_script_timed(call.event, call.script, call.timeout, call.secrets)
            }
        }
    }

    /// Ask the function to wind down. An agent is told so it can finish what
    /// it holds; a held sandbox has nothing to be told — its requests are
    /// children of the supervisor, not of the init, and finish on their own.
    /// Dropping the `Function` is what actually takes the sandbox down.
    pub fn shutdown(&self) -> Result<()> {
        match self {
            Function::Agent(f) => f.shutdown(),
            #[cfg(target_os = "linux")]
            Function::Exec(_) => Ok(()),
        }
    }
}
