//! The OpenAPI document, and the thing that keeps it honest.
//!
//! Hand-written rather than derived from annotations. The alternative on offer
//! was `utoipa`, which means a proc-macro on every handler and a dependency
//! whose output nobody reads until it is wrong; what an SDK author actually
//! needs is prose about *why* `?out=1` exists, which no derive produces.
//!
//! What keeps a hand-written document from drifting is not discipline — it is
//! [`tests::every_route_in_the_router_is_in_the_document`], which reads
//! `api.rs`'s own source with `include_str!`, extracts every arm of the
//! routing `match`, and fails if one is missing here. Adding a route without
//! documenting it is a test failure rather than a thing somebody notices in
//! six months.
//!
//! `zygo api --openapi` prints it. The `info.version` is the binary's, and the
//! `x-zygo-api` extension is [`super::api::API_VERSION`] — the number a client
//! checks, which moves only when a route changes incompatibly.

use super::api::API_VERSION;

/// One route, as both the router and the document see it.
pub struct Route {
    pub method: &'static str,
    /// The path as OpenAPI writes it: `/fn/{name}`.
    pub path: &'static str,
    pub summary: &'static str,
    /// Who may call it: `any`, `tenant-or-operator`, `operator`.
    pub who: &'static str,
    /// Needs deploy rights — `--allow-deploy`, or a minted operator token.
    pub deploy: bool,
}

/// Every route this API answers.
///
/// The order is the router's, so the two can be read side by side.
pub const ROUTES: &[Route] = &[
    Route {
        method: "get",
        path: "/healthz",
        summary: "Whether this host should be sent work.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/version",
        summary: "Zygo's version, the HTTP surface's, and the control protocol's.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/metrics",
        summary: "Prometheus text exposition.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/fn",
        summary: "Warm functions. A tenant token sees its own.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "post",
        path: "/fn/{name}",
        summary: "Call a warm function. `?stream=1` for NDJSON, `?out=1` for the workspace back.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "put",
        path: "/fn/{name}",
        summary: "Warm a function, replacing whatever held the name.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "delete",
        path: "/fn/{name}",
        summary: "Stop a function.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "post",
        path: "/fn/{name}/batch",
        summary: "Several events at once; each element carries its own status.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/fn/{name}/stats",
        summary: "One function's counters.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/fn/{name}/logs",
        summary: "A function's recent log.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "post",
        path: "/fn/{name}/warm",
        summary: "Bring a registered function up without calling it.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/runtimes",
        summary: "Runtime pools. A tenant token sees its own.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "post",
        path: "/runtimes",
        summary: "Register a runtime pool: an image and a dependency set, no code.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "delete",
        path: "/runtimes/{name}",
        summary: "Stop a pool and drop its zygotes.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "post",
        path: "/runtimes/{name}/call",
        summary: "Run one script in a pool. `?stream=1`, `?out=1`.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "put",
        path: "/scripts",
        summary: "Register a script; the body is the script. Idempotent by digest.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/scripts/{digest}",
        summary: "Whether this host holds a script, and how big it is.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "delete",
        path: "/scripts/{digest}",
        summary: "Forget a script. The store is shared by digest.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "put",
        path: "/blobs",
        summary: "Store a tar for workspaces; the body is the tar. Idempotent by digest.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/blobs/{digest}",
        summary: "Whether this host holds a blob, and how big it is.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "delete",
        path: "/blobs/{digest}",
        summary: "Forget a blob. The store is shared by digest.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "post",
        path: "/run",
        summary: "One-shot sandbox. Names an image and a command, so it is a shell.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "delete",
        path: "/requests/{id}",
        summary: "Stop a running request, by its id or the key it was called with.",
        who: "any",
        deploy: false,
    },
    Route {
        method: "post",
        path: "/drain",
        summary: "Stop admitting, finish what is running, then exit.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "get",
        path: "/tenants",
        summary: "Every tenant this host holds.",
        who: "operator",
        deploy: false,
    },
    Route {
        method: "post",
        path: "/tenants",
        summary: "Register a tenant, or find the one already registered.",
        who: "operator",
        deploy: false,
    },
    Route {
        method: "get",
        path: "/tenants/{id}",
        summary: "One tenant. A tenant may read its own.",
        who: "tenant-or-operator",
        deploy: false,
    },
    Route {
        method: "delete",
        path: "/tenants/{id}",
        summary: "Forget a tenant: its work stops and its scripts, tokens and secrets go.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "patch",
        path: "/tenants/{id}/limits",
        summary: "What a tenant may not exceed. These only narrow.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "get",
        path: "/tenants/{id}/secrets",
        summary: "A tenant's secret **names**. Values cannot be read back.",
        who: "tenant-or-operator",
        deploy: false,
    },
    Route {
        method: "put",
        path: "/tenants/{id}/secrets/{name}",
        summary: "Store a secret; the body is the value. Encrypted at rest.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "delete",
        path: "/tenants/{id}/secrets/{name}",
        summary: "Forget a secret.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "post",
        path: "/tenants/{id}/tokens",
        summary: "Mint a token for a tenant. The secret is in the answer, once.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "get",
        path: "/tokens",
        summary: "Every token, hashes and all. Never a secret.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "post",
        path: "/tokens",
        summary: "Mint an operator token. The secret is in the answer, once.",
        who: "operator",
        deploy: true,
    },
    Route {
        method: "delete",
        path: "/tokens/{id}",
        summary: "Revoke a token, from the next request onwards.",
        who: "operator",
        deploy: true,
    },
];

/// The OpenAPI 3.1 document for this build.
pub fn document() -> serde_json::Value {
    let mut paths = serde_json::Map::new();
    for route in ROUTES {
        let entry = paths
            .entry(route.path.to_string())
            .or_insert_with(|| serde_json::json!({}));
        let operation = serde_json::json!({
            "summary": route.summary,
            "operationId": operation_id(route),
            "x-zygo-who": route.who,
            "x-zygo-deploy": route.deploy,
            "parameters": parameters(route.path),
            "responses": {
                "200": { "description": "the call succeeded" },
                "default": {
                    "description": "a failure, as JSON with an `error` and often a `code`",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } },
                },
            },
        });
        entry[route.method] = operation;
    }

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Zygo",
            "summary": "Warm sandboxes over HTTP.",
            "version": env!("CARGO_PKG_VERSION"),
            "license": { "name": "Apache-2.0" },
        },
        // The number a client checks. Bumped only when a route changes
        // incompatibly, so it stays put across releases that change what
        // happens behind one — which is why it is not `info.version`.
        "x-zygo-api": API_VERSION,
        "servers": [{ "url": "http://127.0.0.1:7700" }],
        "components": {
            "securitySchemes": {
                "bearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "An operator or tenant token. `zygo token mint`.",
                },
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    "required": ["error"],
                    "properties": {
                        "error": { "type": "string" },
                        "code": { "type": "string" },
                    },
                },
            },
        },
        "security": [{ "bearer": [] }],
        "paths": paths,
    })
}

/// `postFnName`, from the method and the path.
fn operation_id(route: &Route) -> String {
    let mut out = route.method.to_string();
    for part in route.path.split('/').filter(|p| !p.is_empty()) {
        let word = part.trim_matches(['{', '}']);
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// One `path` parameter per `{brace}` in the template.
fn parameters(path: &str) -> Vec<serde_json::Value> {
    path.split('/')
        .filter(|p| p.starts_with('{') && p.ends_with('}'))
        .map(|p| {
            let name = p.trim_matches(['{', '}']);
            serde_json::json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every arm of the router's `match` appears in [`ROUTES`].
    ///
    /// The whole reason a hand-written document is safe. It reads `api.rs`'s
    /// source — not its behaviour, which cannot be enumerated from outside —
    /// and fails when a route is added without being documented.
    ///
    /// `include_str!` rather than a build script: it is the same file the
    /// compiler just read, so there is no step that can be skipped and no
    /// generated artefact to go stale.
    #[test]
    fn every_route_in_the_router_is_in_the_document() {
        let source = include_str!("api.rs");
        let documented: BTreeSet<String> = ROUTES
            .iter()
            .map(|r| format!("{} {}", r.method.to_uppercase(), r.path))
            .collect();

        let mut found = 0;
        for line in source.lines() {
            let Some(arm) = route_of(line) else { continue };
            found += 1;
            assert!(
                documented.contains(&arm),
                "`{arm}` is a route and is not in ROUTES — add it to \
                 cmd/openapi.rs, with a summary somebody writing a client \
                 could use"
            );
        }
        assert!(
            found > 30,
            "only {found} routes were found in the source; the parser below \
             has stopped matching the router's shape"
        );
    }

    /// Nothing is documented that the router does not answer.
    #[test]
    fn nothing_is_documented_that_does_not_exist() {
        let source = include_str!("api.rs");
        let real: BTreeSet<String> = source.lines().filter_map(route_of).collect();
        for route in ROUTES {
            let arm = format!("{} {}", route.method.to_uppercase(), route.path);
            assert!(
                real.contains(&arm),
                "`{arm}` is documented and the router has no such arm"
            );
        }
    }

    /// Any route this line declares, whichever shape it is written in.
    fn route_of(line: &str) -> Option<String> {
        route_arm(line).or_else(|| early_route(line))
    }

    /// The routes answered *before* the match.
    ///
    /// `/healthz` is one: it is unauthenticated, so it is answered above the
    /// bearer check rather than inside the table below it. A test that only
    /// knew about match arms would report it as documented-but-absent, which
    /// is the opposite of the truth.
    fn early_route(line: &str) -> Option<String> {
        let line = line.trim();
        let rest = line.strip_prefix("if req.method() == Method::")?;
        let (method, rest) = rest.split_once(' ')?;
        let path = rest.split('"').nth(1)?;
        Some(format!("{method} {path}"))
    }

    /// `(&Method::GET, ["fn", name, "logs"]) =>` becomes `GET /fn/{name}/logs`.
    fn route_arm(line: &str) -> Option<String> {
        let line = line.trim();
        let rest = line.strip_prefix("(&Method::")?;
        let (method, rest) = rest.split_once(',')?;
        let inside = rest.trim().strip_prefix('[')?.split(']').next()?;

        let mut path = String::new();
        for part in inside.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            path.push('/');
            if let Some(literal) = part.strip_prefix('"') {
                path.push_str(literal.trim_end_matches('"'));
            } else {
                // A binding — `name`, `id`, `digest` — is a path parameter.
                path.push('{');
                path.push_str(part);
                path.push('}');
            }
        }
        // The catch-all arms (`["fn", ..]`) are method-not-allowed, not
        // routes, and the `..` gives them away.
        if path.contains("{..}") {
            return None;
        }
        Some(format!("{method} {path}"))
    }

    /// The committed document is this build's, and a *removal* costs a bump.
    ///
    /// `spec/openapi.json` is checked in, so a change to the API shows up as a
    /// diff a reviewer reads rather than as a document nobody regenerated.
    ///
    /// The rule is not "any change bumps the version". Adding a route is
    /// compatible — an older client does not call it — and making every
    /// addition a breaking change would teach everybody to ignore the number.
    /// **Removing or renaming** one is not compatible, and that is what
    /// `x-zygo-api` is for, so that is what this insists on.
    #[test]
    fn the_committed_document_matches_this_build() {
        let committed: serde_json::Value =
            serde_json::from_str(include_str!("../../../../spec/openapi.json"))
                .expect("spec/openapi.json is not JSON");
        let current = document();

        let operations = |doc: &serde_json::Value| -> BTreeSet<String> {
            doc["paths"]
                .as_object()
                .expect("paths")
                .iter()
                .flat_map(|(path, item)| {
                    item.as_object()
                        .expect("an item")
                        .keys()
                        .map(|method| format!("{} {path}", method.to_uppercase()))
                        .collect::<Vec<_>>()
                })
                .collect()
        };

        let was = operations(&committed);
        let now = operations(&current);
        let removed: Vec<&String> = was.difference(&now).collect();
        if !removed.is_empty() {
            let before = committed["x-zygo-api"].as_u64().unwrap_or(0);
            assert!(
                u64::from(API_VERSION) > before,
                "{removed:?} were removed from the API, which an older client \
                 cannot survive — raise API_VERSION above {before}"
            );
        }

        assert_eq!(
            current, committed,
            "spec/openapi.json is not this build's document; regenerate it with \
             `zygo api --openapi > spec/openapi.json` and read the diff"
        );
    }

    #[test]
    fn the_document_is_openapi_with_a_version_a_client_can_check() {
        let doc = document();
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(doc["x-zygo-api"], API_VERSION);

        // One operation per route, and the parameters come from the path.
        let logs = &doc["paths"]["/fn/{name}/logs"]["get"];
        assert_eq!(logs["operationId"], "getFnNameLogs");
        assert_eq!(logs["parameters"][0]["name"], "name");
        assert_eq!(logs["parameters"][0]["in"], "path");

        // Two methods on one path are two operations, not two paths.
        assert!(doc["paths"]["/tokens"]["get"].is_object());
        assert!(doc["paths"]["/tokens"]["post"].is_object());
    }

    /// Every route says who may call it, in words the docs use.
    #[test]
    fn every_route_says_who_may_call_it() {
        for route in ROUTES {
            assert!(
                ["any", "tenant-or-operator", "operator"].contains(&route.who),
                "{} {} has `who = {}`",
                route.method,
                route.path,
                route.who
            );
            assert!(
                !route.summary.is_empty() && route.summary.ends_with('.'),
                "{} {} needs a summary that is a sentence",
                route.method,
                route.path
            );
            // Deploy is the operator's by construction: `may_deploy` refuses
            // a tenant token whatever the flag says.
            assert!(
                !route.deploy || route.who == "operator",
                "{} {} needs deploy rights and is not marked operator-only",
                route.method,
                route.path
            );
        }
    }
}
