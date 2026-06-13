//! Open a **local** file in the browser viewer without uploading it.
//!
//! Starts an ephemeral loopback HTTP server that serves the single file, then
//! opens it in a browser. Bioinformatics formats are wrapped in the matching
//! view.smarts.bio viewer (`?url=` pointing back at the loopback server);
//! everything else (images, PDFs, text) is opened directly so the browser
//! renders it natively.
//!
//! The server sends permissive CORS headers and answers the Private-Network-
//! Access preflight (`Access-Control-Allow-Private-Network: true`) so an HTTPS
//! viewer page is allowed to fetch from `http://127.0.0.1` (a potentially-
//! trustworthy origin, so it is not blocked as mixed content).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use tiny_http::{Header, Method, Response, Server};

/// Hosted viewer base; override with `SMARTSBIO_VIEWER_URL` (e.g.
/// `http://localhost:3012` for a local bio-viewers instance).
const DEFAULT_VIEWER_BASE: &str = "https://view.smarts.bio";

/// Stop serving if the browser never connects within this window.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub fn open(path: &str, print_url: bool) -> Result<()> {
    let file_path = PathBuf::from(path);
    if !file_path.is_file() {
        bail!("local file not found: {path}");
    }
    let filename = file_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = file_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let content_type = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();

    let server = Server::http("127.0.0.1:0")
        .map_err(|e| anyhow!("could not start local server: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("could not determine local server port"))?
        .port();

    let file_url = format!("http://127.0.0.1:{port}/{}", urlencoding::encode(filename));
    let open_url = match viewer_route_for(&ext) {
        Some(route) => format!(
            "{}/{}?url={}",
            viewer_base().trim_end_matches('/'),
            route,
            urlencoding::encode(&file_url)
        ),
        // No dedicated viewer (image, PDF, text, …): let the browser render it.
        None => file_url.clone(),
    };

    if print_url {
        println!("{open_url}");
        return Ok(());
    }

    println!(
        "Serving {} on 127.0.0.1:{port} — nothing is uploaded.",
        file_path.display()
    );
    println!("Opening {open_url}");
    println!("Press Ctrl-C when you are done viewing.");
    if let Err(e) = webbrowser::open(&open_url) {
        eprintln!("could not open a browser ({e}); open this URL yourself:\n  {open_url}");
    }

    serve(server, &file_path, &content_type)
}

/// Serve the file until interrupted (Ctrl-C), or until nothing connects within
/// [`IDLE_TIMEOUT`]. Stays alive after the first fetch so reloads keep working.
fn serve(server: Server, file_path: &Path, content_type: &str) -> Result<()> {
    let start = Instant::now();
    let mut served_any = false;

    let cors = header("Access-Control-Allow-Origin", "*");
    let pna = header("Access-Control-Allow-Private-Network", "true");
    let methods = header("Access-Control-Allow-Methods", "GET, OPTIONS");
    let allow_headers = header("Access-Control-Allow-Headers", "*");
    let ctype = header("Content-Type", content_type);

    loop {
        match server.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(request)) => match request.method() {
                // Private Network Access / CORS preflight.
                Method::Options => {
                    let resp = Response::empty(204)
                        .with_header(cors.clone())
                        .with_header(pna.clone())
                        .with_header(methods.clone())
                        .with_header(allow_headers.clone());
                    let _ = request.respond(resp);
                }
                Method::Get | Method::Head => match std::fs::File::open(file_path) {
                    Ok(file) => {
                        served_any = true;
                        let resp = Response::from_file(file)
                            .with_header(ctype.clone())
                            .with_header(cors.clone())
                            .with_header(pna.clone());
                        let _ = request.respond(resp);
                    }
                    Err(e) => {
                        let _ = request
                            .respond(Response::from_string(format!("error: {e}")).with_status_code(500));
                    }
                },
                _ => {
                    let _ = request.respond(Response::empty(405));
                }
            },
            // Idle tick: only auto-stop if the browser never connected at all.
            Ok(None) => {
                if !served_any && start.elapsed() > IDLE_TIMEOUT {
                    eprintln!("no request received within {}s — stopping.", IDLE_TIMEOUT.as_secs());
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn viewer_base() -> String {
    std::env::var("SMARTSBIO_VIEWER_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_VIEWER_BASE.to_string())
}

/// Map a file extension to a view.smarts.bio viewer route, or `None` to let the
/// browser render the file directly.
fn viewer_route_for(ext: &str) -> Option<&'static str> {
    match ext {
        "fa" | "fasta" | "fastq" | "fq" | "fna" | "faa" => Some("sequence"),
        "bam" | "sam" | "cram" => Some("alignment"),
        "vcf" | "bcf" => Some("variant"),
        "pdb" | "cif" | "mmcif" | "ent" => Some("structure"),
        "csv" | "tsv" => Some("information"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::viewer_route_for;

    #[test]
    fn maps_bio_extensions_to_viewers() {
        assert_eq!(viewer_route_for("fasta"), Some("sequence"));
        assert_eq!(viewer_route_for("bam"), Some("alignment"));
        assert_eq!(viewer_route_for("vcf"), Some("variant"));
        assert_eq!(viewer_route_for("pdb"), Some("structure"));
        assert_eq!(viewer_route_for("csv"), Some("information"));
    }

    #[test]
    fn images_have_no_viewer_route() {
        assert_eq!(viewer_route_for("jpeg"), None);
        assert_eq!(viewer_route_for("png"), None);
        assert_eq!(viewer_route_for("pdf"), None);
    }
}
