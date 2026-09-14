//! ue2-mcp — MCP server (stdio) that runs and drives ue2emu emulator instances for firmware testing.
//! Documentation: docs/status/mcp.md.

mod config;
mod ctl;
mod http;
mod img;
mod instance;
mod ring;
mod tools;

use anyhow::Result;
use rmcp::ServiceExt;
use tokio::signal::unix::{signal, SignalKind};

const USAGE: &str = "\
ue2-mcp — MCP server (JSON-RPC over stdio) for the UE2-C64U-Emulator.

Start it from an MCP client (e.g. Claude Code .mcp.json); it takes no arguments.
Tools: emu_start, emu_stop, emu_list, emu_console, emu_screen, emu_screenshot, emu_button, emu_key,
emu_type, emu_wait, emu_expect, emu_rest, emu_control, emu_usb_sync, emu_cart_info, emu_cart_save.

Environment (all optional):
  UE2_REPO           emulator checkout (run/)                        [default: repo containing this binary]
  UE2EMU_BIN         ue2emu binary                                   [default: $UE2_REPO/target/release/ue2emu]
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

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let serve = async move {
        server.serve(rmcp::transport::stdio()).await?.waiting().await?;
        anyhow::Ok(())
    };
    tokio::select! {
        r = serve => if let Err(e) = r { eprintln!("ue2-mcp: {e:#}") },
        _ = term.recv() => eprintln!("ue2-mcp: SIGTERM"),
        _ = int.recv() => eprintln!("ue2-mcp: SIGINT"),
        _ = hup.recv() => eprintln!("ue2-mcp: SIGHUP"),
    }
    state.shutdown().await;
    // tokio's stdin reader sits in a blocking thread that would hold up runtime shutdown.
    std::process::exit(0);
}
