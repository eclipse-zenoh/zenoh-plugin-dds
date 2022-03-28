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
use clap::{App, Arg};
use zenoh::config::Config;
use zenoh::prelude::*;

lazy_static::lazy_static!(
    pub static ref DEFAULT_DOMAIN_STR: String = zplugin_dds::config::DEFAULT_DOMAIN.to_string();
    pub static ref DEFAULT_GROUP_LEASE_STR: String = zplugin_dds::config::DEFAULT_GROUP_LEASE_SEC.to_string();
);

macro_rules! insert_json5 {
    ($config: expr, $args: expr, $key: expr, if $name: expr, $($t: tt)*) => {
        if $args.occurrences_of($name) > 0 {
            $config.insert_json5($key, &serde_json::to_string(&$args.value_of($name).unwrap()$($t)*).unwrap()).unwrap();
        }
    };
    ($config: expr, $args: expr, $key: expr, for $name: expr, $($t: tt)*) => {
        if let Some(value) = $args.values_of($name) {
            $config.insert_json5($key, &serde_json::to_string(&value$($t)*).unwrap()).unwrap();
        }
    };
}

fn parse_args() -> Config {
    let app = App::new("zenoh bridge for DDS")
        .version(zplugin_dds::GIT_VERSION)
        .long_version(zplugin_dds::LONG_VERSION.as_str())
        //
        // zenoh related arguments:
        //
        .arg(Arg::from_usage(
r#"-i, --id=[HEX_STRING] \
'The identifier (as an hexadecimal string, with odd number of chars - e.g.: 0A0B23...) that zenohd must use.
WARNING: this identifier must be unique in the system and must be 16 bytes maximum (32 chars)!
If not set, a random UUIDv4 will be used.'"#,
            ))
        .arg(Arg::from_usage(
r#"-m, --mode=[MODE]  'The zenoh session mode.'"#)
            .possible_values(&["peer", "client"])
            .default_value("peer")
        )
        .arg(Arg::from_usage(
r#"-c, --config=[FILE] \
'The configuration file. Currently, this file must be a valid JSON5 file.'"#,
            ))
        .arg(Arg::from_usage(
r#"-l, --listen=[ENDPOINT]... \
'A locator on which this router will listen for incoming sessions.
Repeat this option to open several listeners.'"#,
                ),
            )
        .arg(Arg::from_usage(
r#"-e, --connect=[ENDPOINT]... \
'A peer locator this router will try to connect to.
Repeat this option to connect to several peers.'"#,
            ))
        .arg(Arg::from_usage(
r#"--no-multicast-scouting \
'By default the zenoh bridge listens and replies to UDP multicast scouting messages for being discovered by peers and routers.
This option disables this feature.'"#
        ))
        .arg(Arg::from_usage(
r#"--rest-http-port=[PORT | IP:PORT] \
'Configures HTTP interface for the REST API (disabled by default, setting this option enables it). Accepted values:'
  - a port number
  - a string with format `<local_ip>:<port_number>` (to bind the HTTP server to a specific interface)."#
        ))
        //
        // DDS related arguments:
        //
        .arg(Arg::from_usage(
r#"-s, --scope=[String]   'A string added as prefix to all routed DDS topics when mapped to a zenoh resource. This should be used to avoid conflicts when several distinct DDS systems using the same topics names are routed via zenoh'"#
        ))
        .arg(Arg::from_usage(
r#"-d, --domain=[ID]   'The DDS Domain ID (if using with ROS this should be the same as ROS_DOMAIN_ID).'"#)
            .default_value(&*DEFAULT_DOMAIN_STR)
        )
        .arg(Arg::from_usage(
r#"--group-member-id=[ID]   'A custom identifier for the bridge, that will be used in group management (if not specified, the zenoh UUID is used).'"#
        ))
        .arg(Arg::from_usage(
r#"--group-lease=[Duration]   'The lease duration (in seconds) used in group management for all DDS plugins.'"#)
            .default_value(&*DEFAULT_GROUP_LEASE_STR)
        )
        .arg(Arg::from_usage(
r#"-a, --allow=[String]   'A regular expression matching the set of 'partition/topic-name' that must be routed via zenoh. By default, all partitions and topics are allowed.
If both '--allow' and '--deny' are set a partition and/or topic will be allowed if it matches only the 'allow' expression.
Examples of expressions: '.*/TopicA', 'Partition-?/.*', 'cmd_vel|rosout'...'"#
        ))
        .arg(Arg::from_usage(
r#"--deny=[String]   'A regular expression matching the set of 'partition/topic-name' that must not be routed via zenoh. By default, no partitions and no topics are denied.
If both '--allow' and '--deny' are set a partition and/or topic will be allowed if it matches only the 'allow' expression.
Examples of expressions: '.*/TopicA', 'Partition-?/.*', 'cmd_vel|rosout'...'"#
        ))
        .arg(Arg::from_usage(
r#"--max-frequency=[String]...   'Specifies a maximum frequency of data routing over zenoh for a set of topics. The string must have the format "<regex>=<float>":
  - "regex" is a regular expression matching the set of 'partition/topic-name' (same syntax than --allow option)
    for which the data (per DDS instance) must be routed at no higher rate than the specified max frequency.
  - "float" is the maximum frequency in Hertz; if publication rate is higher, downsampling will occur when routing.
Repeat this option to configure several topics expressions with a max frequency.'"#
        ))
        .arg(Arg::from_usage(
r#"-r, --generalise-sub=[String]...   'A list of key expression to use for generalising subscriptions (usable multiple times).'"#
        ))
        .arg(Arg::from_usage(
r#"-w, --generalise-pub=[String]...   'A list of key expression to use for generalising publications (usable multiple times).'"#
        ))
        .arg(Arg::from_usage(
r#"-f, --fwd-discovery   'When set, rather than creating a local route when discovering a local DDS entity, this discovery info is forwarded to the remote plugins/bridges. Those will create the routes, including a replica of the discovered entity.'"#
            ).alias("forward-discovery")
        );
    let args = app.get_matches();

    // load config file at first
    let mut config = match args.value_of("config") {
        Some(conf_file) => Config::from_file(conf_file).unwrap(),
        None => Config::default(),
    };
    // if "dds" plugin conf is not present, add it (empty to use default config)
    if config.plugin("dds").is_none() {
        config.insert_json5("plugins/dds", "{}").unwrap();
    }

    // apply zenoh related arguments over config
    // NOTE: only if args.occurrences_of()>0 to avoid overriding config with the default arg value
    if args.occurrences_of("id") > 0 {
        config
            .set_id(args.value_of("id").map(|s| s.to_string()))
            .unwrap();
    }
    if args.occurrences_of("mode") > 0 {
        config
            .set_mode(Some(args.value_of("mode").unwrap().parse().unwrap()))
            .unwrap();
    }
    if let Some(endpoints) = args.values_of("connect") {
        config
            .connect
            .endpoints
            .extend(endpoints.map(|p| p.parse().unwrap()))
    }
    if let Some(endpoints) = args.values_of("listen") {
        config
            .listen
            .endpoints
            .extend(endpoints.map(|p| p.parse().unwrap()))
    }
    if args.is_present("no-multicast-scouting") {
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
    }
    if let Some(port) = args.value_of("rest-http-port") {
        config
            .insert_json5("plugins/rest/http_port", &format!(r#""{}""#, port))
            .unwrap();
    }

    // Always add timestamps to publications (required for PublicationCache used in case of TRANSIENT_LOCAL topics)
    config.set_add_timestamp(Some(true)).unwrap();

    // apply DDS related arguments over config
    insert_json5!(config, args, "plugins/dds/scope", if "scope",);
    insert_json5!(config, args, "plugins/dds/domain", if "domain", .parse::<u64>().unwrap());
    insert_json5!(config, args, "plugins/dds/group_member_id", if "group-member-id", );
    insert_json5!(config, args, "plugins/dds/group_lease", if "group-lease", .parse::<f64>().unwrap());
    insert_json5!(config, args, "plugins/dds/allow", if "allow", );
    insert_json5!(config, args, "plugins/dds/deny", if "deny", );
    insert_json5!(config, args, "plugins/dds/max_frequencies", for "max-frequency", .collect::<Vec<_>>());
    insert_json5!(config, args, "plugins/dds/generalise_pubs", for "generalise-pub", .collect::<Vec<_>>());
    insert_json5!(config, args, "plugins/dds/generalise_subs", for "generalise-sub", .collect::<Vec<_>>());
    if args.is_present("fwd-discovery") {
        config
            .insert_json5("plugins/dds/forward_discovery", "true")
            .unwrap();
    }
    config
}

#[async_std::main]
async fn main() {
    use zenoh_plugin_trait::Plugin;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("z=info")).init();
    log::info!("zenoh-bridge-dds {}", *zplugin_dds::LONG_VERSION);

    let config = parse_args();
    let rest_plugin = config.plugin("rest").is_some();

    // create a zenoh Runtime (to share with plugins)
    let runtime = zenoh::net::runtime::Runtime::new(config).await.unwrap();

    // start REST plugin
    if rest_plugin {
        zplugin_rest::RestPlugin::start("rest", &runtime).unwrap();
    }

    // start DDS plugin
    zplugin_dds::DDSPlugin::start("dds", &runtime).unwrap();
    async_std::task::block_on(async_std::future::pending::<()>());
}
