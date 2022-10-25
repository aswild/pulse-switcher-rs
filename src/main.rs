use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use clap::{ArgAction, Parser};
use pulsectl::controllers::{types::DeviceInfo, DeviceControl, SinkController};
use regex::RegexSet;
use serde::Deserialize;
use yall::{log_macros::*, Logger};

/// subset of DeviceInfo that we care about, specifically with name/description strings un-optioned
#[derive(Debug)]
struct Device {
    index: u32,
    name: String,
    desc: String,
}

impl From<&DeviceInfo> for Device {
    /// From a borrowed &DeviceInfo we clone the strings
    fn from(dev: &DeviceInfo) -> Device {
        Device {
            index: dev.index,
            name: dev.name.clone().unwrap_or_else(|| format!("[unknown name {}]", dev.index)),
            desc: dev
                .description
                .clone()
                .unwrap_or_else(|| format!("[unknown description {}]", dev.index)),
        }
    }
}

impl From<DeviceInfo> for Device {
    /// From an owned DeviceInfo we move the strings
    fn from(mut dev: DeviceInfo) -> Device {
        Device {
            index: dev.index,
            name: dev.name.take().unwrap_or_else(|| format!("[unknown name {}]", dev.index)),
            desc: dev
                .description
                .take()
                .unwrap_or_else(|| format!("[unknown description {}]", dev.index)),
        }
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} ({}, {})", self.desc, self.index, self.name)
    }
}

/// The config file which lists patterns to match against device names/descriptions.
/// Deserialized from $XDG_CONFIG_HOME/pulse-switcher/config.toml
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    /// Include devices whose name matches any of these regexes
    include_names: Vec<String>,
    /// Include devices whose description matches any of these regexes
    include_descriptions: Vec<String>,
    /// Exclude devices whose name matches any of these regexes
    exclude_names: Vec<String>,
    /// Exclude devices whose description matches any of these regexes
    exclude_descriptions: Vec<String>,
}

#[derive(Debug, Default)]
struct DeviceFilter {
    include_name: Option<RegexSet>,
    include_desc: Option<RegexSet>,
    exclude_name: Option<RegexSet>,
    exclude_desc: Option<RegexSet>,
}

impl DeviceFilter {
    fn from_config(conf: &Config) -> Result<DeviceFilter> {
        fn parse_regex_set(v: &[String]) -> Result<Option<RegexSet>> {
            Ok(if v.is_empty() {
                None
            } else {
                Some(RegexSet::new(v)?)
            })
        }

        Ok(DeviceFilter {
            include_name: parse_regex_set(&conf.include_names)?,
            include_desc: parse_regex_set(&conf.include_descriptions)?,
            exclude_name: parse_regex_set(&conf.exclude_names)?,
            exclude_desc: parse_regex_set(&conf.exclude_descriptions)?,
        })
    }

    fn filter(&self, dev: &Device) -> bool {
        let want_include = {
            if let Some(re) = &self.include_name {
                re.is_match(&dev.name)
            } else if let Some(re) = &self.include_desc {
                re.is_match(&dev.desc)
            } else {
                // if no include patterns, then include all devices
                self.include_name.is_none() && self.include_desc.is_none()
            }
        };
        trace!("want_include({}) = {}", dev.name, want_include);

        let want_exclude = {
            if let Some(re) = &self.exclude_name {
                re.is_match(&dev.name)
            } else if let Some(re) = &self.exclude_desc {
                re.is_match(&dev.desc)
            } else {
                // if no exclude patterns, then don't exclude any devices
                !(self.exclude_name.is_none() && self.exclude_desc.is_none())
            }
        };
        trace!("want_exclude({}) = {}", dev.name, want_exclude);

        want_include && !want_exclude
    }
}

/// Load the config file with the given path. May error if the file can't be read, isn't valid toml
/// matching the Config deserialization, or any regex can't compile.
fn load_config<P: AsRef<Path>>(path: P) -> Result<DeviceFilter> {
    let path = path.as_ref();
    debug!("loading config file {}", path.display());
    let config = fs::read_to_string(path).context("read failed")?;
    let config: Config = toml::from_str(&config).context("parse failed")?;
    DeviceFilter::from_config(&config).context("parse failed")
}

/// Load the default config file, which is `$XDG_CONFIG_HOME/pulse-switcher/config.toml`. If the
/// default file doesn't exist, return an empty/default config, but return error if it exists but
/// can't be loaded.
fn default_config() -> Result<DeviceFilter> {
    if let Some(mut file) = dirs_next::config_dir() {
        file.push("pulse-switcher");
        file.push("config.toml");
        if file.is_file() {
            load_config(file)
        } else {
            debug!("Default config file {} not found, using default", file.display());
            Ok(DeviceFilter::default())
        }
    } else {
        warn!("Failed to get XDG_CONFIG_HOME, using default config");
        Ok(DeviceFilter::default())
    }
}

#[derive(Debug, Parser)]
#[clap(disable_help_subcommand = true, max_term_width = 120, version)]
struct Args {
    /// Verbose output, can be repeated.
    ///
    /// Pass once to add debug messages, twice for trace messages.
    #[clap(global = true, short, long, action = ArgAction::Count)]
    verbose: u8,

    /// Quiet output, can be repeated. Conflicts with --verbose
    ///
    /// Pass to show only warnings/errors, twice for only errors, thrice for silence.
    #[clap(global = true, short, long, action = ArgAction::Count, conflicts_with = "verbose")]
    quiet: u8,

    /// Config file path. Default '$XDG_CONFIG_HOME/pulse-switcher/config.toml' if it exists.
    #[clap(global = true, short, long = "config", value_name = "FILE")]
    config_file: Option<PathBuf>,

    #[clap(subcommand)]
    cmd: Option<Command>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// List devices. (default command)
    ///
    /// List all available devices, the selected devices as filtered by the config file, and the
    /// current default device.
    List,

    /// Set the next filtered device as the new default device.
    ///
    /// The order of filtered devices is based on the order the PulseAudio returns them. If the
    /// current default device is not filtered, then the first filtered device will be used.
    Next,
}

fn run() -> Result<()> {
    let args = Args::parse();
    Logger::new().verbose(args.verbose.into()).quiet(args.quiet.into()).init();

    let dev_filter = match args.config_file {
        Some(ref file) => {
            load_config(file).with_context(|| format!("failed to load '{}'", file.display()))
        }
        None => default_config(),
    }?;
    debug!("dev_filter: {:#?}", dev_filter);

    let mut ctx = SinkController::create().context("failed to get SinkController")?;
    let all_devs: Vec<Device> = ctx
        .list_devices()
        .context("failed to list devices")?
        .into_iter()
        .map(Device::from)
        .collect();

    let default_dev =
        Device::from(ctx.get_default_device().context("failed to get default device")?);

    let matching_devs: Vec<&Device> =
        all_devs.iter().filter(|dev| dev_filter.filter(dev)).collect();

    match args.cmd.unwrap_or(Command::List) {
        Command::List => {
            println!("All devices:");
            for dev in all_devs.iter() {
                println!("{}", dev);
            }
            println!("\nMatching devices:");
            for dev in matching_devs.iter() {
                println!("{}", dev);
            }
            println!("\nDefault device: {}", default_dev);
        }

        Command::Next => {
            ensure!(!matching_devs.is_empty(), "no matching devices found");
            let new_idx = matching_devs
                .iter()
                .enumerate()
                // find the index of the default device in our vec of matching devs
                .find_map(|(idx, dev)| if dev.name == default_dev.name { Some(idx) } else { None })
                // next index, wrapping around as needed
                .map(|idx| (idx + 1) % matching_devs.len())
                // if the default dev isn't one of our matching devices, pick the first one.
                .unwrap_or(0);

            let new = matching_devs[new_idx];
            info!("Setting device '{}' the default PA sink", new);
            let ret = ctx.set_default_device(&new.name).context("failed setting default device")?;
            // I'm not sure when set_default_device returns Ok(false) rather than Err(_), probably
            // when we succesfully request the thing from libpulse, but the C API returns an error.
            ensure!(ret, "failed setting default device: API returned false");
        }
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        error!("{:#}", e);
        std::process::exit(1);
    }
}
