//! Serves the OpenAPI contract as browsable Redoc documentation, locally,
//! plus the workspace rustdoc tree at `/rustdoc/` when it has been built.
//!
//! `openapi/openapi.yaml` is the supported integration boundary, and reading a
//! 320-path YAML file in an editor is not the same as being able to answer "what
//! does this endpoint return". This binds to loopback and serves that one file
//! plus the Redoc bundle that renders it.
//!
//! The rustdoc side serves `target/doc/` as built by `just rustdoc` — the
//! contract answers "what does the API do", the rustdoc tree answers "what is
//! the code that does it", which is the question you have inside the brain or
//! domain crates. The tree is re-read from disk per request, so rebuilding the
//! docs and refreshing the browser shows the change.
//!
//! Bound to `127.0.0.1` and nothing else. The spec describes authority surfaces
//! and error contracts; it is not a secret, but a documentation server has no
//! business listening on a network interface, and a local tool that quietly
//! became reachable is the kind of thing nobody notices until it matters.
//!
//! The spec is read from disk on every request rather than embedded at compile
//! time, so editing the YAML and refreshing the browser shows the change — the
//! whole point of running it while working on the contract.
//!
//! ```text
//! just docs            # serve on 127.0.0.1:8088
//! just docs 9000       # or any other port
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use tower_http::services::ServeDir;

/// Where the spec lives, relative to the workspace root.
const SPEC_RELATIVE: &str = "openapi/openapi.yaml";

/// Where `cargo doc` writes, relative to the workspace root.
const RUSTDOC_RELATIVE: &str = "target/doc";

/// Redoc, from a CDN, pinned to a major version.
///
/// Not vendored: this runs on a developer's machine with a network, and a
/// vendored 2 MB bundle in the repository would be one more thing to keep
/// current. If the machine is offline the page says so rather than rendering
/// blank — see the `onerror` handler below.
const REDOC_BUNDLE: &str = "https://cdn.redoc.ly/redoc/v2.1.5/bundles/redoc.standalone.js";

#[derive(Clone)]
struct DocsState {
    spec_path: PathBuf,
    rustdoc_dir: PathBuf,
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(8088);

    // `CARGO_MANIFEST_DIR` is this crate; the spec is two levels up. Resolved at
    // runtime from the binary's own location would break under `cargo run` from
    // a different directory, which is how this is actually started.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let spec_path = root.join(SPEC_RELATIVE);

    if !spec_path.exists() {
        eprintln!("no spec at {}", spec_path.display());
        std::process::exit(1);
    }

    let rustdoc_dir = root.join(RUSTDOC_RELATIVE);
    let state = DocsState {
        spec_path,
        rustdoc_dir: rustdoc_dir.clone(),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/openapi.yaml", get(spec))
        .route("/healthz", get(|| async { "ok" }))
        // `nest` claims `/rustdoc` itself too, so no sibling route may sit on
        // the same path. The bare path and `/rustdoc/` both reach ServeDir,
        // which finds no index.html at the tree root (cargo doc gives each
        // crate its own directory) — the not-found fallback answers with the
        // crate index page instead, and missing files land there too.
        .nest_service(
            "/rustdoc",
            ServeDir::new(rustdoc_dir)
                .not_found_service(get(rustdoc_index).with_state(state.clone())),
        )
        .with_state(state);

    // Loopback only. See the module comment.
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("could not bind {addr}: {error}");
            eprintln!("another process may already hold that port; pass a different one");
            std::process::exit(1);
        }
    };
    println!("CrowdRelay API docs  http://{addr}");
    println!("  spec     {SPEC_RELATIVE} (re-read on every request)");
    println!("  rustdoc  http://{addr}/rustdoc/  ({RUSTDOC_RELATIVE}, `just rustdoc` to build)");
    println!("  stop     Ctrl-C");

    let server = axum::serve(listener, app);
    if let Err(error) = server
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}

/// The spec itself, re-read each time so an edit is visible on refresh.
async fn spec(State(state): State<DocsState>) -> Response {
    match tokio::fs::read(&state.spec_path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/yaml; charset=utf-8"),
                // No caching: the file changes while this is running, and a
                // cached spec would make an edit look like it did nothing.
                (header::CACHE_CONTROL, "no-store"),
            ],
            Body::from(bytes),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not read {}: {error}", state.spec_path.display()),
        )
            .into_response(),
    }
}

/// Landing page for the rustdoc tree: one link per documented crate.
///
/// `cargo doc` gives each crate its own directory and no root index, so this
/// lists whichever crate directories currently exist — a new crate shows up
/// here without this page ever being edited. Names come out of `target/doc`
/// verbatim; a directory that has no `index.html` is not a crate and is
/// skipped.
async fn rustdoc_index(State(state): State<DocsState>) -> Response {
    let mut crates: Vec<String> = Vec::new();
    let mut entries = match tokio::fs::read_dir(&state.rustdoc_dir).await {
        Ok(entries) => entries,
        Err(_) => {
            return (
                StatusCode::OK,
                Html(format!(
                    r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>CrowdRelay internals</title>
<style>body {{ margin: 3rem auto; max-width: 46rem; background: #0d1117; color: #c9d7e6; font: 14px/1.7 -apple-system, system-ui, sans-serif; }}
code {{ color: #58a6ff; }}</style></head><body>
<h1>Internals (rustdoc)</h1>
<p>No docs tree at <code>{}</code> yet.</p>
<p>Build it with <code>just rustdoc</code>, then refresh.</p>
<p><a href="/">&#8592; API contract</a></p>
</body></html>"#,
                    state.rustdoc_dir.display()
                )),
            )
                .into_response();
        }
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let is_crate_dir = entry
            .file_type()
            .await
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
            && entry.path().join("index.html").exists();
        if is_crate_dir && let Some(name) = entry.file_name().to_str() {
            crates.push(name.to_owned());
        }
    }
    crates.sort();

    let links = crates
        .iter()
        .map(|name| format!(r#"<li><a href="/rustdoc/{name}/">{name}</a></li>"#))
        .collect::<Vec<_>>()
        .join("\n");
    Html(format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>CrowdRelay internals</title>
<style>body {{ margin: 3rem auto; max-width: 46rem; background: #0d1117; color: #c9d7e6; font: 14px/1.7 -apple-system, system-ui, sans-serif; }}
a {{ color: #58a6ff; }} li {{ margin: .3rem 0; }}</style></head><body>
<h1>Internals (rustdoc)</h1>
<ul>
{links}
</ul>
<p><a href="/">&#8592; API contract</a></p>
</body></html>"#
    ))
    .into_response()
}

async fn index() -> Response {
    // Dark theme, matching the generated PDF reference so the two read as one
    // set of documentation rather than two tools that happen to cover the same
    // system.
    // `r##"` rather than `r#"`: the Redoc theme carries "#58a6ff", and the `"#`
    // in a colour literal closes an `r#"` raw string early.
    let page = format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CrowdRelay API</title>
<style>
  body {{ margin: 0; background: #0d1117; }}
  #offline {{
    display: none; color: #f85149; background: #151b23;
    border: 1px solid #2a3441; border-left: 2px solid #f85149;
    border-radius: 6px; margin: 3rem auto; padding: 1rem 1.25rem; max-width: 46rem;
    font: 14px/1.6 -apple-system, system-ui, sans-serif;
  }}
  #offline code {{ color: #c9d7e6; }}
</style>
</head>
<body>
<div id="offline">
  <b>Redoc could not load.</b><br>
  The renderer comes from a CDN and this machine could not reach it. The spec
  itself is still served at <code>/openapi.yaml</code>.
</div>
<a href="/rustdoc/" style="position:fixed;top:10px;right:14px;z-index:100;color:#8b949e;background:#151b23;border:1px solid #2a3441;border-radius:6px;padding:4px 10px;font:12px -apple-system,system-ui,sans-serif;text-decoration:none">internals &#8594;</a>
<redoc spec-url="/openapi.yaml"
       theme='{{"colors":{{"primary":{{"main":"#58a6ff"}}}},"typography":{{"fontFamily":"-apple-system, system-ui, sans-serif","code":{{"fontFamily":"SF Mono, Menlo, monospace"}}}}}}'
       hide-download-button></redoc>
<script src="{REDOC_BUNDLE}"
        onerror="document.getElementById('offline').style.display='block'"></script>
</body>
</html>"##
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page,
    )
        .into_response()
}
