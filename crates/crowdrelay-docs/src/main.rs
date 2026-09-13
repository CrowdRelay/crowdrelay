//! Serves the OpenAPI contract as browsable Redoc documentation, locally.
//!
//! `openapi/openapi.yaml` is the supported integration boundary, and reading a
//! 320-path YAML file in an editor is not the same as being able to answer "what
//! does this endpoint return". This binds to loopback and serves that one file
//! plus the Redoc bundle that renders it.
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
    response::{IntoResponse, Response},
    routing::get,
};

/// Where the spec lives, relative to the workspace root.
const SPEC_RELATIVE: &str = "openapi/openapi.yaml";

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

    let state = DocsState { spec_path };
    let app = Router::new()
        .route("/", get(index))
        .route("/openapi.yaml", get(spec))
        .route("/healthz", get(|| async { "ok" }))
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
    println!("  spec  {SPEC_RELATIVE} (re-read on every request)");
    println!("  stop  Ctrl-C");

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
