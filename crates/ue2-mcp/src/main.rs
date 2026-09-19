//! ue2-mcp — MCP server (stdio) that runs and drives ue2emu emulator instances for firmware testing.
//! Documentation: docs/status/mcp.md.

mod config;
mod ctl;
mod http;
mod img;
mod instance;
mod proc;
mod ring;
mod tools;

use anyhow::Result;
use rmcp::ServiceExt;

const USAGE: &str = "\
ue2-mcp — MCP server (JSON-RPC over stdio) for the UE2-C64U-Emulator.

Start it from an MCP client (e.g. Claude Code .mcp.json); it takes no arguments.
Tools: emu_start, emu_stop, emu_list, emu_console, emu_screen, emu_screenshot, emu_button, emu_key,
emu_type, emu_wait, emu_expect, emu_rest, emu_control, emu_usb_sync, emu_cart_info, emu_cart_save.

Environment (all optional):
  UE2_REPO           emulator checkout (run/)                        [default: repo containing this binary,
                                                                      else ~/.ue2emu]
  UE2EMU_BIN         ue2emu binary                                   [default: $UE2_REPO/target/release/ue2emu,
                                                                      outside a checkout the ue2emu next to this binary]
  UE2_FIRMWARE_TREE  default 1541ultimate checkout                   [default: $UE2_REPO/firmware/1541ultimate]
  UE2_MCP_RUN        instance directories                            [default: $UE2_REPO/run/mcp]

Docs: docs/status/mcp.md
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("ue2-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // Started by `instance::spawn_watchdog`, one per emulator instance.
    if args.first().is_some_and(|a| a == "--watchdog") {
        std::process::exit(proc::watchdog(&args[1..]));
    }
    if let Some(a) = args.first() {
        eprintln!("ue2-mcp: unexpected argument {a:?}; configuration is by environment (see --help)");
        std::process::exit(2);
    }

    let cfg = config::Config::from_env();
    eprintln!(
        "ue2-mcp {}: emulator {}, firmware tree {}, runs in {}",
        env!("CARGO_PKG_VERSION"),
        cfg.emulator.display(),
        cfg.firmware_tree.display(),
        cfg.run_base.display()
    );
    let server = tools::Emu::new(cfg);
    let state = server.state();

    let serve = async move {
        server.serve(rmcp::transport::stdio()).await?.waiting().await?;
        anyhow::Ok(())
    };
    tokio::select! {
        r = serve => if let Err(e) = r { eprintln!("ue2-mcp: {e:#}") },
        name = stop_request()? => eprintln!("ue2-mcp: {name}"),
    }
    state.shutdown().await;
    // tokio's stdin reader sits in a blocking thread that would hold up runtime shutdown.
    std::process::exit(0);
}

/// Resolves when the process is asked to stop: SIGTERM, SIGINT or SIGHUP; Ctrl-C, Ctrl-Break or the console
/// closing on Windows. Returns the name of what arrived.
fn stop_request() -> Result<impl std::future::Future<Output = &'static str>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (mut term, mut int, mut hup) =
            (signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?, signal(SignalKind::hangup())?);
        Ok(async move {
            tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
                _ = hup.recv() => "SIGHUP",
            }
        })
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};
        let (mut c, mut brk, mut close) = (ctrl_c()?, ctrl_break()?, ctrl_close()?);
        Ok(async move {
            tokio::select! {
                _ = c.recv() => "Ctrl-C",
                _ = brk.recv() => "Ctrl-Break",
                _ = close.recv() => "console closed",
            }
        })
    }
}
