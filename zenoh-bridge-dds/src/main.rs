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
use zenoh::config::Config;
use zenoh_plugin_trait::Plugin;

// customize the DDS plugin args for retro-compatibility with previous versions of the standalone bridge
fn customize_dds_args<'a, 'b>(mut args: Vec<Arg<'a, 'b>>) -> Vec<Arg<'a, 'b>> {
    // NOTE: no way to check what's each Arg is in the Vec!
    // We need to assume that there are 9, and that they are in correct order...
    // as specifed in src/lib.rs in get_expected_args()
    assert_eq!(9, args.len());
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
    let arg = args.remove(0).visible_alias("max-frequency");
    args.push(arg);
    let arg = args.remove(0).short("f").visible_alias("fwd-discovery");
    args.push(arg);

    args
}
macro_rules! insert_json {
    ($config: expr, $args: expr, $key: expr, if $name: expr, $($t: tt)*) => {
        if let Some(value) = $args.value_of($name) {
            $config.insert_json($key, &serde_json::to_string(&value$($t)*).unwrap()).unwrap();
        }
    };
    ($config: expr, $args: expr, $key: expr, for $name: expr, $($t: tt)*) => {
        if let Some(value) = $args.values_of($name) {
            $config.insert_json($key, &serde_json::to_string(&value$($t)*).unwrap()).unwrap();
        }
    };
    ($config: expr, $args: expr, $key: expr, $name: expr, $($t: tt)*) => {
        $config.insert_json($key, &serde_json::to_string(&$args.value_of($name)$($t)*).unwrap()).unwrap();
    };
}

fn parse_args() -> (Config, ArgMatches<'static>) {
    let app = App::new("zenoh bridge for DDS")
        .version(zplugin_dds::GIT_VERSION)
        .long_version(zplugin_dds::LONG_VERSION.as_str())
        .arg(Arg::from_usage(
            "-e, --peer=[LOCATOR]...  'Peer locator used to initiate the zenoh session (usable multiple times).'",
        ))
        .arg(Arg::from_usage(
            "-l, --listener=[LOCATOR]...   'Locators to listen on (usable multiple times).'",
        ))
        .arg(Arg::from_usage(
                "-i, --id=[hex_string] \
            'The identifier (as an hexadecimal string - e.g.: 0A0B23...) that the zenoh bridge must use. \
            WARNING: this identifier must be unique in the system! \
            If not set, a random UUIDv4 will be used.'",
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
        .arg(Arg::with_name("rest-http-port")
        .long("rest-http-port")
        .required(false)
        .takes_value(true)
        .value_name("PORT")
        .help("Maps to `--cfg=/plugins/rest/http_port:PORT`. Disabled by default."))
        .args(&customize_dds_args(dds_args()));

    let args = app.get_matches();

    let mut config = match args.value_of("config") {
        Some(conf_file) => Config::from_file(conf_file).unwrap(),
        None => Config::default(),
    };
    config
        .set_mode(Some(args.value_of("mode").unwrap().parse().unwrap()))
        .unwrap();
    if let Some(peers) = args.values_of("peer") {
        config.peers.extend(peers.map(|p| p.parse().unwrap()))
    }
    if let Some(listeners) = args.values_of("listener") {
        config
            .listeners
            .extend(listeners.map(|p| p.parse().unwrap()))
    }
    if args.is_present("no-multicast-scouting") {
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
    }
    config.set_add_timestamp(Some(true)).unwrap();
    if let Some(port) = args.value_of("rest-http-port") {
        config
            .insert_json("plugins/rest/http_port", &format!(r#""{}""#, port))
            .unwrap();
    }

    insert_json!(config, args, "plugins/dds/scope", "dds-scope", .unwrap());
    insert_json!(config, args, "plugins/dds/join_publications", for "dds-generalise-pub", .collect::<Vec<_>>());
    insert_json!(config, args, "plugins/dds/join_subscriptions", for "dds-generalise-sub", .collect::<Vec<_>>());
    insert_json!(config, args, "plugins/dds/domain", "dds-domain", .unwrap().parse::<u64>().unwrap());
    insert_json!(config, args, "plugins/dds/allow", if "dds-allow", );
    insert_json!(config, args, "plugins/dds/group_member_id", if "dds-group-member-id", );
    insert_json!(config, args, "plugins/dds/group_lease", if "dds-group-lease", .parse::<u64>().unwrap());
    insert_json!(config, args, "plugins/dds/max_frequencies", for "dds-max-frequency", .collect::<Vec<_>>());
    if args.is_present("dds-fwd-discovery") {
        config
            .insert_json("plugins/dds/forward_discovery", "true")
            .unwrap();
    }
    (config, args)
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
    let (config, args) = parse_args();
    let rest_plugin = config.plugin("rest").is_some();
    // create a zenoh Runtime (to share with plugins)
    let runtime = zenoh::net::runtime::Runtime::new(0, config, args.value_of("id"))
        .await
        .unwrap();

    // start REST plugin
    if rest_plugin {
        zplugin_rest::RestPlugin::start("rest", &runtime).unwrap();
    }

    // start DDS plugin
    zplugin_dds::DDSPlugin::start("dds", &runtime).unwrap();
    async_std::task::block_on(async_std::future::pending::<()>());
}

const GROUP_DEFAULT_LEASE: &str = "3";
fn dds_args() -> Vec<Arg<'static, 'static>> {
    vec![
            Arg::from_usage(
                "--dds-scope=[String]   'A string used as prefix to scope DDS traffic.'"
            ).default_value(""),
            Arg::from_usage(
                "--dds-generalise-pub=[String]...   'A list of key expression to use for generalising publications (usable multiple times).'"
            ),
            Arg::from_usage(
                "--dds-generalise-sub=[String]...   'A list of key expression to use for generalising subscriptions (usable multiple times).'"
            ),
            Arg::from_usage(
                "--dds-domain=[ID]   'The DDS Domain ID (if using with ROS this should be the same as ROS_DOMAIN_ID).'"
            ).default_value(&*zplugin_dds::DDS_DOMAIN_DEFAULT_STR),
            Arg::from_usage(
                "--dds-allow=[String]   'A regular expression matching the set of 'partition/topic-name' that should be bridged. \
                By default, all partitions and topic are allowed. \
                Examples of expressions: '.*/TopicA', 'Partition-?/.*', 'cmd_vel|rosout'...'"
            ),
            Arg::from_usage(
                "--dds-group-member-id=[ID]   'A custom identifier for the bridge, that will be used in group management (if not specified, the zenoh UUID is used).'"
            ),
            Arg::from_usage(
                "--dds-group-lease=[Duration]   'The lease duration (in seconds) used in group management for all DDS plugins.'"
            ).default_value(GROUP_DEFAULT_LEASE),
            Arg::from_usage(
r#"--dds-max-frequency=[String]...   'Specifies a maximum frequency of data routing over zenoh for a set of topics. The string must have the format "<regex>=<float>":
    . "regex" is a regular expression matching the set of
    'partition/topic-name' for which the data (per DDS instance) must
        be routed at no higher rate than associated max frequency
        (same syntax than --dds-allow option).
    . "float" is the maximum frequency in Hertz; if publication rate is
    higher, downsampling will occur when routing.
(usable multiple times)'"#
            ),
            Arg::from_usage(
                "--dds-fwd-discovery   'When set, rather than creating a local route when discovering a local DDS entity, \
                this discovery info is forwarded to the remote plugins/bridges. Those will create the routes, including a replica of the discovered entity.'"
            ),
        ]
}
