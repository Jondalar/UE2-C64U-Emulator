//! Device models. Each module owns its IO windows and exposes `install`.

pub mod board;
pub mod c64;
pub mod drives;
pub mod flash;
pub mod i2c;
pub mod iec;
pub mod itu;
pub mod misc;
pub mod overlay;
pub mod rmii;
pub mod sdcard;
pub mod streams;
pub mod u64io;
pub mod usb;
pub mod wifi;

use crate::io::IoMap;
use crate::machine::MachineConfig;

pub fn install_all(map: &mut IoMap, cfg: &MachineConfig) {
    itu::install(map, cfg);
    board::install(map, cfg);
    i2c::install(map, cfg);
    c64::install(map, cfg);
    usb::install(map, cfg);
    iec::install(map, cfg);
    misc::install(map, cfg);
    rmii::install(map, cfg);
    flash::install(map, cfg);
    wifi::install(map, cfg);
    sdcard::install(map, cfg);
    streams::install(map, cfg);
    u64io::install(map, cfg);
    overlay::install(map, cfg);
}
