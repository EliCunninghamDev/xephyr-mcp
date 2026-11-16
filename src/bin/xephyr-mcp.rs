use anyhow::Result;
use serde_json::json;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use xephyr_mcp::{mcp, trace, DisplayManager};

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "panic".to_string()
        };
        let location = info
            .location()
            .map(|place| format!("{}:{}:{}", place.file(), place.line(), place.column()))
            .unwrap_or_default();
        let _ = trace::append(
            &trace::server_path(),
            trace::event("panic", json!({ "message": message, "location": location })),
        );
        previous(info);
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--trace-stats") {
        let session = args.get(1).map(String::as_str);
        println!(
            "{}",
            serde_json::to_string_pretty(&trace::summary(session, 200, 20))?
        );
        return Ok(());
    }

    install_panic_hook();
    trace::record_event(
        "server_start",
        json!({
            "pid": std::process::id(),
            "cwd": std::env::current_dir().ok().map(|path| path.display().to_string()),
        }),
    );

    let manager = Arc::new(DisplayManager::new());
    eprintln!(
        "[xephyr-mcp] tools: {}",
        serde_json::to_string(&manager.tools.describe()).unwrap_or_default()
    );

    let cleanup = manager.clone();
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).ok();
        let mut hup = signal(SignalKind::hangup()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = async {
                match term.as_mut() {
                    Some(sig) => { sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {},
            _ = async {
                match hup.as_mut() {
                    Some(sig) => { sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {},
        }
        let _ = cleanup.stop_with_reason("signal").await;
        trace::record_event("server_exit", json!({ "reason": "signal" }));
        std::process::exit(0);
    });

    let result = mcp::serve(manager.clone()).await;
    if let Err(err) = manager.stop_with_reason("server_exit").await {
        eprintln!("[xephyr-mcp] cleanup failed: {err:#}");
    }
    trace::record_event("server_exit", json!({ "reason": "stdin_closed" }));
    result
}
