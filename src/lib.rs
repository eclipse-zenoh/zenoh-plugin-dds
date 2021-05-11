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
#![feature(vec_into_raw_parts)]

use async_std::channel::{unbounded, Receiver, Sender};
use async_std::task;
use clap::{Arg, ArgMatches};
use cyclors::*;
use futures::prelude::*;
use futures::select;
use log::{debug, info};
use regex::Regex;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;
use zenoh::net::runtime::Runtime;
use zenoh::net::*;

mod dds_mgt;
pub use dds_mgt::*;

#[no_mangle]
pub fn get_expected_args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    get_expected_args2()
}

// NOTE: temporary hack for static link of DDS plugin in zenoh-bridge-dds, thus it can call this function
// instead of relying on #[no_mangle] functions that will conflicts with those defined in REST plugin.
// TODO: remove once eclipse-zenoh/zenoh#89 is implemented
pub fn get_expected_args2<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::from_usage(
            "--dds-scope=[String]...   'A string used as prefix to scope DDS traffic.'\n"
        ),
        Arg::from_usage(
            "--dds-generalise-pub=[String]...   'A comma separated list of key expression to use for generalising pubblications.'\n"
        ),
        Arg::from_usage(
            "--dds-generalise-sub=[String]...   'A comma separated list of key expression to use for generalising subscriptions.'\n"
        ),
        Arg::from_usage(
            "--dds-domain=[ID] 'The DDS Domain ID (if using with ROS this should be the same as ROS_DOMAIN_ID).'\n"
        ),
        Arg::from_usage(
            "--dds-allow=[String] 'The regular expression describing set of /partition/topic-name that should be bridged, everything is forwarded by default.'\n"
        )
    ]
}

#[no_mangle]
pub fn start(runtime: Runtime, args: &'static ArgMatches<'_>) {
    async_std::task::spawn(run(runtime, args.clone()));
}

fn is_allowed(sre: &Option<Regex>, path: &str) -> bool {
    match sre {
        Some(re) => re.is_match(path),
        _ => true,
    }
}

pub async fn run(runtime: Runtime, args: ArgMatches<'_>) {
    const DDS_INFINITE_TIME: i64 = 0x7FFFFFFFFFFFFFFF;

    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    let _ = env_logger::try_init();

    let scope: String = args
        .value_of("dds-scope")
        .map(String::from)
        .or_else(|| Some(String::from("")))
        .unwrap();

    let did = if let Some(sdid) = args.value_of("dds-domain") {
        match sdid.parse::<u32>() {
            Ok(adid) => adid,
            Err(_) => panic!("ERROR: {} is not a valid domain ID ", sdid),
        }
    } else {
        DDS_DOMAIN_DEFAULT
    };

    let allow_re = if let Some(res) = args.value_of("dds-allow") {
        match Regex::new(res) {
            Ok(re) => Some(re),
            Err(e) => {
                panic!("Unable to compile allow regular expression, please see error details below:\n {:?}\n", e)
            }
        }
    } else {
        None
    };

    let join_subscriptions: Vec<String> = args
        .values_of("dds-generalise-sub")
        .unwrap_or_default()
        .map(|s| s.to_string())
        .collect::<Vec<String>>();
    let join_publications: Vec<String> = args
        .values_of("dds-generalise-pub")
        .unwrap_or_default()
        .map(|s| s.to_string())
        .collect::<Vec<String>>();

    // open zenoh-net Session
    let z = Arc::new(Session::init(runtime, true, join_subscriptions, join_publications).await);

    // create DDS Participant
    let dp = unsafe { dds_create_participant(did, std::ptr::null(), std::ptr::null()) };
    let (tx, rx): (Sender<MatchedEntity>, Receiver<MatchedEntity>) = unbounded();
    run_discovery(dp, tx);
    let mut rid_map = HashMap::<String, ResourceId>::new();
    let mut rd_map = HashMap::<String, dds_entity_t>::new();
    let mut wr_map = HashMap::<String, dds_entity_t>::new();
    let _zsub_map = HashMap::<String, CallbackSubscriber>::new();
    loop {
        select!(
            me = rx.recv().fuse() => {
                match me.unwrap() {
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
        )
    }

    drop(rx);
}
