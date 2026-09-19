//! `run --config FILE.toml`: `run` flags from a TOML file (docs/examples/ue2emu.example.toml).
//!
//! - A key is a long flag of `run` without its dashes (`firmware`, `c64-roms`, `usb-dir`, `web-port`, ...) or a
//!   visible alias (`elf`); `config` is not a key.
//! - Values are written as on the command line: a string or number for a flag with a value; `true` (given) or `false`
//!   (not given) for a flag without one; an array for a repeatable flag, one occurrence per element (a single value
//!   is one occurrence); `true` (bare flag) or a string for a flag with an optional value (`c64-roms`).
//! - Relative paths resolve against the file's directory, a leading `~` or `~/` against $HOME: every path value and
//!   the path parts of `usb-dir PATH,...`, `cart-slot FILE.crt,...,save=OUT.crt` and `net socket-vmnet:PATH`.
//! - A flag on the command line replaces the file's value for that flag entirely (a repeatable flag's whole list);
//!   command-line values are not rewritten.
//!
//! The file's entries become `--flag=value` arguments inserted after `run`, leaving out the flags the command line
//! gives, and the whole command line is parsed again.

use std::any::TypeId;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::parser::ValueSource;
use clap::{Arg, ArgAction, Command};
use toml::{Table, Value};
use ue2_vfat::DirSpec;

use crate::cartslot::CartSlotSpec;
use crate::net::NetMode;

/// `argv` with the flags of `run --config FILE` inserted after `run`; unchanged without `--config`.
pub fn with_file(cmd: Command, mut argv: Vec<OsString>) -> Result<Vec<OsString>> {
    // This pass only learns the file and which flags the command line gives. Errors are ignored, so a `requires` the
    // file satisfies does not fail here; any other error is reported by the real parse.
    let mut cmd = cmd.ignore_errors(true);
    cmd.build();
    let Ok(matches) = cmd.try_get_matches_from_mut(argv.iter()) else { return Ok(argv) };
    let Some(("run", run)) = matches.subcommand() else { return Ok(argv) };
    let Some(file) = run.get_one::<PathBuf>("config") else { return Ok(argv) };
    let path = fs::canonicalize(file).with_context(|| format!("config: read {}", file.display()))?;
    let text = fs::read_to_string(&path).with_context(|| format!("config: read {}", path.display()))?;
    let table: Table = text.parse().with_context(|| format!("config: parse {}", path.display()))?;
    let run_cmd = cmd.find_subcommand("run").expect("run is a subcommand");
    let args = file_args(run_cmd, &path, &table, |id| run.value_source(id) == Some(ValueSource::CommandLine))?;
    let keys = table.len();
    eprintln!("config: loaded {} ({keys} key{})", path.display(), if keys == 1 { "" } else { "s" });
    let at = argv.iter().skip(1).position(|a| a == "run").map_or(argv.len(), |i| i + 2);
    argv.splice(at..at, args);
    Ok(argv)
}

/// The `--flag[=value]` arguments `table`, read from `path`, stands for, without the flags whose id is `given`.
fn file_args(run: &Command, path: &Path, table: &Table, given: impl Fn(&str) -> bool) -> Result<Vec<OsString>> {
    let file = path.display();
    let dir = path.parent().unwrap_or(Path::new(""));
    let (mut ids, mut out) = (Vec::new(), Vec::new());
    for (key, value) in table {
        if key == "config" {
            bail!("config: {file}: 'config' cannot be set in a config file");
        }
        let is_help = |a: &Arg| matches!(a.get_action(), ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong);
        let arg = run
            .get_arguments()
            .filter(|a| !is_help(a))
            .find(|a| a.get_long() == Some(key) || a.get_visible_aliases().is_some_and(|v| v.contains(&key.as_str())))
            .with_context(|| format!("config: {file}: unknown key '{key}'"))?;
        let id = arg.get_id().as_str();
        if let Some((_, other)) = ids.iter().find(|(i, _)| *i == id) {
            bail!("config: {file}: '{other}' and '{key}' are the same flag");
        }
        ids.push((id, key));
        if given(id) {
            continue;
        }
        let scalar = |v: &Value| match v {
            Value::String(s) => Some(s.clone()),
            Value::Integer(i) => Some(i.to_string()),
            Value::Float(f) => Some(f.to_string()),
            _ => None,
        };
        let optional = arg.get_num_args().is_some_and(|n| n.min_values() == 0);
        // One entry per occurrence: None for the bare flag.
        let occurrences: Option<Vec<Option<String>>> = match (arg.get_action(), value) {
            (ArgAction::SetTrue, Value::Boolean(b)) => Some(if *b { vec![None] } else { vec![] }),
            (ArgAction::SetTrue, _) => None,
            (ArgAction::Append, Value::Array(items)) => items.iter().map(|v| scalar(v).map(Some)).collect(),
            (ArgAction::Set, Value::Boolean(b)) if optional => Some(if *b { vec![None] } else { vec![] }),
            (_, v) => scalar(v).map(|s| vec![Some(s)]),
        };
        let expected = match arg.get_action() {
            ArgAction::SetTrue => "true or false",
            ArgAction::Append => "a string, a number or an array of them",
            _ if optional => "true, false, a string or a number",
            _ => "a string or a number",
        };
        let occurrences = occurrences.with_context(|| format!("config: {file}: '{key}' needs {expected}"))?;
        let long = arg.get_long().expect("found by its long name");
        for value in occurrences {
            let mut a = OsString::from(format!("--{long}"));
            if let Some(value) = value {
                a.push("=");
                a.push(resolve(arg, dir, &value));
            }
            out.push(a);
        }
    }
    Ok(out)
}

/// A file value of `arg` with its paths resolved (see the module doc). A spec that does not parse stays as it is, for
/// the real parse to report.
fn resolve(arg: &Arg, dir: &Path, value: &str) -> OsString {
    let parser = arg.get_value_parser().type_id();
    let path = |s: &str| resolve_path(dir, s).into_os_string();
    if parser == TypeId::of::<PathBuf>() {
        return path(value);
    }
    if parser == TypeId::of::<NetMode>() {
        if let Some(socket) = value.strip_prefix("socket-vmnet:") {
            let mut s = OsString::from("socket-vmnet:");
            s.push(path(socket));
            return s;
        }
    }
    // The spec parsers take their options from the end, so the parsed path is the start of the value.
    let file_len = if parser == TypeId::of::<DirSpec>() {
        value.parse::<DirSpec>().ok().map(|spec| spec.path.as_os_str().len())
    } else if parser == TypeId::of::<CartSlotSpec>() {
        value.parse::<CartSlotSpec>().ok().map(|spec| spec.path.as_os_str().len())
    } else {
        None
    };
    let Some(file_len) = file_len else { return value.into() };
    let (file, options) = value.split_at(file_len);
    let mut s = path(file);
    for option in options.split(',').skip(1) {
        s.push(",");
        match option.strip_prefix("save=") {
            Some(out) => {
                s.push("save=");
                s.push(path(out));
            }
            None => s.push(option),
        }
    }
    s
}

/// `~` and `~/...` under the home directory (`$HOME`, on Windows the user profile), a relative path under `dir`;
/// absolute and empty paths as they are.
fn resolve_path(dir: &Path, s: &str) -> PathBuf {
    match (s.strip_prefix('~'), std::env::home_dir()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) if rest.starts_with(['/', std::path::MAIN_SEPARATOR]) => home.join(&rest[1..]),
        _ if s.is_empty() => PathBuf::new(),
        _ => dir.join(s),
    }
}

#[cfg(test)]
mod tests {
    use clap::{ArgMatches, CommandFactory};

    use super::*;
    use crate::Cli;

    /// `ue2emu run --config <tmp>/ue2emu.toml CLI...` with the file holding `toml`, parsed: the `run` matches and the
    /// file's directory.
    fn parse(toml: &str, cli: &[&str]) -> Result<(ArgMatches, PathBuf)> {
        let tmp = tempfile::tempdir()?;
        let dir = fs::canonicalize(tmp.path())?;
        let file = dir.join("ue2emu.toml");
        fs::write(&file, toml)?;
        let mut argv: Vec<OsString> = ["ue2emu", "run", "--config"].map(OsString::from).into();
        argv.push(file.into());
        argv.extend(cli.iter().map(OsString::from));
        let argv = with_file(Cli::command(), argv)?;
        let matches = Cli::command().try_get_matches_from(argv)?;
        Ok((matches.subcommand_matches("run").context("run")?.clone(), dir))
    }

    fn error(toml: &str) -> String {
        format!("{:#}", parse(toml, &[]).err().expect("an error"))
    }

    fn paths(m: &ArgMatches, id: &str) -> Vec<PathBuf> {
        m.get_many::<PathBuf>(id).map(|v| v.cloned().collect()).unwrap_or_default()
    }

    /// An absolute path on the host.
    const ABS: &str = if cfg!(windows) { "C:/abs" } else { "/abs" };

    fn home() -> PathBuf {
        std::env::home_dir().expect("a home directory")
    }

    #[test]
    fn every_long_run_flag_is_a_key() {
        let mut cmd = Cli::command();
        cmd.build();
        let run = cmd.find_subcommand("run").unwrap();
        let mut flags = 0;
        for arg in run.get_arguments() {
            let Some(long) = arg.get_long().filter(|l| !matches!(*l, "help" | "config")) else { continue };
            let value = match arg.get_action() {
                ArgAction::SetTrue => Value::Boolean(true),
                ArgAction::Append => Value::Array(vec!["x".into()]),
                _ => Value::String("x".into()),
            };
            let table = Table::from_iter([(long.to_string(), value)]);
            let args = file_args(run, Path::new("/cfg/ue2emu.toml"), &table, |_| false).unwrap();
            assert!(args.len() == 1 && args[0].to_str().unwrap().starts_with(&format!("--{long}")), "{long}: {args:?}");
            flags += 1;
        }
        assert!(flags >= 30, "{flags} flags");
    }

    #[test]
    fn the_example_file_is_valid() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/ue2emu.example.toml");
        let (m, dir) = parse(&fs::read_to_string(example).unwrap(), &[]).unwrap();
        assert_eq!(paths(&m, "flash"), [dir.join("run/flash.bin")]);
        assert!(paths(&m, "elf")[0].starts_with(home()));
    }

    #[test]
    fn command_line_flags_replace_the_file_values() {
        let file = "flash = 'a.bin'\nusb = ['x.img', 'y.img']\nlog = ['io']\nmax-seconds = 5\nnet = 'user'\n";
        let (m, dir) = parse(file, &[]).unwrap();
        assert_eq!((paths(&m, "flash"), paths(&m, "images")), (vec![dir.join("a.bin")], vec![dir.join("x.img"), dir.join("y.img")]));
        assert_eq!(m.get_one::<f64>("max_seconds"), Some(&5.0));
        let (m, _) = parse(file, &["--flash", "/b.bin", "--usb", "z.img", "--log", "irq", "--web-port", "9000"]).unwrap();
        assert_eq!((paths(&m, "flash"), paths(&m, "images")), (vec![PathBuf::from("/b.bin")], vec![PathBuf::from("z.img")]));
        assert_eq!(m.get_many::<String>("log").unwrap().collect::<Vec<_>>(), ["irq"]);
        assert_eq!((m.get_one::<u16>("web_port"), m.contains_id("net")), (Some(&9000), true), "--net from the file");
        assert_eq!(m.get_one::<f64>("max_seconds"), Some(&5.0));
    }

    #[test]
    fn booleans_give_or_leave_out_flags() {
        let (m, _) = parse("headless = true\ntrace = false\nlog = 'io,irq'\n", &[]).unwrap();
        assert_eq!((m.get_flag("headless"), m.get_flag("trace")), (true, false));
        assert_eq!(m.get_many::<String>("log").unwrap().collect::<Vec<_>>(), ["io", "irq"]);
        let (m, _) = parse("headless = false\n", &["--headless"]).unwrap();
        assert!(m.get_flag("headless"));
    }

    #[test]
    fn c64_roms_is_true_or_a_directory() {
        let (m, _) = parse("flash = 'f.bin'\nc64-roms = true\n", &[]).unwrap();
        assert_eq!((m.value_source("c64_roms"), paths(&m, "c64_roms")), (Some(ValueSource::CommandLine), vec![]));
        let (m, dir) = parse("flash = 'f.bin'\nc64-roms = 'roms'\n", &[]).unwrap();
        assert_eq!(paths(&m, "c64_roms"), [dir.join("roms")]);
        let (m, _) = parse("c64-roms = false\n", &[]).unwrap();
        assert_eq!(m.value_source("c64_roms"), None);
    }

    #[test]
    fn paths_resolve_against_the_file_and_home() {
        let file = format!("flash = 'run/flash.bin'\nsd = '{ABS}/sd.img'\nfirmware = '~/c64u/x.ue2'\nroms = '~'\nscript = '../s.txt'\n");
        let (m, dir) = parse(&file, &["--audio-wav", "out.wav"]).unwrap();
        assert_eq!(paths(&m, "flash"), [dir.join("run/flash.bin")]);
        assert_eq!(paths(&m, "sd"), [PathBuf::from(format!("{ABS}/sd.img"))]);
        assert_eq!((paths(&m, "elf"), paths(&m, "roms")), (vec![home().join("c64u/x.ue2")], vec![home()]));
        assert_eq!(paths(&m, "script"), [dir.join("../s.txt")]);
        assert_eq!(paths(&m, "audio_wav"), [PathBuf::from("out.wav")], "command-line values stay");
    }

    #[test]
    fn spec_paths_resolve() {
        let file = format!(
            "cart-slot = 'games/a,1.crt,save=out/b.crt,flash-decode=15'\nusb-dir = ['sticks/one,size=512M,ro', '{ABS}/two']\n\
             net = 'socket-vmnet:vmnet.sock'\n"
        );
        let (m, dir) = parse(&file, &[]).unwrap();
        let cart = m.get_one::<CartSlotSpec>("cart_slot").unwrap();
        assert_eq!(cart.path, dir.join("games/a,1.crt"));
        assert_eq!((cart.target(), cart.flash_decode.as_str()), (Some(dir.join("out/b.crt").as_path()), "15"));
        let dirs: Vec<_> = m.get_many::<DirSpec>("dirs").unwrap().collect();
        assert_eq!((&dirs[0].path, dirs[0].read_only, &dirs[1].path), (&dir.join("sticks/one"), true, &PathBuf::from(format!("{ABS}/two"))));
        assert_eq!(m.get_one::<NetMode>("net"), Some(&NetMode::SocketVmnet(dir.join("vmnet.sock"))));
    }

    #[test]
    fn bad_keys_values_and_files_fail() {
        let named = |toml: &str, what: &str| {
            let e = error(toml);
            assert!(e.contains(what) && e.contains("ue2emu.toml"), "{toml:?}: {e}");
        };
        named("flsh = 'a'\n", "unknown key 'flsh'");
        named("config = 'b.toml'\n", "'config' cannot be set");
        named("flash = ['a', 'b']\n", "'flash' needs a string or a number");
        named("net = { mode = 'user' }\n", "'net' needs a string or a number");
        named("headless = 1\n", "'headless' needs true or false");
        named("c64-roms = [1]\n", "'c64-roms' needs true, false, a string or a number");
        named("log = [['io']]\n", "'log' needs a string, a number or an array of them");
        named("elf = 'a'\nfirmware = 'b'\n", "'elf' and 'firmware' are the same flag");
        named("flash = \n", "config: parse");
        let missing = ["ue2emu", "run", "--config", "/nonexistent/ue2emu.toml"].map(OsString::from).to_vec();
        assert!(format!("{:#}", with_file(Cli::command(), missing).unwrap_err()).contains("config: read /nonexistent/ue2emu.toml"));
    }

    #[test]
    fn without_config_the_arguments_stay() {
        for args in [&["ue2emu", "run", "--flash", "x.bin"][..], &["ue2emu", "install", "--help"], &["ue2emu"]] {
            let argv: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(with_file(Cli::command(), argv.clone()).unwrap(), argv);
        }
    }
}
