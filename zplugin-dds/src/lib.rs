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
use log::{debug, info, warn};
use regex::Regex;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use zenoh::net::runtime::Runtime;
use zenoh::net::utils::resource_name;
use zenoh::net::*;
use zenoh::{GetRequest, Path, PathExpr, Value, Zenoh};
use zenoh_ext::net::group::{Group, GroupEvent, JoinEvent, Member};
use zenoh_ext::net::{
    PublicationCache, QueryingSubscriber, SessionExt, PUBLICATION_CACHE_QUERYABLE_KIND,
};
use zenoh_plugin_trait::{prelude::*, PluginId};

mod qos;
use qos::*;
mod dds_mgt;
use dds_mgt::*;

const GROUP_NAME: &str = "zenoh-plugin-dds";
const GROUP_DEFAULT_LEASE: &str = "3";
lazy_static::lazy_static!(
    static ref DDS_DOMAIN_DEFAULT_STR: String = DDS_DOMAIN_DEFAULT.to_string();
);
const PUB_CACHE_QUERY_PREFIX: &str = "/zenoh_dds_plugin/pub_cache";

pub struct DDSPlugin;

impl Plugin for DDSPlugin {
    type Requirements = Vec<Arg<'static, 'static>>;

    type StartArgs = (Runtime, ArgMatches<'static>);

    fn compatibility() -> zenoh_plugin_trait::PluginId {
        PluginId {
            uid: "zenoh-dds-plugin",
        }
    }

    fn get_requirements() -> Self::Requirements {
        get_expected_args()
    }

    fn start(
        (runtime, args): &Self::StartArgs,
    ) -> Result<Box<dyn std::any::Any + Send + Sync>, Box<dyn std::error::Error>> {
        async_std::task::spawn(run(runtime.clone(), args.to_owned()));
        Ok(Box::new(()))
    }
}

zenoh_plugin_trait::declare_plugin!(DDSPlugin);

// NOTE: temporary hack for static link of DDS plugin in zenoh-bridge-dds, thus it can call this function
// instead of relying on #[no_mangle] functions that will conflicts with those defined in REST plugin.
// TODO: remove once eclipse-zenoh/zenoh#89 is implemented
pub fn get_expected_args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
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
        ).default_value(&*DDS_DOMAIN_DEFAULT_STR),
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
    ]
}

pub async fn run(runtime: Runtime, args: ArgMatches<'_>) {
    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    let _ = env_logger::try_init();

    let scope = args.value_of("dds-scope").unwrap().to_string();

    let domain_id_str = args.value_of("dds-domain").unwrap();
    let domain_id = match domain_id_str.parse::<u32>() {
        Ok(adid) => adid,
        Err(_) => panic!("ERROR: {} is not a valid domain ID ", domain_id_str),
    };

    let group_lease_str = args.value_of("dds-group-lease").unwrap();
    let group_lease = match group_lease_str.parse::<u64>() {
        Ok(lease) => Duration::from_secs(lease),
        Err(_) => panic!(
            "ERROR: {} is not a valid lease duration in seconds ",
            group_lease_str
        ),
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

    // create group member
    let zid = zsession.id().await;
    let member_id = args.value_of("dds-group-member-id").unwrap_or(&zid);
    let member = Member::new(member_id).lease(group_lease);

    // create DDS Participant
    let dp = unsafe { dds_create_participant(domain_id, std::ptr::null(), std::ptr::null()) };

    let dds_plugin = DdsPlugin {
        scope,
        domain_id,
        allow_re,
        zsession: &zsession,
        member,
        dp,
        dds_writer: HashMap::<String, DdsEntity>::new(),
        dds_reader: HashMap::<String, DdsEntity>::new(),
        routes_from_dds: HashMap::<String, FromDDSRoute>::new(),
        routes_to_dds: HashMap::<String, ToDDSRoute>::new(),
        admin_space: HashMap::<String, AdminRef>::new(),
    };

    dds_plugin.run().await;
}

// An reference used in admin space to point to a struct (DdsEntiry or Route) stored in another map
enum AdminRef {
    DdsWriterEntity(String),
    DdsReaderEntity(String),
    FromDdsRoute(String),
    ToDdsRoute(String),
    Config,
}

enum ZPublisher<'a> {
    Publisher(Publisher<'a>),
    PublicationCache(PublicationCache<'a>),
}

// a route from DDS
#[derive(Serialize)]
struct FromDDSRoute<'a> {
    // the local DDS Reader created to match the discovered user's DDS Writers
    #[serde(skip)]
    _dds_reader: dds_entity_t,
    // the zenoh publisher used to re-publish to zenoh the data received by the DDS Reader
    #[serde(skip)]
    _zenoh_publisher: ZPublisher<'a>,
    // the list of discovered user's DDS writers keys that are routed by this route
    routed_writers: Vec<String>,
}

enum ZSubscriber<'a> {
    Subscriber(Subscriber<'a>),
    QueryingSubscriber(QueryingSubscriber<'a>),
}

// a route to DDS
#[derive(Serialize)]
struct ToDDSRoute<'a> {
    // the local DDS Writer created to match the discovered user's DDS Readers
    #[serde(skip)]
    _dds_writer: dds_entity_t,
    // the zenoh subscriber receiving data to be re-published by the DDS Writer
    #[serde(skip)]
    zenoh_subscriber: ZSubscriber<'a>,
    // the list of discovered user's DDS readers keys that are routed by this route
    routed_readers: Vec<String>,
}

struct DdsPlugin<'a> {
    scope: String,
    domain_id: u32,
    allow_re: Option<Regex>,
    // Note: &'a Arc<Session> here to keep the ownership of Session outside this struct
    // and be able to store the publishers/subscribers it creates in this same struct.
    zsession: &'a Arc<Session>,
    member: Member,
    dp: dds_entity_t,
    // maps of all discovered DDS entities (indexed by DDS key)
    dds_writer: HashMap<String, DdsEntity>,
    dds_reader: HashMap<String, DdsEntity>,
    // maps of established routes from/to DDS (indexed by zenoh resource key)
    routes_from_dds: HashMap<String, FromDDSRoute<'a>>,
    routes_to_dds: HashMap<String, ToDDSRoute<'a>>,
    // admin space: index is the admin_path (relative to admin_prefix)
    // value is the JSon string to return to queries.
    admin_space: HashMap<String, AdminRef>,
}

impl Serialize for DdsPlugin<'_> {
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

impl<'a> DdsPlugin<'a> {
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

            // TODO: check if matching routes are unused and remove them
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

            // TODO: check if matching routes are unused and remove them
        }
    }

    fn insert_route_from_dds(&mut self, zkey: &str, r: FromDDSRoute<'a>) {
        // insert reference in admin_space
        let path = format!("route/from_dds/{}", zkey);
        self.admin_space
            .insert(path, AdminRef::FromDdsRoute(zkey.to_string()));

        // insert route in routes_from_dds map
        self.routes_from_dds.insert(zkey.to_string(), r);
    }

    fn insert_route_to_dds(&mut self, zkey: &str, r: ToDDSRoute<'a>) {
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
            route.routed_writers.push(pub_to_match.key.clone());
            return RouteStatus::Routed(zkey);
        }

        // declare the zenoh resource and the publisher
        let rkey = ResKey::RName(zkey.clone());
        let nrid = self.zsession.declare_resource(&rkey).await.unwrap();
        let rid = ResKey::RId(nrid);
        let zenoh_publisher: ZPublisher<'a> = if pub_to_match.qos.is_transient_local() {
            let history = pub_to_match.qos.history_length();
            debug!(
                "Caching publications for TRANSIENT_LOCAL Writer on resource {} with history {}",
                zkey, history
            );
            ZPublisher::PublicationCache(
                self.zsession
                    .declare_publication_cache(&rid)
                    .history(history)
                    .queryable_prefix(format!("{}/{}", PUB_CACHE_QUERY_PREFIX, self.member.id()))
                    .await
                    .unwrap(),
            )
        } else {
            ZPublisher::Publisher(self.zsession.declare_publisher(&rid).await.unwrap())
        };

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
            FromDDSRoute {
                _dds_reader: dr,
                _zenoh_publisher: zenoh_publisher,
                routed_writers: vec![pub_to_match.key.clone()],
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
            route.routed_readers.push(sub_to_match.key.clone());
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
        let rkey = ResKey::RName(zkey.to_string());
        let sub_info = SubInfo {
            reliability: Reliability::Reliable,
            mode: SubMode::Push,
            period: None,
        };
        let (zenoh_subscriber, mut receiver): (_, Pin<Box<dyn Stream<Item = Sample> + Send>>) =
            if sub_to_match.qos.is_transient_local() {
                debug!(
                    "Querying historical data for TRANSIENT_LOCAL Reader on resource {}",
                    zkey
                );
                let mut sub = self
                    .zsession
                    .declare_querying_subscriber(&rkey)
                    .query_reskey(format!("{}/*{}", PUB_CACHE_QUERY_PREFIX, zkey).into())
                    .wait()
                    .unwrap();
                let receiver = sub.receiver().clone();
                (ZSubscriber::QueryingSubscriber(sub), Box::pin(receiver))
            } else {
                let mut sub = self
                    .zsession
                    .declare_subscriber(&rkey, &sub_info)
                    .wait()
                    .unwrap();
                let receiver = sub.receiver().clone();
                (ZSubscriber::Subscriber(sub), Box::pin(receiver))
            };

        let ton = sub_to_match.topic_name.clone();
        let tyn = sub_to_match.type_name.clone();
        let keyless = sub_to_match.keyless;
        let dp = self.dp;
        task::spawn(async move {
            while let Some(d) = receiver.next().await {
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
            ToDDSRoute {
                _dds_writer: dw,
                zenoh_subscriber,
                routed_readers: vec![sub_to_match.key.clone()],
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

    pub fn get_sub_path_exprs<'s>(path_expr: &'s str, prefix: &str) -> Vec<&'s str> {
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

    async fn run(mut self) {
        // join DDS plugins group
        let group = Group::join(self.zsession.clone(), GROUP_NAME, self.member.clone()).await;
        let group_subscriber = group.subscribe().await;
        let mut group_stream = group_subscriber.stream();

        // run DDS discovery
        let (tx, rx): (Sender<DiscoveryEvent>, Receiver<DiscoveryEvent>) = unbounded();
        run_discovery(self.dp, tx);

        // declare admin space queryable
        let admin_path_prefix = format!("/@/service/{}/dds", self.zsession.id().await);
        let admin_path_expr = format!("{}/**", admin_path_prefix);
        let z = Zenoh::from(self.zsession.as_ref());
        let w = z.workspace(None).await.unwrap();
        debug!("Declare admin space on {}", admin_path_expr);
        let mut admin_space = w
            .register_eval(&PathExpr::try_from(admin_path_expr.clone()).unwrap())
            .await
            .unwrap();

        // add plugin's config in admin space
        self.admin_space
            .insert("config".to_string(), AdminRef::Config);

        let scope = self.scope.clone();
        loop {
            select!(
                evt = rx.recv().fuse() => {
                    match evt.unwrap() {
                        DiscoveryEvent::DiscoveredPublication {
                            mut entity
                        } => {
                            if entity.partitions.is_empty() {
                                let zkey = format!("{}/{}", scope, entity.topic_name);
                                let route_status = self.try_add_route_from_dds(zkey, &entity).await;
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.partitions {
                                    let zkey = format!("{}/{}/{}", scope, p, entity.topic_name);
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
                                let zkey = format!("{}/{}", scope, entity.topic_name);
                                let route_status = self.try_add_route_to_dds(zkey, &entity).await;
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.partitions {
                                    let zkey = format!("{}/{}/{}", scope, p, entity.topic_name);
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

                group_event = group_stream.next().fuse() => {
                    if let Some(GroupEvent::Join(JoinEvent{member})) = group_event {
                        debug!("New zenoh_dds_plugin detected: {}", member.id());
                        // make all QueryingSubscriber to query this new member
                        for (zkey, zsub) in &mut self.routes_to_dds {
                            if let ZSubscriber::QueryingSubscriber(sub) = &mut zsub.zenoh_subscriber {
                                let rkey: ResKey = format!("{}/{}/{}", PUB_CACHE_QUERY_PREFIX, member.id(), zkey).into();
                                debug!("Query for TRANSIENT_LOCAL topic on: {}", rkey);
                                let target = QueryTarget {
                                    kind: PUBLICATION_CACHE_QUERYABLE_KIND,
                                    target: Target::All,
                                };
                                if let Err(e) = sub.query_on(&rkey, "", target, QueryConsolidation::none()).await {
                                    warn!("Query on {} for TRANSIENT_LOCAL topic failed: {}", rkey, e);
                                }
                            }
                        }
                    }
                }

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
