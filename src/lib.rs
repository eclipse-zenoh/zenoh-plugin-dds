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

pub async fn run(runtime: Runtime, args: ArgMatches<'_>) {
    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    let _ = env_logger::try_init();

    let scope: String = args
        .value_of("dds-scope")
        .map(String::from)
        .or_else(|| Some(String::from("")))
        .unwrap();

    let domain_id = if let Some(sdid) = args.value_of("dds-domain") {
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
    let dp = unsafe { dds_create_participant(domain_id, std::ptr::null(), std::ptr::null()) };

    let mut dds_plugin = DdsPlugin {
        scope,
        domain_id,
        allow_re,
        z,
        dp,
        dds_pub: HashMap::<String, DdsEntity>::new(),
        dds_sub: HashMap::<String, DdsEntity>::new(),
        routes_from_dds: HashMap::<String, dds_entity_t>::new(),
        routes_to_dds: HashMap::<String, dds_entity_t>::new(),
    };

    dds_plugin.run().await;
}

enum RouteStatus {
    Created,
    NotAllowed,
    _QoSConflict,
}

struct DdsPlugin {
    scope: String,
    domain_id: u32,
    allow_re: Option<Regex>,
    z: Arc<Session>,
    dp: dds_entity_t,
    // maps of all discovered DDS entities (indexed by DDS key)
    dds_pub: HashMap<String, DdsEntity>,
    dds_sub: HashMap<String, DdsEntity>,
    // maps of established routes from/to DDS (indexed by zenoh resource key)
    routes_from_dds: HashMap<String, dds_entity_t>,
    routes_to_dds: HashMap<String, dds_entity_t>,
}

impl DdsPlugin {
    const DDS_INFINITE_TIME: i64 = 0x7FFFFFFFFFFFFFFF;

    fn is_allowed(&self, path: &str) -> bool {
        match &self.allow_re {
            Some(re) => re.is_match(path),
            _ => true,
        }
    }

    async fn try_add_route_from_dds(
        &mut self,
        zkey: &str,
        pub_to_match: &DdsEntity,
    ) -> RouteStatus {
        if !self.is_allowed(zkey) {
            info!(
                "Ignoring Publication for resource {} as it is not allowed (see --allow option)",
                zkey
            );
            return RouteStatus::NotAllowed;
        }

        if self.routes_from_dds.contains_key(zkey) {
            // TODO: check if there is no QoS conflict with existing route
            debug!(
                "Route from DDS to resource {} already exists -- ignoring",
                zkey
            );
            return RouteStatus::Created;
        }

        // declare the zenoh resource and the publisher
        let rkey = ResKey::RName(zkey.to_string());
        let nrid = self.z.declare_resource(&rkey).await.unwrap();
        let rid = ResKey::RId(nrid);
        let _ = self.z.declare_publisher(&rid).await;

        info!(
            "New route: DDS '{}' => zenoh '{}' (rid={}) with type '{}'",
            pub_to_match.topic_name, zkey, rid, pub_to_match.type_name
        );

        // create matching DDS Reader that forwards to zenoh
        let qos = unsafe {
            let qos = dds_create_qos();
            dds_copy_qos(qos, pub_to_match.qos.0);
            dds_qset_ignorelocal(qos, dds_ignorelocal_kind_DDS_IGNORELOCAL_PARTICIPANT);
            dds_qset_history(qos, dds_history_kind_DDS_HISTORY_KEEP_ALL, 0);
            qos
        };
        let dr: dds_entity_t = create_forwarding_dds_reader(
            self.dp,
            pub_to_match.topic_name.clone(),
            pub_to_match.type_name.clone(),
            pub_to_match.keyless,
            QosHolder(qos),
            rid,
            self.z.clone(),
        );

        self.routes_from_dds.insert(zkey.to_string(), dr);
        RouteStatus::Created
    }

    async fn try_add_route_to_dds(&mut self, zkey: &str, sub_to_match: &DdsEntity) -> RouteStatus {
        if let Some(re) = &self.allow_re {
            if !re.is_match(zkey) {
                info!(
                        "Ignoring Subscription for resource {} as it is not allowed (see --allow option)",
                        zkey
                    );
                return RouteStatus::NotAllowed;
            }
        }

        if self.routes_from_dds.contains_key(zkey) {
            // TODO: check if there is no type or QoS conflict with existing route
            debug!(
                "Route from resource {} to DDS already exists -- ignoring",
                zkey
            );
            return RouteStatus::Created;
        }

        info!(
            "New route: zenoh '{}' => DDS '{}' with type '{}'",
            zkey, sub_to_match.topic_name, sub_to_match.type_name
        );

        // create matching DDS Writer that forwards data coming from zenoh
        let qos = unsafe {
            let qos = dds_create_qos();
            dds_copy_qos(qos, sub_to_match.qos.0);
            dds_qset_ignorelocal(qos, dds_ignorelocal_kind_DDS_IGNORELOCAL_PARTICIPANT);
            dds_qset_history(qos, dds_history_kind_DDS_HISTORY_KEEP_ALL, 0);

            // Workaround for the DDS Writer to correctly match with a FastRTPS Reader declaring a Reliability max_blocking_time < infinite
            let mut kind: dds_reliability_kind_t = dds_reliability_kind_DDS_RELIABILITY_RELIABLE;
            let mut max_blocking_time: dds_duration_t = 0;
            if dds_qget_reliability(qos, &mut kind, &mut max_blocking_time)
                && max_blocking_time < Self::DDS_INFINITE_TIME
            {
                // Add 1 nanosecond to max_blocking_time for the Publisher
                max_blocking_time += 1;
                dds_qset_reliability(qos, kind, max_blocking_time);
            }
            qos
        };

        let wr = create_forwarding_dds_writer(
            self.dp,
            sub_to_match.topic_name.clone(),
            sub_to_match.type_name.clone(),
            sub_to_match.keyless,
            QosHolder(qos),
        );

        // create zenoh subscriber
        let sub_info = SubInfo {
            reliability: Reliability::Reliable,
            mode: SubMode::Push,
            period: None,
        };

        let zn = self.z.clone();
        let rkey = ResKey::RName(zkey.to_string());
        let ton = sub_to_match.topic_name.clone();
        let tyn = sub_to_match.type_name.clone();
        let keyless = sub_to_match.keyless;
        let dp = self.dp;
        task::spawn(async move {
            let mut sub = zn.declare_subscriber(&rkey, &sub_info).await.unwrap();
            let stream = sub.stream();
            while let Some(d) = stream.next().await {
                log::trace!("Route data to DDS '{}'", &ton);
                unsafe {
                    let bs = d.payload.to_vec();
                    // As per the Vec documentation (see https://doc.rust-lang.org/std/vec/struct.Vec.html#method.into_raw_parts)
                    // the only way to correctly releasing it is to create a vec using from_raw_parts
                    // and then have its destructor do the cleanup.
                    // Thus, while tempting to just pass the raw pointer to cyclone and then free it from C,
                    // that is not necessarily safe or guaranteed to be leak free.
                    let (ptr, len, capacity) = bs.into_raw_parts();
                    let cton = CString::new(ton.clone()).unwrap().into_raw();
                    let ctyn = CString::new(tyn.clone()).unwrap().into_raw();
                    let st = cdds_create_blob_sertopic(
                        dp,
                        cton as *mut std::os::raw::c_char,
                        ctyn as *mut std::os::raw::c_char,
                        keyless,
                    );
                    drop(CString::from_raw(cton));
                    drop(CString::from_raw(ctyn));
                    let fwdp =
                        cdds_ddsi_payload_create(st, ddsi_serdata_kind_SDK_DATA, ptr, len as u64);
                    dds_writecdr(wr, fwdp as *mut ddsi_serdata);
                    drop(Vec::from_raw_parts(ptr, len, capacity));
                    cdds_sertopic_unref(st);
                };
            }
        });

        self.routes_to_dds.insert(zkey.to_string(), wr);
        RouteStatus::Created
    }

    async fn run(&mut self) {
        let (tx, rx): (Sender<DiscoveryEvent>, Receiver<DiscoveryEvent>) = unbounded();
        run_discovery(self.dp, tx);

        loop {
            select!(
                me = rx.recv().fuse() => {
                    match me.unwrap() {
                        DiscoveryEvent::DiscoveredPublication {
                            entity
                        } => {
                            if entity.partitions.is_empty() {
                                let zkey = format!("{}/{}", self.scope, entity.topic_name);
                                let _route_status = self.try_add_route_from_dds(&zkey, &entity).await;
                            } else {
                                for p in &entity.partitions {
                                    let zkey = format!("{}/{}/{}", self.scope, p, entity.topic_name);
                                    let _route_status = self.try_add_route_from_dds(&zkey, &entity).await;
                                }
                            }
                            self.dds_pub.insert(entity.key.clone(), entity);
                        }
                        DiscoveryEvent::UndiscoveredPublication {
                            key,
                        } => {
                            if let Some(_entity) = self.dds_pub.remove(&key) {
                                // TODO: check is matching routes are unused and remove them
                            }
                        }
                        DiscoveryEvent::DiscoveredSubscription {
                            entity
                        } => {
                            if entity.partitions.is_empty() {
                                let zkey = format!("{}/{}", self.scope, entity.topic_name);
                                let _route_status = self.try_add_route_to_dds(&zkey, &entity).await;
                            } else {
                                for p in &entity.partitions {
                                    let zkey = format!("{}/{}/{}", self.scope, p, entity.topic_name);
                                    let _route_status = self.try_add_route_to_dds(&zkey, &entity).await;
                                }
                            }
                            self.dds_sub.insert(entity.key.clone(), entity);
                        }
                        DiscoveryEvent::UndiscoveredSubscription {
                            key,
                        } => {
                            if let Some(_entity) = self.dds_sub.remove(&key) {
                                // TODO: check is matching routes are unused and remove them
                            }
                        }
                    }
                }
            )
        }
    }
}
