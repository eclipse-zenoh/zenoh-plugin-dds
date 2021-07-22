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
use async_std::channel::{unbounded, Receiver, Sender};
use async_std::task;
use clap::{Arg, ArgMatches};
use cyclors::*;
use futures::prelude::*;
use futures::select;
use git_version::git_version;
use log::{debug, info};
use regex::Regex;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use zenoh::net::runtime::Runtime;
use zenoh::net::utils::resource_name;
use zenoh::net::*;
use zenoh::{GetRequest, Path, PathExpr, Value, Zenoh};

mod qos;
use qos::*;
mod dds_mgt;
use dds_mgt::*;

pub const GIT_VERSION: &str = git_version!(prefix = "v", cargo_prefix = "v");

lazy_static::lazy_static!(
    pub static ref LONG_VERSION: String = format!("{} built with {}", GIT_VERSION, env!("RUSTC_VERSION"));
);

// #[no_mangle]
// pub fn get_expected_args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
//     get_expected_args2()
// }

// NOTE: temporary hack for static link of DDS plugin in zenoh-bridge-dds, thus it can call this function
// instead of relying on #[no_mangle] functions that will conflicts with those defined in REST plugin.
// TODO: remove once eclipse-zenoh/zenoh#89 is implemented
pub fn get_expected_args2<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::from_usage(
            "--dds-scope=[String]   'A string used as prefix to scope DDS traffic.'"
        ),
        Arg::from_usage(
            "--dds-generalise-pub=[String]...   'A list of key expression to use for generalising publications (usable multiple times).'"
        ),
        Arg::from_usage(
            "--dds-generalise-sub=[String]...   'A list of key expression to use for generalising subscriptions (usable multiple times).'"
        ),
        Arg::from_usage(
            "--dds-domain=[ID]   'The DDS Domain ID (if using with ROS this should be the same as ROS_DOMAIN_ID).'"
        ),
        Arg::from_usage(
            "--dds-allow=[String]   'A regular expression matching the set of 'partition/topic-name' that should be bridged. \
            By default, all partitions and topic are allowed. \
            Examples of expressions: '.*/TopicA', 'Partition-?/.*', 'cmd_vel|rosout'...'"
        )
    ]
}

// #[no_mangle]
// pub fn start(runtime: Runtime, args: &'static ArgMatches<'_>) {
//     async_std::task::spawn(run(runtime, args.clone()));
// }

pub async fn run(runtime: Runtime, args: ArgMatches<'_>) {
    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    let _ = env_logger::try_init();
    debug!("DDS plugin {}", LONG_VERSION.as_str());

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

    // open zenoh-net Session (with local routing disabled to avoid loops)
    let zsession =
        Arc::new(Session::init(runtime, false, join_subscriptions, join_publications).await);

    // create DDS Participant
    let dp = unsafe { dds_create_participant(domain_id, std::ptr::null(), std::ptr::null()) };

    let mut dds_plugin = DdsPlugin {
        scope,
        domain_id,
        allow_re,
        zsession,
        dp,
        dds_writer: HashMap::<String, DdsEntity>::new(),
        dds_reader: HashMap::<String, DdsEntity>::new(),
        routes_from_dds: HashMap::<String, Route>::new(),
        routes_to_dds: HashMap::<String, Route>::new(),
        admin_space: HashMap::<String, AdminRef>::new(),
    };

    dds_plugin.run().await;
}

// An reference used in admin space to point to a struct (DdsEntity or Route) stored in another map
enum AdminRef {
    DdsWriterEntity(String),
    DdsReaderEntity(String),
    FromDdsRoute(String),
    ToDdsRoute(String),
    Config,
    Version,
}

// a route from or to DDS
#[derive(Debug, Serialize)]
struct Route {
    // the local DDS entity created to match the discovered user's DDS entites
    #[serde(skip)]
    matching_entity: dds_entity_t,
    // the list of discovered user's DDS entities keys that are routed by this route
    routed_entities: Vec<String>,
}

struct DdsPlugin {
    scope: String,
    domain_id: u32,
    allow_re: Option<Regex>,
    zsession: Arc<Session>,
    dp: dds_entity_t,
    // maps of all discovered DDS entities (indexed by DDS key)
    dds_writer: HashMap<String, DdsEntity>,
    dds_reader: HashMap<String, DdsEntity>,
    // maps of established routes from/to DDS (indexed by zenoh resource key)
    routes_from_dds: HashMap<String, Route>,
    routes_to_dds: HashMap<String, Route>,
    // admin space: index is the admin_path (relative to admin_prefix)
    // value is the JSon string to return to queries.
    admin_space: HashMap<String, AdminRef>,
}

impl Serialize for DdsPlugin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // return the plugin's config as a JSON struct
        let mut s = serializer.serialize_struct("dds", 3)?;
        s.serialize_field("domain_id", &self.domain_id)?;
        s.serialize_field("scope", &self.scope)?;
        s.serialize_field(
            "allow",
            &self
                .allow_re
                .as_ref()
                .map_or_else(|| "**".to_string(), |re| re.to_string()),
        )?;
        s.end()
    }
}

lazy_static::lazy_static! {
    static ref JSON_NULL_STR: String = serde_json::to_string(&serde_json::json!(null)).unwrap();
}

impl DdsPlugin {
    fn is_allowed(&self, zkey: &str) -> bool {
        match &self.allow_re {
            Some(re) => re.is_match(zkey),
            _ => true,
        }
    }

    fn insert_dds_writer(&mut self, e: DdsEntity) {
        // insert reference in admin_space
        let path = format!(
            "participant/{}/writer/{}/{}",
            e.participant_key, e.key, e.topic_name
        );
        self.admin_space
            .insert(path, AdminRef::DdsWriterEntity(e.key.clone()));

        // insert DdsEntity in dds_writer map
        self.dds_writer.insert(e.key.clone(), e);
    }

    fn remove_dds_writer(&mut self, key: &str) {
        // remove from dds_writer map
        if let Some(e) = self.dds_writer.remove(key) {
            // remove from admin space
            let path = format!(
                "participant/{}/writer/{}/{}",
                e.participant_key, e.key, e.topic_name
            );
            self.admin_space.remove(&path);

            // TODO: check is matching routes are unused and remove them
        }
    }

    fn insert_dds_reader(&mut self, e: DdsEntity) {
        // insert reference in admin_space
        let path = format!(
            "participant/{}/reader/{}/{}",
            e.participant_key, e.key, e.topic_name
        );
        self.admin_space
            .insert(path, AdminRef::DdsReaderEntity(e.key.clone()));

        // insert DdsEntity in dds_reader map
        self.dds_reader.insert(e.key.clone(), e);
    }

    fn remove_dds_reader(&mut self, key: &str) {
        // remove from dds_reader map
        if let Some(e) = self.dds_reader.remove(key) {
            // remove from admin space
            let path = format!(
                "participant/{}/reader/{}/{}",
                e.participant_key, e.key, e.topic_name
            );
            self.admin_space.remove(&path);

            // TODO: check is matching routes are unused and remove them
        }
    }

    fn insert_route_from_dds(&mut self, zkey: &str, r: Route) {
        // insert reference in admin_space
        let path = format!("route/from_dds/{}", zkey);
        self.admin_space
            .insert(path, AdminRef::FromDdsRoute(zkey.to_string()));

        // insert route in routes_from_dds map
        self.routes_from_dds.insert(zkey.to_string(), r);
    }

    fn insert_route_to_dds(&mut self, zkey: &str, r: Route) {
        // insert reference in admin_space
        let path = format!("route/to_dds/{}", zkey);
        self.admin_space
            .insert(path, AdminRef::ToDdsRoute(zkey.to_string()));

        // insert route in routes_from_dds map
        self.routes_to_dds.insert(zkey.to_string(), r);
    }

    async fn try_add_route_from_dds(
        &mut self,
        zkey: String,
        pub_to_match: &DdsEntity,
    ) -> RouteStatus {
        if !self.is_allowed(&zkey) {
            info!(
                "Ignoring Publication for resource {} as it is not allowed (see --allow option)",
                zkey
            );
            return RouteStatus::NotAllowed;
        }

        if let Some(route) = self.routes_from_dds.get_mut(&zkey) {
            // TODO: check if there is no QoS conflict with existing route
            debug!(
                "Route from DDS to resource {} already exists -- ignoring",
                zkey
            );
            route.routed_entities.push(pub_to_match.key.clone());
            return RouteStatus::Routed(zkey);
        }

        // declare the zenoh resource and the publisher
        let rkey = ResKey::RName(zkey.clone());
        let nrid = self.zsession.declare_resource(&rkey).await.unwrap();
        let rid = ResKey::RId(nrid);
        let _ = self.zsession.declare_publisher(&rid).await;

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
            self.zsession.clone(),
        );

        self.insert_route_from_dds(
            &zkey,
            Route {
                matching_entity: dr,
                routed_entities: vec![pub_to_match.key.clone()],
            },
        );
        RouteStatus::Routed(zkey)
    }

    async fn try_add_route_to_dds(
        &mut self,
        zkey: String,
        sub_to_match: &DdsEntity,
    ) -> RouteStatus {
        if let Some(re) = &self.allow_re {
            if !re.is_match(&zkey) {
                info!(
                        "Ignoring Subscription for resource {} as it is not allowed (see --allow option)",
                        zkey
                    );
                return RouteStatus::NotAllowed;
            }
        }

        if let Some(route) = self.routes_to_dds.get_mut(&zkey) {
            // TODO: check if there is no type or QoS conflict with existing route
            debug!(
                "Route from resource {} to DDS already exists -- ignoring",
                zkey
            );
            route.routed_entities.push(sub_to_match.key.clone());
            return RouteStatus::Routed(zkey);
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
                && max_blocking_time < DDS_INFINITE_TIME
            {
                // Add 1 nanosecond to max_blocking_time for the Publisher
                max_blocking_time += 1;
                dds_qset_reliability(qos, kind, max_blocking_time);
            }
            qos
        };

        let dw = create_forwarding_dds_writer(
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

        let zn = self.zsession.clone();
        let rkey = ResKey::RName(zkey.to_string());
        let ton = sub_to_match.topic_name.clone();
        let tyn = sub_to_match.type_name.clone();
        let keyless = sub_to_match.keyless;
        let dp = self.dp;
        task::spawn(async move {
            let mut sub = zn.declare_subscriber(&rkey, &sub_info).await.unwrap();
            while let Some(d) = sub.receiver().next().await {
                log::trace!("Route data to DDS '{}'", &ton);
                unsafe {
                    let bs = d.payload.to_vec();
                    // As per the Vec documentation (see https://doc.rust-lang.org/std/vec/struct.Vec.html#method.into_raw_parts)
                    // the only way to correctly releasing it is to create a vec using from_raw_parts
                    // and then have its destructor do the cleanup.
                    // Thus, while tempting to just pass the raw pointer to cyclone and then free it from C,
                    // that is not necessarily safe or guaranteed to be leak free.
                    // TODO replace when stable https://github.com/rust-lang/rust/issues/65816
                    let (ptr, len, capacity) = vec_into_raw_parts(bs);
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
                    dds_writecdr(dw, fwdp as *mut ddsi_serdata);
                    drop(Vec::from_raw_parts(ptr, len, capacity));
                    cdds_sertopic_unref(st);
                };
            }
        });

        self.insert_route_to_dds(
            &zkey,
            Route {
                matching_entity: dw,
                routed_entities: vec![sub_to_match.key.clone()],
            },
        );
        RouteStatus::Routed(zkey)
    }

    fn get_admin_value(&self, admin_ref: &AdminRef) -> Option<Value> {
        match admin_ref {
            AdminRef::DdsReaderEntity(key) => self
                .dds_reader
                .get(key)
                .map(|e| Value::Json(serde_json::to_string(e).unwrap())),
            AdminRef::DdsWriterEntity(key) => self
                .dds_writer
                .get(key)
                .map(|e| Value::Json(serde_json::to_string(e).unwrap())),
            AdminRef::FromDdsRoute(zkey) => self
                .routes_from_dds
                .get(zkey)
                .map(|e| Value::Json(serde_json::to_string(e).unwrap())),
            AdminRef::ToDdsRoute(zkey) => self
                .routes_to_dds
                .get(zkey)
                .map(|e| Value::Json(serde_json::to_string(e).unwrap())),
            AdminRef::Config => Some(Value::Json(serde_json::to_string(self).unwrap())),
            AdminRef::Version => Some(Value::Json(format!(r#""{}""#, LONG_VERSION.as_str()))),
        }
    }

    async fn treat_admin_query(&self, get_request: GetRequest, admin_path_prefix: &str) {
        debug!("Query on admin space: {:?}", get_request.selector);

        // get the list of sub-path expressions that will match the same stored keys than
        // the selector, if those keys had the path_prefix.
        let path_exprs =
            Self::get_sub_path_exprs(get_request.selector.path_expr.as_str(), admin_path_prefix);

        // Get all matching keys/values
        let mut kvs: Vec<(&str, Value)> = Vec::with_capacity(path_exprs.len());
        for path_expr in path_exprs {
            if path_expr.contains('*') {
                // iterate over all admin space to find matching keys
                for (path, admin_ref) in self.admin_space.iter() {
                    if resource_name::intersect(path_expr, path) {
                        if let Some(v) = self.get_admin_value(admin_ref) {
                            kvs.push((path, v));
                        }
                    }
                }
            } else {
                // path_expr correspond to 1 key - just get it.
                if let Some(admin_ref) = self.admin_space.get(path_expr) {
                    if let Some(v) = self.get_admin_value(admin_ref) {
                        kvs.push((path_expr, v));
                    }
                }
            }
        }

        // send replies
        for (path, v) in kvs.drain(..) {
            let admin_path = Path::try_from(format!("{}/{}", admin_path_prefix, path)).unwrap();
            // support the case of empty fragment in Selector (e.g.: "/@/**?[]"), returning 'null' value in such case
            let value = match &get_request.selector.fragment {
                Some(f) if f.is_empty() => Value::Json((*JSON_NULL_STR).clone()),
                _ => v,
            };
            get_request.reply(admin_path, value);
        }
    }

    pub fn get_sub_path_exprs<'a>(path_expr: &'a str, prefix: &str) -> Vec<&'a str> {
        if let Some(remaining) = path_expr.strip_prefix(prefix) {
            vec![remaining]
        } else {
            let mut result = vec![];
            for (i, c) in path_expr.char_indices().rev() {
                if c == '/' || i == path_expr.len() - 1 {
                    let sub_part = &path_expr[..i + 1];
                    if resource_name::intersect(sub_part, prefix) {
                        // if sub_part ends with "**" or "**/", keep those in remaining part
                        let remaining = if sub_part.ends_with("**/") {
                            &path_expr[i - 2..]
                        } else if sub_part.ends_with("**") {
                            &path_expr[i - 1..]
                        } else {
                            &path_expr[i + 1..]
                        };
                        // if remaining is "**" return only this since it covers all
                        if remaining == "**" {
                            return vec!["**"];
                        }
                        result.push(remaining);
                    }
                }
            }
            result
        }
    }
    async fn run(&mut self) {
        // run DDS discovery
        let (tx, rx): (Sender<DiscoveryEvent>, Receiver<DiscoveryEvent>) = unbounded();
        run_discovery(self.dp, tx);

        // declare admin space queryable
        let admin_path_prefix = format!("/@/service/{}/dds", self.zsession.id().await);
        let admin_path_expr = format!("{}/**", admin_path_prefix);
        let z = Zenoh::from(&*self.zsession);
        let w = z.workspace(None).await.unwrap();
        debug!("Declare admin space on {}", admin_path_expr);
        let mut admin_space = w
            .register_eval(&PathExpr::try_from(admin_path_expr.clone()).unwrap())
            .await
            .unwrap();

        // add plugin's config and version in admin space
        self.admin_space
            .insert("config".to_string(), AdminRef::Config);
        self.admin_space
            .insert("version".to_string(), AdminRef::Version);

        loop {
            select!(
                evt = rx.recv().fuse() => {
                    match evt.unwrap() {
                        DiscoveryEvent::DiscoveredPublication {
                            mut entity
                        } => {
                            if entity.partitions.is_empty() {
                                let zkey = format!("{}/{}", self.scope, entity.topic_name);
                                let route_status = self.try_add_route_from_dds(zkey, &entity).await;
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.partitions {
                                    let zkey = format!("{}/{}/{}", self.scope, p, entity.topic_name);
                                    let route_status = self.try_add_route_from_dds(zkey, &entity).await;
                                    entity.routes.insert(p.clone(), route_status);
                                }
                            }
                            self.insert_dds_writer(entity);
                        }
                        DiscoveryEvent::UndiscoveredPublication {
                            key,
                        } => {
                            self.remove_dds_writer(&key);
                        }
                        DiscoveryEvent::DiscoveredSubscription {
                            mut entity
                        } => {
                            if entity.partitions.is_empty() {
                                let zkey = format!("{}/{}", self.scope, entity.topic_name);
                                let route_status = self.try_add_route_to_dds(zkey, &entity).await;
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.partitions {
                                    let zkey = format!("{}/{}/{}", self.scope, p, entity.topic_name);
                                    let route_status = self.try_add_route_to_dds(zkey, &entity).await;
                                    entity.routes.insert(p.clone(), route_status);
                                }
                            }
                            self.insert_dds_reader(entity);
                        }
                        DiscoveryEvent::UndiscoveredSubscription {
                            key,
                        } => {
                            self.remove_dds_reader(&key)
                        }
                    }
                },

                get_request = admin_space.next().fuse() => {
                    self.treat_admin_query(get_request.unwrap(), &admin_path_prefix).await;
                }
            )
        }
    }
}

//TODO replace when stable https://github.com/rust-lang/rust/issues/65816
#[inline]
pub(crate) fn vec_into_raw_parts<T>(v: Vec<T>) -> (*mut T, usize, usize) {
    let mut me = ManuallyDrop::new(v);
    (me.as_mut_ptr(), me.len(), me.capacity())
}
