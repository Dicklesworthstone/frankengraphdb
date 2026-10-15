//! Embedded browser assets, not a second query or authorization surface.
//!
//! The existing HTTP handler enters this router only after its cancellation,
//! terminal-output and drain checks. Static assets contain no database names,
//! credentials or live results. Every data request still goes through the
//! existing capability-authorized schema/query routes and guarded transport.

use asupersync::http::h1::types::{Method, Response};

const CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; \
    connect-src 'self'; worker-src 'self'; img-src 'self' blob:; \
    base-uri 'none'; frame-ancestors 'none'; form-action 'none'; object-src 'none'";

fn asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    // Exact paths only: no filesystem, percent-decoding, path traversal,
    // user-selected file, directory listing or SPA catch-all over API routes.
    Some(match path {
        "/studio/" | "/studio/index.html" => {
            ("text/html; charset=utf-8", include_bytes!("index.html"))
        }
        "/studio/studio.css" => ("text/css; charset=utf-8", include_bytes!("studio.css")),
        "/studio/app.mjs" => ("text/javascript; charset=utf-8", include_bytes!("app.mjs")),
        "/studio/model.mjs" => (
            "text/javascript; charset=utf-8",
            include_bytes!("model.mjs"),
        ),
        "/studio/experience.mjs" => (
            "text/javascript; charset=utf-8",
            include_bytes!("experience.mjs"),
        ),
        "/studio/graph.mjs" => (
            "text/javascript; charset=utf-8",
            include_bytes!("graph.mjs"),
        ),
        "/studio/layout.mjs" => (
            "text/javascript; charset=utf-8",
            include_bytes!("layout.mjs"),
        ),
        "/studio/force.wasm" => ("application/wasm", include_bytes!("wasm/force.wasm")),
        "/studio/demo.mjs" => ("text/javascript; charset=utf-8", include_bytes!("demo.mjs")),
        "/studio/favicon.svg" => ("image/svg+xml", include_bytes!("favicon.svg")),
        _ => return None,
    })
}

struct Page {
    status: u16,
    reason: &'static str,
    mime: &'static str,
    body: &'static [u8],
    redirect: bool,
}

fn route(path: &str, method: &Method) -> Option<Page> {
    if path != "/" && path != "/studio" && !path.starts_with("/studio/") {
        return None;
    }
    let mut page = Page {
        status: 404,
        reason: "Not Found",
        mime: "text/plain; charset=utf-8",
        body: b"No such Studio asset.\n",
        redirect: false,
    };
    if *method != Method::Get {
        page.status = 405;
        page.reason = "Method Not Allowed";
        page.body = b"Studio assets accept GET only.\n";
    } else if path == "/" || path == "/studio" {
        page.status = 308;
        page.reason = "Permanent Redirect";
        page.body = b"Open /studio/\n";
        page.redirect = true;
    } else if let Some((mime, body)) = asset(path) {
        page.status = 200;
        page.reason = "OK";
        page.mime = mime;
        page.body = body;
    }
    Some(page)
}

pub(super) fn respond(path: &str, method: &Method) -> Option<Response> {
    let page = route(path, method)?;
    let mut response = Response::new(page.status, page.reason, page.body.to_vec())
        .with_header("Content-Type", page.mime)
        .with_header("Cache-Control", "no-store")
        .with_header("Content-Security-Policy", CSP)
        .with_header("X-Content-Type-Options", "nosniff")
        .with_header("X-Frame-Options", "DENY")
        .with_header("Referrer-Policy", "no-referrer")
        .with_header("Cross-Origin-Resource-Policy", "same-origin")
        .with_header("Cross-Origin-Opener-Policy", "same-origin")
        .with_header(
            "Permissions-Policy",
            "camera=(), microphone=(), geolocation=()",
        );
    if page.redirect {
        response = response.with_header("Location", "/studio/");
    }
    if page.status == 405 {
        response = response.with_header("Allow", "GET");
    }
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_asset_inventory_is_utf8_and_has_explicit_browser_mime_types() {
        for (path, mime) in [
            ("/studio/", "text/html; charset=utf-8"),
            ("/studio/index.html", "text/html; charset=utf-8"),
            ("/studio/studio.css", "text/css; charset=utf-8"),
            ("/studio/app.mjs", "text/javascript; charset=utf-8"),
            ("/studio/model.mjs", "text/javascript; charset=utf-8"),
            ("/studio/experience.mjs", "text/javascript; charset=utf-8"),
            ("/studio/graph.mjs", "text/javascript; charset=utf-8"),
            ("/studio/layout.mjs", "text/javascript; charset=utf-8"),
            ("/studio/demo.mjs", "text/javascript; charset=utf-8"),
            ("/studio/favicon.svg", "image/svg+xml"),
        ] {
            let page = route(path, &Method::Get).unwrap();
            assert_eq!(page.status, 200, "{path}");
            assert_eq!(page.mime, mime, "{path}");
            assert!(!page.body.is_empty());
            assert!(core::str::from_utf8(page.body).is_ok());
            assert!(!page.redirect);
        }
    }

    #[test]
    fn api_and_other_namespaces_never_fall_back_to_the_studio_shell() {
        for path in [
            "/v1/health",
            "/v1/databases/graph/query",
            "/v1/databases/graph/write",
            "/studio-other",
            "/favicon.ico",
            "https://other/studio/",
        ] {
            assert!(route(path, &Method::Get).is_none(), "{path}");
        }
        for path in [
            "/studio/../http.rs",
            "/studio/%2e%2e/http.rs",
            "/studio/tests/model.test.mjs",
            "/studio/.git/config",
            "/studio/missing",
            "/studio//app.mjs",
        ] {
            assert_eq!(route(path, &Method::Get).unwrap().status, 404, "{path}");
        }
    }

    #[test]
    fn assets_redirect_only_to_the_fixed_local_directory_and_never_accept_posts() {
        for path in ["/", "/studio"] {
            let page = route(path, &Method::Get).unwrap();
            assert_eq!(page.status, 308);
            assert!(page.redirect);
        }
        for path in ["/", "/studio", "/studio/", "/studio/app.mjs"] {
            let page = route(path, &Method::Post).unwrap();
            assert_eq!(page.status, 405);
            assert!(!page.redirect);
        }
        assert!(CSP.contains("connect-src 'self'"));
        assert!(CSP.contains("worker-src 'self'"));
        assert!(CSP.contains("frame-ancestors 'none'"));
        assert!(!CSP.contains("unsafe-inline"));
        // The Wasm-only keyword must not allow JavaScript string evaluation.
        assert!(CSP.contains("'wasm-unsafe-eval'"));
        assert!(!CSP.contains("'unsafe-eval'"));
    }

    #[test]
    fn force_module_is_binary_wasm_and_source_files_are_not_routes() {
        let page = route("/studio/force.wasm", &Method::Get).unwrap();
        assert_eq!(page.status, 200);
        assert_eq!(page.mime, "application/wasm");
        assert!(page.body.starts_with(b"\0asm\x01\0\0\0"));
        for path in [
            "/studio/wasm/force.c",
            "/studio/wasm/build.sh",
            "/studio/wasm/force.wasm",
        ] {
            assert_eq!(route(path, &Method::Get).unwrap().status, 404);
        }
        assert_eq!(
            route("/studio/force.wasm", &Method::Post).unwrap().status,
            405
        );
    }
}
