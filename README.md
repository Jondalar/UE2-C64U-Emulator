# UE2-C64U-Emulator

Run the firmware of the Ultimate 64 Elite II and the C64 Ultimate on your computer, and develop and test it without
flashing a device.

<p>
  <img src="docs/images/browser.png" width="49%" alt="File browser">
  <img src="docs/images/config.png" width="49%" alt="Settings menu">
  <img src="docs/images/action-menu.png" width="49%" alt="Action menu">
  <img src="docs/images/c64-ready.png" width="49%" alt="The C64 at READY">
</p>

The emulator runs the unmodified firmware (`ultimate.elf` or a `.ue2` update file) and models the hardware around it:
menu and file browser, flash, SD card and USB sticks, the network with REST API and web UI, and a C64 with SID sound,
cartridges and a 1541 drive. The C64 is [TRX64](https://github.com/Jondalar/TRX64). An MCP server lets Claude Code
sessions start and drive emulator instances for automated tests.

## Install

```sh
brew install jondalar/ue2emu/ue2emu
```

To build from source, see [docs/status/install.md](docs/status/install.md). macOS is the main platform. Linux builds
and passes CI but has not been used in practice. Windows is not supported.

## Getting started

Firmware and ROMs are not included. You need:

- a firmware image: your own `ultimate.elf` build or an `update.ue2` file
- the `roms` folder of a [1541ultimate](https://github.com/GideonZ/1541ultimate) checkout, for the menu font and the
  C64 ROMs

```sh
# Once: run the updater inside the .ue2 into a flash image and add the C64 ROMs.
ue2emu install --update update.ue2 --flash flash.bin --roms 1541ultimate/roms --yes --c64-roms

# Start. F12 opens the menu, the cursor keys navigate.
ue2emu run --firmware update.ue2 --flash flash.bin --roms 1541ultimate/roms --net user
```

With `--net user` the web UI is at http://127.0.0.1:8080. `ue2emu run --help` lists all options. They can also go
into a TOML file, started with `ue2emu run --config my.toml` ([example](docs/examples/ue2emu.example.toml)).

## Documentation

- [Usage](docs/status/install.md): build, options, config files, `install`, ROMs, MCP server
- [Status](docs/status/README.md): what works, by area
- [Architecture](docs/ARCHITECTURE.md)

## License

GPL-3.0-or-later. The cartridge logic is ported from GideonZ/1541ultimate (GPL); the SID sound is reSID, built by
TRX64.
