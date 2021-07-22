//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
use clap::{App, Arg, ArgMatches};
use zenoh::Properties;
use zenoh_plugin_trait::Plugin;

// customize the DDS plugin args for retro-compatibility with previous versions of the standalone bridge
fn customize_dds_args<'a, 'b>(mut args: Vec<Arg<'a, 'b>>) -> Vec<Arg<'a, 'b>> {
    // NOTE: no way to check what's each Arg is in the Vec!
    // We need to assume that there are 7, and that they are in correct order...
    // as specifed in src/lib.rs in get_expected_args()
    assert_eq!(7, args.len());
    let arg = args.remove(0).short("s").visible_alias("scope");
    args.push(arg);
    let arg = args.remove(0).short("w").visible_alias("generalise-pub");
    args.push(arg);
    let arg = args.remove(0).short("r").visible_alias("generalise-sub");
    args.push(arg);
    let arg = args.remove(0).short("d").visible_alias("domain");
    args.push(arg);
    let arg = args.remove(0).short("a").visible_alias("allow");
    args.push(arg);
    let arg = args.remove(0).visible_alias("group-member-id");
    args.push(arg);
    let arg = args.remove(0).visible_alias("group-lease");
    args.push(arg);

    args
}

fn parse_args() -> (Properties, bool, ArgMatches<'static>) {
    let app = App::new("zenoh bridge for DDS")
        .version(zplugin_dds::GIT_VERSION)
        .long_version(zplugin_dds::LONG_VERSION.as_str())
        .arg(Arg::from_usage(
            "-e, --peer=[LOCATOR]...  'Peer locator used to initiate the zenoh session.'",
        ))
        .arg(Arg::from_usage(
            "-l, --listener=[LOCATOR]...   'Locators to listen on.'",
        ))
        .arg(Arg::from_usage(
            "-c, --config=[FILE]      'A configuration file.'",
        ))
        .arg(
            Arg::from_usage("-m, --mode=[MODE]  'The zenoh session mode.'")
                .possible_values(&["peer", "client"])
                .default_value("peer"),
        )
        .arg(
            Arg::from_usage(
                "--no-multicast-scouting \
                'By default the zenoh bridge listens and replies to UDP multicast scouting messages for being discovered by peers and routers. \
                This option disables this feature.'")
        )
        .arg(
            Arg::from_usage(
                "--rest-plugin   'Enable the zenoh REST plugin (disabled by default)'")
        )
        .args(&zplugin_rest::RestPlugin::get_requirements()) // add REST plugin expected args
        .args(&customize_dds_args(zplugin_dds::DDSPlugin::get_requirements()));

    let args = app.get_matches();

    let mut config: Properties = if let Some(conf_file) = args.value_of("config") {
        Properties::from(std::fs::read_to_string(conf_file).unwrap())
    } else {
        Properties::default()
    };
    config.insert("mode".into(), args.value_of("mode").unwrap().into());

    if let Some(value) = args.values_of("peer") {
        config.insert("peer".into(), value.collect::<Vec<&str>>().join(","));
    }

    if let Some(value) = args.values_of("listener") {
        config.insert("listener".into(), value.collect::<Vec<&str>>().join(","));
    }

    if args.is_present("no-multicast-scouting") {
        config.insert("multicast_scouting".into(), "false".into());
    }

    // Add timestamps to publications (required for PublicationCache usage)
    config.insert("add_timestamp".into(), "true".into());

    (config, args.is_present("rest-plugin"), args)
}

#[async_std::main]
async fn main() {
    // Temporary check, while "dzd" is in deprecation phase
    if let Ok(path) = std::env::current_exe() {
        if let Some(exe) = path.file_name() {
            if exe.to_string_lossy().starts_with("dzd") {
                println!("****");
                println!("**** WARNING: dzd has a new name: zenoh-bridge-dds");
                println!("****          Please use this new binary as 'dzd' is deprecated.");
                println!("****");
                println!();
            }
        }
    }

    env_logger::init();
    let (config, rest_plugin, args) = parse_args();

    // create a zenoh Runtime (to share with plugins)
    let runtime = zenoh::net::runtime::Runtime::new(0, config.into(), None)
        .await
        .unwrap();

    let runtime_args = (runtime, args);
    // start REST plugin
    if rest_plugin {
        zplugin_rest::RestPlugin::start(&runtime_args).unwrap();
    }

    // start DDS plugin
    zplugin_dds::DDSPlugin::start(&runtime_args).unwrap();
    async_std::task::block_on(async_std::future::pending::<()>());
}
