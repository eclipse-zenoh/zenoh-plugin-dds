#![feature(vec_into_raw_parts)]

use async_std::task;
use clap::{App, Arg};
use cyclors::*;
use futures::prelude::*;
use log::{debug, info};
use regex::Regex;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use zenoh::net::*;
use zenoh::Properties;
use zplugin_dds::*;

fn parse_args() -> (Properties, String, u32, Option<Regex>) {
    let args = App::new("dzd zenoh router for DDS")
        .arg(Arg::from_usage(
            "-e, --peer=[LOCATOR]...  'Peer locator used to initiate the zenoh session.'\n",
        ))
        .arg(Arg::from_usage(
            "-l, --listener=[LOCATOR]...   'Locators to listen on.'\n",
        ))
        .arg(Arg::from_usage(
            "-c, --config=[FILE]      'A configuration file.'\n",
        ))
        .arg(Arg::from_usage(
            "-s, --scope=[String]...   'A string used as prefix to scope DDS traffic.'\n",
        ))
        .arg(Arg::from_usage(
            "-w, --generalise-pub=[String]...   'A comma separated list of key expression to use for generalising pubblications.'\n",
        ))
        .arg(Arg::from_usage(
            "-r, --generalise-sub=[String]...   'A comma separated list of key expression to use for generalising subscriptions.'\n",
        ))
        .arg(
            Arg::from_usage("-m, --mode=[MODE]  'The zenoh session mode.'\n")
                .possible_values(&["peer", "client"])
                .default_value("client"),
        )
        .arg(
            Arg::from_usage(
                "-d, --domain=[ID] 'The DDS Domain ID (if using with ROS this should be the same as ROS_DOMAIN_ID).'\n")
        )
        .arg(
            Arg::from_usage(
                "-a, --allow=[String] 'The regular expression describing set of /partition/topic-name that should be bridged, everything is forwarded by default.'\n"
            )
        )
        .get_matches();

    let scope: String = args
        .value_of("scope")
        .map(String::from)
        .or_else(|| Some(String::from("")))
        .unwrap();

    let mut config: Properties = if let Some(conf_file) = args.value_of("config") {
        Properties::from(std::fs::read_to_string(conf_file).unwrap())
    } else {
        Properties::default()
    };
    config.insert("local_routing".into(), "false".into());
    config.insert("mode".into(), args.value_of("mode").unwrap().into());

    if let Some(value) = args.values_of("generalise-sub") {
        config.insert(
            "join_subscriptions".into(),
            value.collect::<Vec<&str>>().join(","),
        );
    }
    if let Some(value) = args.values_of("generalise-pub") {
        config.insert(
            "join_publications".into(),
            value.collect::<Vec<&str>>().join(","),
        );
    }
    if let Some(value) = args.values_of("peer") {
        config.insert("peer".into(), value.collect::<Vec<&str>>().join(","));
    }

    let allow = if let Some(res) = args.value_of("allow") {
        match Regex::new(res) {
            Ok(re) => Some(re),
            Err(e) => {
                panic!("Unable to compile allow regular expression, please see error details below:\n {:?}\n", e)
            }
        }
    } else {
        None
    };

    let did = if let Some(sdid) = args.value_of("domain") {
        match sdid.parse::<u32>() {
            Ok(adid) => adid,
            Err(_) => panic!("ERROR: {} is not a valid domain ID ", sdid),
        }
    } else {
        DDS_DOMAIN_DEFAULT
    };

    (config, scope, did, allow)
}

fn is_allowed(sre: &Option<Regex>, path: &str) -> bool {
    match sre {
        Some(re) => re.is_match(path),
        _ => true,
    }
}

#[async_std::main]
async fn main() {
    const DDS_INFINITE_TIME: i64 = 0x7FFFFFFFFFFFFFFF;
    env_logger::init();
    let (config, scope, did, allow_re) = parse_args();
    let dp = unsafe { dds_create_participant(did, std::ptr::null(), std::ptr::null()) };
    let z = Arc::new(open(config.into()).await.unwrap());
    let (tx, rx): (Sender<MatchedEntity>, Receiver<MatchedEntity>) = channel();
    run_discovery(dp, tx);
    let mut rid_map = HashMap::<String, ResourceId>::new();
    let mut rd_map = HashMap::<String, dds_entity_t>::new();
    let mut wr_map = HashMap::<String, dds_entity_t>::new();
    let _zsub_map = HashMap::<String, CallbackSubscriber>::new();
    while let Ok(me) = rx.recv() {
        match me {
            MatchedEntity::DiscoveredPublication {
                topic_name,
                type_name,
                keyless,
                partition,
                qos,
            } => {
                debug!(
                    "DiscoveredPublication({}, {}, {:?}",
                    topic_name, type_name, partition
                );
                let key = match partition {
                    Some(p) => format!("{}/{}/{}", scope, p, topic_name),
                    None => format!("{}/{}", scope, topic_name),
                };
                if !is_allowed(&allow_re, &key) {
                    info!(
                        "Ignoring Publication for key {} as it is not allowed (see --allow option)",
                        &key
                    );
                    break;
                }
                debug!("Declaring resource {}", key);
                match rd_map.get(&key) {
                    None => {
                        let rkey = ResKey::RName(key.clone());
                        let nrid = z.declare_resource(&rkey).await.unwrap();
                        let rid = ResKey::RId(nrid);
                        let _ = z.declare_publisher(&rid).await;
                        rid_map.insert(key.clone(), nrid);
                        info!(
                            "New route: DDS '{}' => zenoh '{}' (rid={}) with type '{}'",
                            topic_name, key, rid, type_name
                        );
                        let dr: dds_entity_t = create_forwarding_dds_reader(
                            dp,
                            topic_name,
                            type_name,
                            keyless,
                            qos,
                            rid,
                            z.clone(),
                        );
                        rd_map.insert(key, dr);
                    }
                    _ => {
                        debug!(
                            "Already forwarding matching subscription {} -- ignoring",
                            topic_name
                        );
                    }
                }
            }
            MatchedEntity::UndiscoveredPublication {
                topic_name,
                type_name,
                partition,
            } => {
                debug!(
                    "UndiscoveredPublication({}, {}, {:?}",
                    topic_name, type_name, partition
                );
            }
            MatchedEntity::DiscoveredSubscription {
                topic_name,
                type_name,
                keyless,
                partition,
                qos,
            } => {
                debug!(
                    "DiscoveredSubscription({}, {}, {:?}",
                    topic_name, type_name, partition
                );
                let key = match &partition {
                    Some(p) => format!("{}/{}/{}", scope, p, topic_name),
                    None => format!("{}/{}", scope, topic_name),
                };

                if !is_allowed(&allow_re, &key) {
                    info!("Ignoring subscription for key {} as it is not allowed (see --allow option)", &key);
                    break;
                }
                if let Some(wr) = match wr_map.get(&key) {
                    Some(_) => {
                        debug!(
                            "The Subscription({}, {}, {:?} is aready handled, IGNORING",
                            topic_name, type_name, partition
                        );
                        None
                    }
                    None => {
                        info!(
                            "New route: zenoh '{}' => DDS '{}' with type '{}'",
                            key, topic_name, type_name
                        );
                        // Workaround for the Publisher to correctly match with a FastRTPS Subscriber declaring a Reliability max_blocking_time < infinite
                        let mut kind: dds_reliability_kind_t =
                            dds_reliability_kind_DDS_RELIABILITY_RELIABLE;
                        let mut max_blocking_time: dds_duration_t = 0;
                        unsafe {
                            if dds_qget_reliability(qos.0, &mut kind, &mut max_blocking_time)
                                && max_blocking_time < DDS_INFINITE_TIME
                            {
                                // Add 1 nanosecond to max_blocking_time for the Publisher
                                max_blocking_time += 1;
                                dds_qset_reliability(qos.0, kind, max_blocking_time);
                            }
                        }

                        let wr = create_forwarding_dds_writer(
                            dp,
                            topic_name.clone(),
                            type_name.clone(),
                            keyless,
                            qos,
                        );
                        wr_map.insert(key.clone(), wr);
                        Some(wr)
                    }
                } {
                    debug!(
                        "The Subscription({}, {}, {:?} is new setting up zenoh and DDS endpoings",
                        topic_name, type_name, partition
                    );
                    let sub_info = SubInfo {
                        reliability: Reliability::Reliable,
                        mode: SubMode::Push,
                        period: None,
                    };

                    let zn = z.clone();
                    task::spawn(async move {
                        let rkey = ResKey::RName(key);
                        let mut sub = zn.declare_subscriber(&rkey, &sub_info).await.unwrap();
                        let stream = sub.stream();
                        while let Some(d) = stream.next().await {
                            log::trace!("Route data to DDS '{}'", topic_name);
                            let ton = topic_name.clone();
                            let tyn = type_name.clone();
                            unsafe {
                                let bs = d.payload.to_vec();
                                // As per the Vec documentation (see https://doc.rust-lang.org/std/vec/struct.Vec.html#method.into_raw_parts)
                                // the only way to correctly releasing it is to create a vec using from_raw_parts
                                // and then have its destructor do the cleanup.
                                // Thus, while tempting to just pass the raw pointer to cyclone and then free it from C,
                                // that is not necessarily safe or guaranteed to be leak free.
                                let (ptr, len, capacity) = bs.into_raw_parts();
                                let cton = CString::new(ton).unwrap().into_raw();
                                let ctyn = CString::new(tyn).unwrap().into_raw();
                                let st = cdds_create_blob_sertopic(
                                    dp,
                                    cton as *mut std::os::raw::c_char,
                                    ctyn as *mut std::os::raw::c_char,
                                    keyless,
                                );
                                drop(CString::from_raw(cton));
                                drop(CString::from_raw(ctyn));
                                let fwdp = cdds_ddsi_payload_create(
                                    st,
                                    ddsi_serdata_kind_SDK_DATA,
                                    ptr,
                                    len as u64,
                                );
                                dds_writecdr(wr, fwdp as *mut ddsi_serdata);
                                drop(Vec::from_raw_parts(ptr, len, capacity));
                                cdds_sertopic_unref(st);
                            };
                        }
                    });
                }
            }
            MatchedEntity::UndiscoveredSubscription {
                topic_name,
                type_name,
                partition,
            } => {
                debug!(
                    "UndiscoveredSubscription({}, {}, {:?}",
                    topic_name, type_name, partition
                );
            }
        }
    }
}
