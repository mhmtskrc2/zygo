//! `zygo spec validate` and `zygo spec explain`.
//!
//! `explain` prints the *effective* configuration — every default filled in,
//! every flag applied. With four layers of precedence, "what am I actually
//! running with?" needs a direct answer.

use std::path::Path;

use anyhow::Context;
use zygo_core::spec::{Layer, ResolveOptions, ResolvedFn, Spec};

use crate::cli::{Cli, SpecCommand};
use crate::output::{self, Style};

pub fn run(cli: &Cli, file: Option<&Path>, command: &SpecCommand) -> anyhow::Result<u8> {
    let spec = load(file)?;
    match command {
        SpecCommand::Validate => validate(cli, &spec),
        SpecCommand::Explain { name } => explain(cli, &spec, name.as_deref()),
    }
}

fn load(file: Option<&Path>) -> anyhow::Result<Spec> {
    Spec::discover(file)?.context(
        "no sandbox.toml found here or in any parent directory\n  \
         → create one, or pass -f <path>",
    )
}

fn validate(cli: &Cli, spec: &Spec) -> anyhow::Result<u8> {
    let style = Style::stdout();
    let names: Vec<String> = spec.function_names().map(str::to_string).collect();

    // Parsing already succeeded; the real work is resolving every function,
    // since that is where limits and network rules are checked.
    let mut warnings = Vec::new();
    for name in &names {
        let resolved = spec.resolve(Some(name), &Layer::default(), &ResolveOptions::default())?;
        warnings.extend(resolved.warnings);
    }
    let warnings = group_warnings(&warnings);

    if cli.json {
        output::json(&serde_json::json!({
            "file": spec.source.as_ref().map(|p| p.display().to_string()),
            "functions": names,
            "warnings": warnings,
            "ok": true,
        }))?;
        return Ok(0);
    }

    for w in &warnings {
        output::warn(w);
    }

    match names.len() {
        0 => println!("{} valid, but no functions are declared", style.yellow("!")),
        n => println!(
            "{} {} valid — {n} function{} ({})",
            style.green("✓"),
            spec.source
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "spec".into()),
            if n == 1 { "" } else { "s" },
            names.join(", ")
        ),
    }
    Ok(0)
}

fn explain(cli: &Cli, spec: &Spec, name: Option<&str>) -> anyhow::Result<u8> {
    let resolved = spec.resolve(name, &Layer::default(), &ResolveOptions::default())?;

    if cli.json {
        output::json(&to_json(&resolved))?;
        return Ok(0);
    }

    for w in &resolved.warnings {
        output::warn(w);
    }

    let style = Style::stdout();
    println!("{}", style.bold(&format!("fn {}", resolved.name)));

    let mut rows: Vec<Vec<String>> = vec![
        row("image", &resolved.image),
        row("isolation", &resolved.isolation.to_string()),
        row("seccomp", &resolved.seccomp.to_string()),
        row(
            "runtime",
            &resolved
                .runtime
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "warm-exec".into()),
        ),
    ];

    if let Some(entry) = &resolved.entry {
        rows.push(row("entry", &entry.display().to_string()));
        rows.push(row("mode", &resolved.mode.to_string()));
    }
    if !resolved.cmd.is_empty() {
        rows.push(row("cmd", &resolved.cmd.join(" ")));
    }

    let l = &resolved.limits;
    rows.extend([
        row("mem", &format!("{} (high {})", l.mem, l.mem_high)),
        row("cpu", &format!("{} cores", l.cpu)),
        row("pids", &l.pids.to_string()),
        row("timeout", &l.timeout.to_string()),
        row("scratch", &l.scratch.to_string()),
        row("nofile", &l.nofile.to_string()),
        row("network", &resolved.network.to_string()),
    ]);

    if !resolved.allow.is_empty() {
        rows.push(row(
            "allow",
            &resolved
                .allow
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    for m in &resolved.mounts {
        rows.push(row("mount", &m.to_string()));
    }
    for (k, v) in &resolved.env {
        rows.push(row("env", &format!("{k}={v}")));
    }
    if !resolved.secrets.is_empty() {
        rows.push(row("secrets", &resolved.secrets.join(", ")));
    }
    rows.extend([
        row("concurrency", &resolved.concurrency.to_string()),
        row("idle_timeout", &resolved.idle_timeout.to_string()),
        row("cold_after", &resolved.cold_after.to_string()),
    ]);

    print!("{}", output::table(&["field", "value"], &rows));
    Ok(0)
}

fn row(field: &str, value: &str) -> Vec<String> {
    vec![field.to_string(), value.to_string()]
}

/// Collapse warnings that repeat across functions into one line naming them
/// all.
///
/// Several warnings — the missing disk I/O limit above all — apply to every
/// function by default. Printing the same sentence once per function in a spec
/// with twenty of them buries the warnings that are actually specific.
fn group_warnings(warnings: &[String]) -> Vec<String> {
    // Warnings are formatted `fn.<name>.<field>: <message>`.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, (String, Vec<String>)> =
        std::collections::HashMap::new();

    for w in warnings {
        let Some((prefix, message)) = w.split_once(": ") else {
            order.push(w.clone());
            groups.insert(w.clone(), (w.clone(), Vec::new()));
            continue;
        };
        let field = prefix
            .strip_prefix("fn.")
            .and_then(|rest| rest.split_once('.'))
            .map(|(name, field)| (name.to_string(), field.to_string()));

        let Some((name, field)) = field else {
            order.push(w.clone());
            groups.insert(w.clone(), (w.clone(), Vec::new()));
            continue;
        };

        let key = format!("{field}: {message}");
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            (format!("{field}: {message}"), Vec::new())
        });
        entry.1.push(name);
    }

    order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .map(|(text, names)| match names.len() {
            0 => text,
            1 => format!("fn.{}.{text}", names[0]),
            _ => format!("{text} [{}]", names.join(", ")),
        })
        .collect()
}

/// Serialise a resolved function for `--json`.
///
/// Written out by hand rather than deriving `Serialize` on `ResolvedFn`: this
/// is a CLI output contract, and it should not change silently when a field is
/// added to the core type.
pub fn to_json(r: &ResolvedFn) -> serde_json::Value {
    serde_json::json!({
        "name": r.name,
        "image": r.image,
        "entry": r.entry.as_ref().map(|p| p.display().to_string()),
        "cmd": r.cmd,
        "mode": r.mode.to_string(),
        "runtime": r.runtime.as_ref().map(|x| x.to_string()),
        "isolation": r.isolation.to_string(),
        "seccomp": r.seccomp.to_string(),
        "workdir": r.workdir.display().to_string(),
        "user": r.user,
        "limits": {
            "mem": r.limits.mem.to_string(),
            "mem_high": r.limits.mem_high.to_string(),
            "swap": r.limits.swap.to_string(),
            "cpu": r.limits.cpu.cores(),
            "pids": r.limits.pids,
            "timeout": r.limits.timeout.to_string(),
            "scratch": r.limits.scratch.to_string(),
            "nofile": r.limits.nofile,
            "io_read": r.limits.io_read.map(|b| b.to_string()),
            "io_write": r.limits.io_write.map(|b| b.to_string()),
            "connections": r.limits.connections,
            "bandwidth": r.limits.bandwidth.map(|b| b.to_string()),
        },
        "network": r.network.to_string(),
        "allow": r.allow.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "mounts": r.mounts.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
        "env": r.env,
        "secrets": r.secrets,
        "concurrency": r.concurrency,
        "idle_timeout": r.idle_timeout.to_string(),
        "cold_after": r.cold_after.to_string(),
        "warnings": r.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_shared_by_several_functions_collapse_to_one_line() {
        let out = group_warnings(&[
            "fn.a.io_read: no disk I/O limit".into(),
            "fn.b.io_read: no disk I/O limit".into(),
            "fn.c.io_read: no disk I/O limit".into(),
            "fn.b.scratch: scratch is over half of mem".into(),
        ]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], "io_read: no disk I/O limit [a, b, c]");
        assert_eq!(out[1], "fn.b.scratch: scratch is over half of mem");
    }

    #[test]
    fn grouping_preserves_warnings_it_cannot_parse() {
        let out = group_warnings(&["something unstructured".into()]);
        assert_eq!(out, ["something unstructured"]);
    }

    #[test]
    fn json_output_reports_every_resolved_limit() {
        let spec = Spec::parse(
            "[fn.f]\nimage=\"alpine\"\ncmd=[\"/bin/true\"]\nmem=\"512M\"\n",
            None,
        )
        .unwrap();
        let resolved = spec
            .resolve(Some("f"), &Layer::default(), &ResolveOptions::default())
            .unwrap();
        let v = to_json(&resolved);

        assert_eq!(v["limits"]["mem"], "512M");
        assert_eq!(v["limits"]["pids"], 64);
        assert_eq!(v["limits"]["timeout"], "30s");
        assert_eq!(v["network"], "none");
        assert_eq!(
            v["runtime"],
            serde_json::Value::Null,
            "no entry ⇒ warm-exec"
        );
    }
}
