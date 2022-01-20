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
use async_trait::async_trait;
use cyclors::*;
use flume::r#async::RecvStream;
use futures::prelude::*;
use futures::select;
use git_version::git_version;
use log::{debug, error, info, trace, warn};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use zenoh::net::runtime::Runtime;
use zenoh::plugins::{Plugin, RunningPluginTrait, ZenohPlugin};
use zenoh::publication::CongestionControl;
use zenoh::query::{QueryConsolidation, QueryTarget, Target};
use zenoh::queryable::{Query, Queryable};
use zenoh::subscriber::Subscriber;
use zenoh::utils::key_expr;
use zenoh::Result as ZResult;
use zenoh::{prelude::*, Session};
use zenoh_ext::group::{Group, GroupEvent, JoinEvent, LeaseExpiredEvent, LeaveEvent, Member};
use zenoh_ext::{PublicationCache, QueryingSubscriber, SessionExt};
use zenoh_util::collections::{Timed, TimedEvent, Timer};
use zenoh_util::{bail, zerror};

pub mod config;
mod dds_mgt;
mod qos;
mod ros_discovery;
use config::Config;
use dds_mgt::*;
use qos::*;

use crate::ros_discovery::{
    NodeEntitiesInfo, ParticipantEntitiesInfo, RosDiscoveryInfoMgr, ROS_DISCOVERY_INFO_TOPIC_NAME,
};

pub const GIT_VERSION: &str = git_version!(prefix = "v", cargo_prefix = "v");

lazy_static::lazy_static!(
    pub static ref LONG_VERSION: String = format!("{} built with {}", GIT_VERSION, env!("RUSTC_VERSION"));
    static ref LOG_PAYLOAD: bool = std::env::var("Z_LOG_PAYLOAD").is_ok();
);

const GROUP_NAME: &str = "zenoh-plugin-dds";
const PUB_CACHE_QUERY_PREFIX: &str = "/@dds_pub_cache";

const ROS_DISCOVERY_INFO_POLL_INTERVAL_MS: u64 = 500;

zenoh_plugin_trait::declare_plugin!(DDSPlugin);

#[allow(clippy::upper_case_acronyms)]
pub struct DDSPlugin;

impl ZenohPlugin for DDSPlugin {}
impl Plugin for DDSPlugin {
    type StartArgs = Runtime;
    type RunningPlugin = zenoh::plugins::RunningPlugin;

    const STATIC_NAME: &'static str = "zenoh-plugin-dds";

    fn start(name: &str, runtime: &Self::StartArgs) -> ZResult<zenoh::plugins::RunningPlugin> {
        // Try to initiate login.
        // Required in case of dynamic lib, otherwise no logs.
        // But cannot be done twice in case of static link.
        let _ = env_logger::try_init();

        let runtime_conf = runtime.config.lock();
        let plugin_conf = runtime_conf
            .plugin(name)
            .ok_or_else(|| zerror!("Plugin `{}`: missing config", name))?;
        let config: Config = serde_json::from_value(plugin_conf.clone())
            .map_err(|e| zerror!("Plugin `{}` configuration error: {}", name, e))?;
        async_std::task::spawn(run(runtime.clone(), config));
        Ok(Box::new(DDSPlugin))
    }
}
impl RunningPluginTrait for DDSPlugin {
    fn config_checker(&self) -> zenoh::plugins::ValidationFunction {
        Arc::new(|_, _, _| bail!("DDSPlugin does not support hot configuration changes."))
    }

    fn adminspace_getter<'a>(
        &'a self,
        selector: &'a Selector<'a>,
        plugin_status_key: &str,
    ) -> ZResult<Vec<zenoh::plugins::Response>> {
        let mut responses = Vec::new();
        let version_key = [plugin_status_key, "/__version__"].concat();
        if zenoh::utils::key_expr::intersect(selector.key_selector.as_str(), &version_key) {
            responses.push(zenoh::plugins::Response {
                key: version_key,
                value: GIT_VERSION.into(),
            })
        }
        Ok(responses)
    }
}

pub async fn run(runtime: Runtime, config: Config) {
    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    let _ = env_logger::try_init();
    debug!("DDS plugin {}", LONG_VERSION.as_str());
    debug!("DDS plugin {:?}", config);

    let group_member_id = match config.group_member_id {
        Some(ref id) => id.clone(),
        None => runtime.get_pid_str(),
    };

    // open zenoh-net Session (with local routing disabled to avoid loops)
    let zsession = Arc::new(
        Session::init(
            runtime,
            false,
            config.generalise_subs.clone(),
            config.generalise_pubs.clone(),
        )
        .await,
    );

    // create group member
    let member = Member::new(&group_member_id).lease(config.group_lease);

    // create DDS Participant
    let dp = unsafe { dds_create_participant(config.domain, std::ptr::null(), std::ptr::null()) };

    let mut dds_plugin = DdsPluginRuntime {
        config,
        zsession: &zsession,
        member,
        dp,
        discovered_writers: HashMap::<String, DdsEntity>::new(),
        discovered_readers: HashMap::<String, DdsEntity>::new(),
        routes_from_dds: HashMap::<String, FromDdsRoute>::new(),
        routes_to_dds: HashMap::<String, ToDdsRoute>::new(),
        admin_space: HashMap::<String, AdminRef>::new(),
    };

    dds_plugin.run().await;
}

// An reference used in admin space to point to a struct (DdsEntity or Route) stored in another map
#[derive(Debug)]
enum AdminRef {
    DdsWriterEntity(String),
    DdsReaderEntity(String),
    FromDdsRoute(String),
    ToDdsRoute(String),
    Config,
    Version,
}

enum ZPublisher<'a> {
    Publisher(ExprId),
    PublicationCache(PublicationCache<'a>),
}

// a route from DDS
#[allow(clippy::upper_case_acronyms)]
#[derive(Serialize)]
struct FromDdsRoute<'a> {
    // the local DDS Reader created to serve the route (i.e. re-publish to zenoh data coming from DDS)
    #[serde(serialize_with = "serialize_entity_guid")]
    serving_reader: dds_entity_t,
    // the zenoh publisher used to re-publish to zenoh the data received by the DDS Reader
    #[serde(skip)]
    _zenoh_publisher: ZPublisher<'a>,
    // the list of remote writers served by this route (admin paths)
    remote_routed_readers: Vec<String>,
    // the list of local readers served by this route (entity keys)
    local_routed_writers: Vec<String>,
}

impl Drop for FromDdsRoute<'_> {
    fn drop(&mut self) {
        if let Err(e) = delete_dds_entity(self.serving_reader) {
            warn!("Error dropping a route from DDS: {}", e);
        }
    }
}

enum ZSubscriber<'a> {
    Subscriber(Subscriber<'a>),
    QueryingSubscriber(QueryingSubscriber<'a>),
}

// a route to DDS
#[allow(clippy::upper_case_acronyms)]
#[derive(Serialize)]
struct ToDdsRoute<'a> {
    // the local DDS Writer created to serve the route (i.e. re-publish to DDS data coming from zenoh)
    #[serde(serialize_with = "serialize_entity_guid")]
    serving_writer: dds_entity_t,
    // the zenoh subscriber receiving data to be re-published by the DDS Writer
    #[serde(skip)]
    zenoh_subscriber: ZSubscriber<'a>,
    // the list of remote writers served by this route (admin paths)
    remote_routed_writers: Vec<String>,
    // the list of local readers served by this route (entity keys)
    local_routed_readers: Vec<String>,
}

impl Drop for ToDdsRoute<'_> {
    fn drop(&mut self) {
        if let Err(e) = delete_dds_entity(self.serving_writer) {
            warn!("Error dropping a route from DDS: {}", e);
        }
    }
}

fn serialize_entity_guid<S>(entity: &dds_entity_t, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match get_guid(entity) {
        Ok(guid) => s.serialize_str(&guid),
        Err(_) => s.serialize_str("UNKOWN_GUID"),
    }
}

struct DdsPluginRuntime<'a> {
    config: Config,
    // Note: &'a Arc<Session> here to keep the ownership of Session outside this struct
    // and be able to store the publishers/subscribers it creates in this same struct.
    zsession: &'a Arc<Session>,
    member: Member,
    dp: dds_entity_t,
    // maps of all discovered DDS entities (indexed by DDS key)
    discovered_writers: HashMap<String, DdsEntity>,
    discovered_readers: HashMap<String, DdsEntity>,
    // maps of established routes from/to DDS (indexed by zenoh resource key)
    routes_from_dds: HashMap<String, FromDdsRoute<'a>>,
    routes_to_dds: HashMap<String, ToDdsRoute<'a>>,
    // admin space: index is the admin_path (relative to admin_prefix)
    // value is the JSon string to return to queries.
    admin_space: HashMap<String, AdminRef>,
}

impl Serialize for DdsPluginRuntime<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // return the plugin's config as a JSON struct
        let mut s = serializer.serialize_struct("dds", 3)?;
        s.serialize_field("domain", &self.config.domain)?;
        s.serialize_field("scope", &self.config.scope)?;
        s.serialize_field(
            "allow",
            &self
                .config
                .allow
                .as_ref()
                .map_or_else(|| ".*".to_string(), |re| re.to_string()),
        )?;
        s.serialize_field(
            "deny",
            &self
                .config
                .deny
                .as_ref()
                .map_or_else(|| "".to_string(), |re| re.to_string()),
        )?;
        s.serialize_field(
            "max-frequencies",
            &self
                .config
                .max_frequencies
                .iter()
                .map(|(re, freq)| format!("{}={}", re, freq))
                .collect::<Vec<String>>(),
        )?;
        s.serialize_field("forward_discovery", &self.config.forward_discovery)?;
        s.serialize_field(
            "reliable_routes_blocking",
            &self.config.reliable_routes_blocking,
        )?;
        s.end()
    }
}

lazy_static::lazy_static! {
    static ref JSON_NULL_VALUE: Value = serde_json::json!(null);
}

impl<'a> DdsPluginRuntime<'a> {
    fn is_allowed(&self, zkey: &str) -> bool {
        if self.config.forward_discovery && zkey.ends_with(ROS_DISCOVERY_INFO_TOPIC_NAME) {
            // If fwd-discovery mode is enabled, don't route /ros_discovery_info
            return false;
        }
        match (&self.config.allow, &self.config.deny) {
            (Some(allow), None) => allow.is_match(zkey),
            (None, Some(deny)) => !deny.is_match(zkey),
            (Some(allow), Some(deny)) => allow.is_match(zkey) && !deny.is_match(zkey),
            (None, None) => true,
        }
    }

    // Return the read period if zkey matches one of the --dds-periodic-topics option
    fn get_read_period(&self, zkey: &str) -> Option<Duration> {
        for (re, freq) in &self.config.max_frequencies {
            if re.is_match(zkey) {
                return Some(Duration::from_secs_f32(1f32 / freq));
            }
        }
        None
    }

    fn get_admin_path(e: &DdsEntity, is_writer: bool) -> String {
        if is_writer {
            format!(
                "participant/{}/writer/{}/{}",
                e.participant_key, e.key, e.topic_name
            )
        } else {
            format!(
                "participant/{}/reader/{}/{}",
                e.participant_key, e.key, e.topic_name
            )
        }
    }

    fn insert_dds_writer(&mut self, admin_path: String, e: DdsEntity) {
        // insert reference in admin_space
        self.admin_space
            .insert(admin_path, AdminRef::DdsWriterEntity(e.key.clone()));

        // insert DdsEntity in dds_writer map
        self.discovered_writers.insert(e.key.clone(), e);
    }

    fn remove_dds_writer(&mut self, key: &str) -> Option<(String, DdsEntity)> {
        // remove from dds_writer map
        if let Some(e) = self.discovered_writers.remove(key) {
            // remove from admin_space
            let admin_path = DdsPluginRuntime::get_admin_path(&e, true);
            self.admin_space.remove(&admin_path);
            Some((admin_path, e))
        } else {
            None
        }
    }

    fn insert_dds_reader(&mut self, admin_path: String, e: DdsEntity) {
        // insert reference in admin_space
        self.admin_space
            .insert(admin_path, AdminRef::DdsReaderEntity(e.key.clone()));

        // insert DdsEntity in dds_reader map
        self.discovered_readers.insert(e.key.clone(), e);
    }

    fn remove_dds_reader(&mut self, key: &str) -> Option<(String, DdsEntity)> {
        // remove from dds_reader map
        if let Some(e) = self.discovered_readers.remove(key) {
            // remove from admin space
            let admin_path = DdsPluginRuntime::get_admin_path(&e, false);
            self.admin_space.remove(&admin_path);
            Some((admin_path, e))
        } else {
            None
        }
    }

    fn insert_route_from_dds(&mut self, zkey: &str, r: FromDdsRoute<'a>) {
        // insert reference in admin_space
        let path = format!("route/from_dds/{}", zkey);
        self.admin_space
            .insert(path, AdminRef::FromDdsRoute(zkey.to_string()));

        // insert route in routes_from_dds map
        self.routes_from_dds.insert(zkey.to_string(), r);
    }

    fn insert_route_to_dds(&mut self, zkey: &str, r: ToDdsRoute<'a>) {
        // insert reference in admin_space
        let path = format!("route/to_dds/{}", zkey);
        self.admin_space
            .insert(path, AdminRef::ToDdsRoute(zkey.to_string()));

        // insert route in routes_from_dds map
        self.routes_to_dds.insert(zkey.to_string(), r);
    }

    async fn try_add_route_from_dds(
        &mut self,
        zkey: &str,
        topic_name: &str,
        topic_type: &str,
        keyless: bool,
        reader_qos: Qos,
        congestion_ctrl: CongestionControl,
    ) -> RouteStatus {
        if !self.is_allowed(zkey) {
            info!(
                "Ignoring Publication for resource {} as it is not allowed (see your 'allow' or 'deny' configuration)",
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
            return RouteStatus::Routed(zkey.to_string());
        }

        // declare the zenoh resource and the publisher
        let rid = self.zsession.declare_expr(zkey).await.unwrap();
        let zenoh_publisher: ZPublisher<'a> = if reader_qos.durability.kind
            == DurabilityKind::TRANSIENT_LOCAL
        {
            #[allow(non_upper_case_globals)]
            let history = match (reader_qos.history.kind, reader_qos.history.depth) {
                (HistoryKind::KEEP_LAST, n) => {
                    // Compute cache size as history.depth * durability_service.max_instances
                    // This makes the assumption that the frequency of publication is the same for all instances...
                    // But as we have no way to know have 1 cache per-instance, there is no other choice.
                    if reader_qos.durability_service.max_instances == DDS_LENGTH_UNLIMITED && n > 0
                    {
                        usize::MAX
                    } else if reader_qos.durability_service.max_instances > 0 {
                        if let Some(m) = n.checked_mul(reader_qos.durability_service.max_instances)
                        {
                            m as usize
                        } else {
                            usize::MAX
                        }
                    } else {
                        n as usize
                    }
                }
                (HistoryKind::KEEP_ALL, _) => usize::MAX,
            };
            debug!(
                "Caching publications for TRANSIENT_LOCAL Writer on resource {} with history {}",
                zkey, history
            );
            ZPublisher::PublicationCache(
                self.zsession
                    .publication_cache(rid)
                    .history(history)
                    .queryable_prefix(format!("{}/{}", PUB_CACHE_QUERY_PREFIX, self.member.id()))
                    .await
                    .unwrap(),
            )
        } else {
            self.zsession.declare_publication(rid).await.unwrap();
            ZPublisher::Publisher(rid)
        };

        let read_period = self.get_read_period(zkey);

        // create matching DDS Writer that forwards data coming from zenoh
        match create_forwarding_dds_reader(
            self.dp,
            topic_name.to_string(),
            topic_type.to_string(),
            keyless,
            reader_qos,
            rid.into(),
            self.zsession.clone(),
            read_period,
            congestion_ctrl,
        ) {
            Ok(dr) => {
                info!(
                    "New route: DDS '{}' => zenoh '{}' with type '{}'",
                    topic_name, zkey, topic_type
                );

                self.insert_route_from_dds(
                    zkey,
                    FromDdsRoute {
                        serving_reader: dr,
                        _zenoh_publisher: zenoh_publisher,
                        remote_routed_readers: vec![],
                        local_routed_writers: vec![],
                    },
                );
                RouteStatus::Routed(zkey.to_string())
            }
            Err(e) => {
                error!(
                    "Failed to create route DDS '{}' => zenoh '{}: {}",
                    topic_name, zkey, e
                );
                RouteStatus::CreationFailure(e)
            }
        }
    }

    async fn try_add_route_to_dds(
        &mut self,
        zkey: &str,
        topic_name: &str,
        topic_type: &str,
        keyless: bool,
        writer_qos: Qos,
    ) -> RouteStatus {
        if !self.is_allowed(zkey) {
            info!(
                "Ignoring Subscription for resource {} as it is not allowed (see your 'allow' or 'deny' configuration)",
                zkey
            );
            return RouteStatus::NotAllowed;
        }

        if self.routes_to_dds.contains_key(zkey) {
            // TODO: check if there is no type or QoS conflict with existing route
            debug!(
                "Route from resource {} to DDS already exists -- ignoring",
                zkey
            );
            return RouteStatus::Routed(zkey.to_string());
        }

        // create matching DDS Writer that forwards data coming from zenoh
        let is_transient_local = writer_qos.durability.kind == DurabilityKind::TRANSIENT_LOCAL;
        match create_forwarding_dds_writer(
            self.dp,
            topic_name.to_string(),
            topic_type.to_string(),
            keyless,
            writer_qos,
        ) {
            Ok(dw) => {
                info!(
                    "New route: zenoh '{}' => DDS '{}' with type '{}'",
                    zkey, topic_name, topic_type
                );

                // create zenoh subscriber
                let (zenoh_subscriber, mut receiver): (
                    _,
                    Pin<Box<dyn Stream<Item = Sample> + Send>>,
                ) = if is_transient_local {
                    debug!(
                        "Querying historical data for TRANSIENT_LOCAL Reader on resource {}",
                        zkey
                    );
                    let mut sub = self
                        .zsession
                        .subscribe_with_query(zkey)
                        .reliable()
                        .query_selector(&format!("{}/*{}", PUB_CACHE_QUERY_PREFIX, zkey))
                        .wait()
                        .unwrap();
                    let receiver = sub.receiver().clone();
                    (ZSubscriber::QueryingSubscriber(sub), Box::pin(receiver))
                } else {
                    let mut sub = self.zsession.subscribe(zkey).reliable().wait().unwrap();
                    let receiver = sub.receiver().clone();
                    (ZSubscriber::Subscriber(sub), Box::pin(receiver))
                };

                let ton = topic_name.to_string();
                let tyn = topic_type.to_string();
                let keyless = keyless;
                let dp = self.dp;
                task::spawn(async move {
                    while let Some(s) = receiver.next().await {
                        if *LOG_PAYLOAD {
                            log::trace!(
                                "Route data from zenoh {} to DDS '{}' - payload: {:?}",
                                s.key_expr,
                                &ton,
                                s.value.payload
                            );
                        } else {
                            log::trace!("Route data from zenoh {} to DDS '{}'", s.key_expr, &ton);
                        }
                        unsafe {
                            let bs = s.value.payload.to_vec();
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
                            let fwdp = cdds_ddsi_payload_create(
                                st,
                                ddsi_serdata_kind_SDK_DATA,
                                ptr,
                                len.try_into().unwrap(),
                            );
                            dds_writecdr(dw, fwdp as *mut ddsi_serdata);
                            drop(Vec::from_raw_parts(ptr, len, capacity));
                            cdds_sertopic_unref(st);
                        };
                    }
                });

                self.insert_route_to_dds(
                    zkey,
                    ToDdsRoute {
                        serving_writer: dw,
                        zenoh_subscriber,
                        remote_routed_writers: vec![],
                        local_routed_readers: vec![],
                    },
                );
                RouteStatus::Routed(zkey.to_string())
            }
            Err(e) => {
                error!(
                    "Failed to create route zenoh '{}' => DDS '{}' : {}",
                    zkey, topic_name, e
                );
                RouteStatus::CreationFailure(e)
            }
        }
    }

    fn get_admin_value(&self, admin_ref: &AdminRef) -> Option<Value> {
        match admin_ref {
            AdminRef::DdsReaderEntity(key) => self
                .discovered_readers
                .get(key)
                .map(|e| serde_json::to_value(e).unwrap()),
            AdminRef::DdsWriterEntity(key) => self
                .discovered_writers
                .get(key)
                .map(|e| serde_json::to_value(e).unwrap()),
            AdminRef::FromDdsRoute(zkey) => self
                .routes_from_dds
                .get(zkey)
                .map(|e| serde_json::to_value(e).unwrap()),
            AdminRef::ToDdsRoute(zkey) => self
                .routes_to_dds
                .get(zkey)
                .map(|e| serde_json::to_value(e).unwrap()),
            AdminRef::Config => Some(serde_json::to_value(self).unwrap()),
            AdminRef::Version => Some(Value::String(LONG_VERSION.clone())),
        }
    }

    async fn treat_admin_query(&self, query: Query, admin_path_prefix: &str) {
        let selector = query.selector();
        debug!("Query on admin space: {:?}", selector);

        // get the list of sub-path expressions that will match the same stored keys than
        // the selector, if those keys had the path_prefix.
        let sub_selectors =
            Self::get_sub_key_selectors(selector.key_selector.as_str(), admin_path_prefix);

        // Get all matching keys/values
        let mut kvs: Vec<(&str, Value)> = Vec::with_capacity(sub_selectors.len());
        for path_expr in sub_selectors {
            if path_expr.contains('*') {
                // iterate over all admin space to find matching keys
                for (path, admin_ref) in self.admin_space.iter() {
                    if key_expr::intersect(path_expr, path) {
                        if let Some(v) = self.get_admin_value(admin_ref) {
                            kvs.push((path, v));
                        } else {
                            warn!("INTERNAL ERROR: Dangling {:?}", admin_ref);
                        }
                    }
                }
            } else {
                // path_expr correspond to 1 key - just get it.
                if let Some(admin_ref) = self.admin_space.get(path_expr) {
                    if let Some(v) = self.get_admin_value(admin_ref) {
                        kvs.push((path_expr, v));
                    } else {
                        warn!("INTERNAL ERROR: Dangling {:?}", admin_ref);
                    }
                }
            }
        }

        // send replies
        for (path, v) in kvs.drain(..) {
            let admin_path = format!("{}{}", admin_path_prefix, path);
            // support the case of empty fragment in Selector (e.g.: "/@/**?[]"), returning 'null' value in such case
            // let value_selector = selector.parse_value_selector()?;
            let value = match selector.parse_value_selector().map(|vs| vs.fragment) {
                Ok(f) if f.is_empty() => (*JSON_NULL_VALUE).clone(),
                _ => v,
            };
            query.reply(Sample::new(admin_path, value));
        }
    }

    pub fn get_sub_key_selectors<'s>(key_selector: &'s str, prefix: &str) -> Vec<&'s str> {
        if let Some(remaining) = key_selector.strip_prefix(prefix) {
            vec![remaining]
        } else {
            let mut result = vec![];
            for (i, c) in key_selector.char_indices().rev() {
                if c == '/' || i == key_selector.len() - 1 {
                    let sub_part = &key_selector[..i + 1];
                    if key_expr::intersect(sub_part, prefix) {
                        // if sub_part ends with "**" or "**/", keep those in remaining part
                        let remaining = if sub_part.ends_with("**/") {
                            &key_selector[i - 2..]
                        } else if sub_part.ends_with("**") {
                            &key_selector[i - 1..]
                        } else {
                            &key_selector[i + 1..]
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
        // join DDS plugins group
        let group = Group::join(self.zsession.clone(), GROUP_NAME, self.member.clone()).await;
        let group_subscriber = group.subscribe().await;
        let mut group_stream = group_subscriber.stream();

        // run DDS discovery
        let (tx, dds_disco_rcv): (Sender<DiscoveryEvent>, Receiver<DiscoveryEvent>) = unbounded();
        run_discovery(self.dp, tx);

        // declare admin space queryable
        let admin_path_prefix = format!("/@/service/{}/dds/", self.zsession.id().await);
        let admin_path_expr = format!("{}**", admin_path_prefix);
        debug!("Declare admin space on {}", admin_path_expr);
        let mut admin_queryable = self
            .zsession
            .queryable(&admin_path_expr)
            .kind(zenoh::queryable::EVAL)
            .await
            .unwrap();

        // add plugin's config and version in admin space
        self.admin_space
            .insert("config".to_string(), AdminRef::Config);
        self.admin_space
            .insert("version".to_string(), AdminRef::Version);

        if self.config.forward_discovery {
            self.run_fwd_discovery_mode(
                &mut group_stream,
                &dds_disco_rcv,
                admin_path_prefix,
                &mut admin_queryable,
            )
            .await;
        } else {
            self.run_local_discovery_mode(
                &mut group_stream,
                &dds_disco_rcv,
                admin_path_prefix,
                &mut admin_queryable,
            )
            .await;
        }
    }

    async fn run_local_discovery_mode(
        &mut self,
        group_stream: &mut RecvStream<'_, GroupEvent>,
        dds_disco_rcv: &Receiver<DiscoveryEvent>,
        admin_path_prefix: String,
        admin_queryable: &mut Queryable<'_>,
    ) {
        debug!(r#"Run in "local discovery" mode"#);

        let scope = self.config.scope.clone();
        let admin_query_rcv = admin_queryable.receiver();
        loop {
            select!(
                evt = dds_disco_rcv.recv().fuse() => {
                    match evt.unwrap() {
                        DiscoveryEvent::DiscoveredPublication {
                            mut entity
                        } => {
                            debug!("Discovered DDS Writer on {}: {}", entity.topic_name, entity.key);
                            // get its admin_path
                            let admin_path = DdsPluginRuntime::get_admin_path(&entity, true);

                            // copy and adapt Writer's QoS for creation of a matching Reader
                            let mut qos = entity.qos.clone();
                            qos.ignore_local_participant = true;

                            // CongestionControl to be used when re-publishing over zenoh: Blocking if Writer is RELIABLE (since we don't know what is remote Reader's QoS)
                            let congestion_ctrl = match (self.config.reliable_routes_blocking, entity.qos.reliability.kind) {
                                (true, ReliabilityKind::RELIABLE) => CongestionControl::Block,
                                _ => CongestionControl::Drop,
                            };

                            // create 1 route per partition, or just 1 if no partition
                            if entity.qos.partitions.is_empty() {
                                let zkey = format!("{}/{}", scope, entity.topic_name);
                                let route_status = self.try_add_route_from_dds(&zkey, &entity.topic_name, &entity.type_name, entity.keyless, qos, congestion_ctrl).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                        // add Writer's key in local_matched_writers list
                                        r.local_routed_writers.push(entity.key.clone());
                                    }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.qos.partitions {
                                    let zkey = format!("{}/{}/{}", scope, p, entity.topic_name);
                                    let route_status = self.try_add_route_from_dds(&zkey, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone(), congestion_ctrl).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                            // if route has been created, add this Writer in its routed_writers list
                                            r.local_routed_writers.push(entity.key.clone());
                                        }
                                    }
                                    entity.routes.insert(p.clone(), route_status);
                                }
                            }

                            // store the writer
                            self.insert_dds_writer(admin_path, entity);
                        }

                        DiscoveryEvent::UndiscoveredPublication {
                            key,
                        } => {
                            debug!("Undiscovered DDS Writer {}", key);
                            if let Some((_, e)) = self.remove_dds_writer(&key) {
                                // remove it from all the active routes refering it (deleting the route if no longer used)
                                let admin_space = &mut self.admin_space;
                                self.routes_from_dds.retain(|zkey, route| {
                                        route.local_routed_writers.retain(|s| s != &key);
                                        if route.local_routed_writers.is_empty() {
                                            info!(
                                                "Remove unused route: DDS '{}' => zenoh '{}'",
                                                e.topic_name, zkey
                                            );
                                            let path = format!("route/from_dds/{}", zkey);
                                            admin_space.remove(&path);
                                            false
                                        } else {
                                            true
                                        }
                                    }
                                );
                            }
                        }

                        DiscoveryEvent::DiscoveredSubscription {
                            mut entity
                        } => {
                            debug!("Discovered DDS Reader on {}: {}", entity.topic_name, entity.key);
                            let admin_path = DdsPluginRuntime::get_admin_path(&entity, false);

                            // copy and adapt Reader's QoS for creation of a matching Writer
                            let mut qos = entity.qos.clone();
                            qos.ignore_local_participant = true;
                            // if Reader is TRANSIENT_LOCAL, configure durability_service QoS with same history than the Reader.
                            // This is because CycloneDDS is actually usinf durability_service.history for transient_local historical data.
                            if qos.durability.kind == DurabilityKind::TRANSIENT_LOCAL {
                                qos.durability_service.service_cleanup_delay = 60 * DDS_1S_DURATION;
                                qos.durability_service.history_kind = qos.history.kind;
                                qos.durability_service.history_depth = qos.history.depth;
                                qos.durability_service.max_samples = DDS_LENGTH_UNLIMITED;
                                qos.durability_service.max_instances = DDS_LENGTH_UNLIMITED;
                                qos.durability_service.max_samples_per_instance = DDS_LENGTH_UNLIMITED;
                            }
                            // Workaround for the DDS Writer to correctly match with a FastRTPS Reader declaring a Reliability max_blocking_time < infinite
                            if qos.reliability.max_blocking_time < DDS_INFINITE_TIME {
                                qos.reliability.max_blocking_time += 1;
                            }

                            // create 1 route per partition, or just 1 if no partition
                            if entity.qos.partitions.is_empty() {
                                let zkey = format!("{}/{}", scope, entity.topic_name);
                                let route_status = self.try_add_route_to_dds(&zkey, &entity.topic_name, &entity.type_name, entity.keyless, qos).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                        // if route has been created, add this Reader in its routed_readers list
                                        r.local_routed_readers.push(entity.key.clone());
                                    }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.qos.partitions {
                                    let zkey = format!("{}/{}/{}", scope, p, entity.topic_name);
                                    let route_status = self.try_add_route_to_dds(&zkey, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone()).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                            // if route has been created, add this Reader in its routed_readers list
                                            r.local_routed_readers.push(entity.key.clone());
                                        }
                                    }
                                    entity.routes.insert(p.clone(), route_status);
                                }
                            }

                            // store the reader
                            self.insert_dds_reader(admin_path, entity);
                        }

                        DiscoveryEvent::UndiscoveredSubscription {
                            key,
                        } => {
                            debug!("Undiscovered DDS Reader {}", key);
                            if let Some((_, e)) = self.remove_dds_reader(&key) {
                                // remove it from all the active routes refering it (deleting the route if no longer used)
                                let admin_space = &mut self.admin_space;
                                self.routes_to_dds.retain(|zkey, route| {
                                        route.local_routed_readers.retain(|s| s != &key);
                                        if route.local_routed_readers.is_empty() {
                                            info!(
                                                "Remove unused route: zenoh '{}' => DDS '{}'",
                                                zkey, e.topic_name
                                            );
                                            let path = format!("route/to_dds/{}", zkey);
                                            admin_space.remove(&path);
                                            false
                                        } else {
                                            true
                                        }
                                    }
                                );
                            }
                        }
                    }
                },

                group_event = group_stream.next().fuse() => {
                    if let Some(GroupEvent::Join(JoinEvent{member})) = group_event {
                        debug!("New zenoh_dds_plugin detected: {}", member.id());
                        // make all QueryingSubscriber to query this new member
                        for (zkey, zsub) in &mut self.routes_to_dds {
                            if let ZSubscriber::QueryingSubscriber(sub) = &mut zsub.zenoh_subscriber {
                                let rkey: KeyExpr = format!("{}/{}/{}", PUB_CACHE_QUERY_PREFIX, member.id(), zkey).into();
                                debug!("Query for TRANSIENT_LOCAL topic on: {}", rkey);
                                let target = QueryTarget {
                                    kind: PublicationCache::QUERYABLE_KIND,
                                    target: Target::All,
                                };
                                if let Err(e) = sub.query_on(Selector::from(&rkey), target, QueryConsolidation::none()).await {
                                    warn!("Query on {} for TRANSIENT_LOCAL topic failed: {}", rkey, e);
                                }
                            }
                        }
                    }
                }

                get_request = admin_query_rcv.next().fuse() => {
                    self.treat_admin_query(get_request.unwrap(), &admin_path_prefix).await;
                }
            )
        }
    }

    async fn run_fwd_discovery_mode(
        &mut self,
        group_stream: &mut RecvStream<'_, GroupEvent>,
        dds_disco_rcv: &Receiver<DiscoveryEvent>,
        admin_path_prefix: String,
        admin_queryable: &mut Queryable<'_>,
    ) {
        debug!(r#"Run in "forward discovery" mode"#);

        // The admin paths where discovery info will be forwarded to remote DDS plugins.
        // Note: "/@dds" is used as prefix instead of "/@/..." to not have the PublicationCache replying to queries on admin space.
        let uuid = self.zsession.id().await;
        let fwd_writers_path_prefix = format!("/@dds/{}/writer/", uuid);
        let fwd_readers_path_prefix = format!("/@dds/{}/reader/", uuid);
        let fwd_ros_discovery_prefix = format!("/@dds/{}/ros_disco/", uuid);

        // Cache the publications on admin space for late joiners DDS plugins
        let _fwd_disco_pub_cache = self
            .zsession
            .publication_cache(format!("/@dds/{}/**", uuid))
            .await
            .unwrap();

        // Subscribe to remote DDS plugins publications of new Readers/Writers on admin space
        let mut fwd_disco_sub = self
            .zsession
            .subscribe_with_query("/@dds/**")
            .await
            .unwrap();

        // Manage ros_discovery_info topic, reading it periodically
        let ros_disco_mgr = RosDiscoveryInfoMgr::create(self.dp).unwrap();
        let timer = Timer::default();
        let (tx, mut ros_disco_timer_rcv): (Sender<()>, Receiver<()>) = unbounded();
        let ros_disco_timer_event = TimedEvent::periodic(
            Duration::from_millis(ROS_DISCOVERY_INFO_POLL_INTERVAL_MS),
            ChannelEvent { tx },
        );
        let _ = timer.add_async(ros_disco_timer_event).await;

        // The ParticipantEntitiesInfo to be re-published on ros_discovery_info (with this bridge's participant gid)
        let mut participant_info = ParticipantEntitiesInfo::new(get_guid(&self.dp).unwrap());

        let scope = self.config.scope.clone();
        let admin_query_rcv = admin_queryable.receiver();
        loop {
            select!(
                evt = dds_disco_rcv.recv().fuse() => {
                    match evt.unwrap() {
                        DiscoveryEvent::DiscoveredPublication {
                            entity
                        } => {
                            debug!("Discovered DDS Writer on {}: {} => advertise it", entity.topic_name, entity.key);
                            // advertise the entity and its scope within admin space (bincode format)
                            let admin_path = DdsPluginRuntime::get_admin_path(&entity, true);
                            let fwd_path = format!("{}{}", fwd_writers_path_prefix, admin_path);
                            let msg = (&entity, &scope);
                            self.zsession.put(&fwd_path, bincode::serialize(&msg).unwrap()).congestion_control(CongestionControl::Block).await.unwrap();

                            // store the writer in admin space
                            self.insert_dds_writer(admin_path, entity);
                        }

                        DiscoveryEvent::UndiscoveredPublication {
                            key,
                        } => {
                            debug!("Undiscovered DDS Writer {} => advertise it", key);
                            if let Some((admin_path, _)) = self.remove_dds_writer(&key) {
                                let fwd_path = format!("{}{}", fwd_writers_path_prefix, admin_path);
                                // publish its deletion from admin space
                                self.zsession.delete(&fwd_path).congestion_control(CongestionControl::Block).await.unwrap();
                            }
                        }

                        DiscoveryEvent::DiscoveredSubscription {
                            entity
                        } => {
                            debug!("Discovered DDS Reader on {}: {} => advertise it", entity.topic_name, entity.key);
                            // advertise the entity and its scope within admin space (bincode format)
                            let admin_path = DdsPluginRuntime::get_admin_path(&entity, false);
                            let fwd_path = format!("{}{}", fwd_readers_path_prefix, admin_path);
                            let msg = (&entity, &scope);
                            self.zsession.put(&fwd_path, bincode::serialize(&msg).unwrap()).congestion_control(CongestionControl::Block).await.unwrap();

                            // store the reader
                            self.insert_dds_reader(admin_path, entity);
                        }

                        DiscoveryEvent::UndiscoveredSubscription {
                            key,
                        } => {
                            debug!("Undiscovered DDS Reader {} => advertise it", key);
                            if let Some((admin_path, _)) = self.remove_dds_reader(&key) {
                                let fwd_path = format!("{}{}", fwd_readers_path_prefix, admin_path);
                                // publish its deletion from admin space
                                self.zsession.delete(&fwd_path).congestion_control(CongestionControl::Block).await.unwrap();
                            }
                        }
                    }
                },

                sample = fwd_disco_sub.receiver().next().fuse() => {
                    let sample = sample.unwrap();
                    let fwd_path = &sample.key_expr;
                    debug!("Received forwarded discovery message on {}", fwd_path);

                    // decode the beginning part of fwd_path segments (0th is empty as res_name starts with '/')
                    let mut split_it = fwd_path.as_str().splitn(5, '/');
                    let remote_uuid = split_it.nth(2).unwrap();
                    let disco_kind = split_it.next().unwrap();
                    let remaining_path = split_it.next().unwrap();

                    match disco_kind {
                        // it's a writer discovery message
                        "writer" => {
                            // reconstruct full admin path for this entity (i.e. with it's remote plugin's uuid)
                            let full_admin_path = format!("/@/service/{}/dds/{}", remote_uuid, remaining_path);
                            if sample.kind != SampleKind::Delete {
                                // deserialize payload
                                let (entity, scope) = bincode::deserialize::<(DdsEntity, String)>(&sample.value.payload.to_vec()).unwrap();
                                // copy and adapt Writer's QoS for creation of a proxy Writer
                                let mut qos = entity.qos.clone();
                                qos.ignore_local_participant = true;

                                // create 1 "to_dds" route per partition, or just 1 if no partition
                                if entity.qos.partitions.is_empty() {
                                    let zkey = format!("{}/{}", scope, entity.topic_name);
                                    let route_status = self.try_add_route_to_dds(&zkey, &entity.topic_name, &entity.type_name, entity.keyless, qos).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                            // add the writer's admin path to the list of remote_routed_writers
                                            r.remote_routed_writers.push(full_admin_path);
                                            // check amongst local Readers is some are matching (only wrt. topic_name and partition. TODO: consider qos match also)
                                            for reader in self.discovered_readers.values_mut() {
                                                if reader.topic_name == entity.topic_name && reader.qos.partitions.is_empty() {
                                                    r.local_routed_readers.push(reader.key.clone());
                                                    reader.routes.insert("*".to_string(), route_status.clone());
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    for p in &entity.qos.partitions {
                                        let zkey = format!("{}/{}/{}", scope, p, entity.topic_name);
                                        let route_status = self.try_add_route_to_dds(&zkey, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone()).await;
                                        if let RouteStatus::Routed(ref route_key) = route_status {
                                            if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                                // add the writer's admin path to the list of remote_routed_writers
                                                r.remote_routed_writers.push(full_admin_path.clone());
                                                // check amongst local Readers is some are matching (only wrt. topic_name and partition. TODO: consider qos match also)
                                                for reader in self.discovered_readers.values_mut() {
                                                    if reader.topic_name == entity.topic_name && reader.qos.partitions.contains(p) {
                                                        r.local_routed_readers.push(reader.key.clone());
                                                        reader.routes.insert(p.clone(), route_status.clone());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                // writer was deleted; remove it from all the active routes refering it (deleting the route if no longer used)
                                let admin_space = &mut self.admin_space;
                                self.routes_to_dds.retain(|zkey, route| {
                                        route.remote_routed_writers.retain(|s| s != &full_admin_path);
                                        if route.remote_routed_writers.is_empty() {
                                            info!(
                                                "Remove unused route: zenoh '{}' => DDS '{}'",
                                                zkey, zkey
                                            );
                                            let path = format!("route/to_dds/{}", zkey);
                                            admin_space.remove(&path);
                                            false
                                        } else {
                                            true
                                        }
                                    }
                                );
                            }
                        }

                        // it's a reader discovery message
                        "reader" => {
                            // reconstruct full admin path for this entity (i.e. with it's remote plugin's uuid)
                            let full_admin_path = format!("/@/service/{}/dds/{}", remote_uuid, remaining_path);
                            if sample.kind != SampleKind::Delete {
                                // deserialize payload
                                let (entity, scope) = bincode::deserialize::<(DdsEntity, String)>(&sample.value.payload.to_vec()).unwrap();
                                // copy and adapt Reader's QoS for creation of a proxy Reader
                                let mut qos = entity.qos.clone();
                                qos.ignore_local_participant = true;

                                // CongestionControl to be used when re-publishing over zenoh: Blocking if Reader is RELIABLE (since Writer will also be, otherwise no matching)
                                let congestion_ctrl = match (self.config.reliable_routes_blocking, entity.qos.reliability.kind) {
                                    (true, ReliabilityKind::RELIABLE) => CongestionControl::Block,
                                    _ => CongestionControl::Drop,
                                };

                                // create 1 'from_dds" route per partition, or just 1 if no partition
                                if entity.qos.partitions.is_empty() {
                                    let zkey = format!("{}/{}", scope, entity.topic_name);
                                    let route_status = self.try_add_route_from_dds(&zkey, &entity.topic_name, &entity.type_name, entity.keyless, qos, congestion_ctrl).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                            // add the reader's admin path to the list of remote_routed_writers
                                            r.remote_routed_readers.push(full_admin_path);
                                            // check amongst local Writers is some are matching (only wrt. topic_name and partition. TODO: consider qos match also)
                                            for writer in self.discovered_writers.values_mut() {
                                                if writer.topic_name == entity.topic_name && writer.qos.partitions.is_empty() {
                                                    r.local_routed_writers.push(writer.key.clone());
                                                    writer.routes.insert("*".to_string(), route_status.clone());
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    for p in &entity.qos.partitions {
                                        let zkey = format!("{}/{}/{}", scope, p, entity.topic_name);
                                        let route_status = self.try_add_route_from_dds(&zkey, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone(), congestion_ctrl).await;
                                        if let RouteStatus::Routed(ref route_key) = route_status {
                                            if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                                // add the reader's admin path to the list of remote_routed_writers
                                                r.remote_routed_readers.push(full_admin_path.clone());
                                                // check amongst local Writers is some are matching (only wrt. topic_name and partition. TODO: consider qos match also)
                                                for writer in self.discovered_writers.values_mut() {
                                                    if writer.topic_name == entity.topic_name && writer.qos.partitions.contains(p) {
                                                        r.local_routed_writers.push(writer.key.clone());
                                                        writer.routes.insert(p.clone(), route_status.clone());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                // reader was deleted; remove it from all the active routes refering it (deleting the route if no longer used)
                                let admin_space = &mut self.admin_space;
                                self.routes_from_dds.retain(|zkey, route| {
                                        route.remote_routed_readers.retain(|s| s != &full_admin_path);
                                        if route.remote_routed_readers.is_empty() {
                                            info!(
                                                "Remove unused route: DDS '{}' => zenoh '{}'",
                                                zkey, zkey
                                            );
                                            let path = format!("route/from_dds/{}", zkey);
                                            admin_space.remove(&path);
                                            false
                                        } else {
                                            true
                                        }
                                    }
                                );
                            }
                        }

                        // it's a ros_discovery_info message
                        "ros_disco" => {
                            match cdr::deserialize_from::<_, ParticipantEntitiesInfo, _>(
                                sample.value.payload,
                                cdr::size::Infinite,
                            ) {
                                Ok(mut info) => {
                                    // remap all original gids with the gids of the routes
                                    self.remap_entities_info(&mut info.node_entities_info_seq);
                                    // update the ParticipantEntitiesInfo for this bridge and re-publish it on DDS
                                    participant_info.update_with(info.node_entities_info_seq);
                                    debug!("Publish updated ros_discovery_info: {:?}", participant_info);
                                    if let Err(e) = ros_disco_mgr.write(&participant_info) {
                                        error!("Error forwarding ros_discovery_info: {}", e);
                                    }
                                }
                                Err(e) => error!(
                                    "Error receiving ParticipantEntitiesInfo on {}: {}",
                                    fwd_path, e
                                ),
                            }
                        }

                        _ => {
                            error!("Unexpected forwarded discovery message received on {}", fwd_path);
                        }
                    }
                },

                group_event = group_stream.next().fuse() => {
                    match group_event {
                        Some(GroupEvent::Join(JoinEvent{member})) => {
                            debug!("New zenoh_dds_plugin detected: {}", member.id());
                            // make all QueryingSubscriber to query this new member
                            for (zkey, zsub) in &mut self.routes_to_dds {
                                if let ZSubscriber::QueryingSubscriber(sub) = &mut zsub.zenoh_subscriber {
                                    let rkey: KeyExpr = format!("{}/{}/{}", PUB_CACHE_QUERY_PREFIX, member.id(), zkey).into();
                                    debug!("Query for TRANSIENT_LOCAL topic on: {}", rkey);
                                    let target = QueryTarget {
                                        kind: PublicationCache::QUERYABLE_KIND,
                                        target: Target::All,
                                    };
                                    if let Err(e) = sub.query_on(Selector::from(&rkey), target, QueryConsolidation::none()).await {
                                        warn!("Query on {} for TRANSIENT_LOCAL topic failed: {}", rkey, e);
                                    }
                                }
                            }
                        }
                        Some(GroupEvent::Leave(LeaveEvent{mid})) | Some(GroupEvent::LeaseExpired(LeaseExpiredEvent{mid})) => {
                            debug!("Remote zenoh_dds_plugin left: {}", mid);
                            // remove all the references to the plugin's enities, removing no longer used routes
                            // and updating/re-publishing ParticipantEntitiesInfo
                            let admin_space = &mut self.admin_space;
                            let admin_subpath = format!("/@/service/{}/dds/", mid);
                            let mut participant_info_changed = false;
                            self.routes_to_dds.retain(|zkey, route| {
                                route.remote_routed_writers.retain(|s| !s.contains(&admin_subpath));
                                if route.remote_routed_writers.is_empty() {
                                    info!(
                                        "Remove unused route: zenoh '{}' => DDS '{}'",
                                        zkey, zkey
                                    );
                                    let path = format!("route/to_dds/{}", zkey);
                                    admin_space.remove(&path);
                                    participant_info.remove_writer_gid(&get_guid(&route.serving_writer).unwrap());
                                    participant_info_changed = true;
                                    false
                                } else {
                                    true
                                }
                            });
                            self.routes_from_dds.retain(|zkey, route| {
                                route.remote_routed_readers.retain(|s| !s.contains(&admin_subpath));
                                if route.remote_routed_readers.is_empty() {
                                    info!(
                                        "Remove unused route: DDS '{}' => zenoh '{}'",
                                        zkey, zkey
                                    );
                                    let path = format!("route/from_dds/{}", zkey);
                                    admin_space.remove(&path);
                                    participant_info.remove_reader_gid(&get_guid(&route.serving_reader).unwrap());
                                    participant_info_changed = true;
                                    false
                                } else {
                                    true
                                }
                            });
                            if participant_info_changed {
                                debug!("Publishing up-to-date tos_discovery_info after leaving of plugin {}", mid);
                                participant_info.cleanup();
                                if let Err(e) = ros_disco_mgr.write(&participant_info) {
                                    error!("Error forwarding ros_discovery_info: {}", e);
                                }
                            }

                        }

                        _ => {}
                    }
                }

                get_request = admin_query_rcv.next().fuse() => {
                    self.treat_admin_query(get_request.unwrap(), &admin_path_prefix).await;
                }

                _ = ros_disco_timer_rcv.next().fuse() => {
                    let infos = ros_disco_mgr.read();
                    for (gid, buf) in infos {
                        trace!("Received ros_discovery_info from DDS for {}, forward via zenoh: {}", gid, hex::encode(buf.to_vec().as_slice()));
                        // forward the payload on zenoh
                        if let Err(e) = self.zsession.put(&format!("{}{}", fwd_ros_discovery_prefix, gid), buf).await {
                            error!("Forward ROS discovery info failed: {}", e);
                        }
                    }
                }
            )
        }
    }

    fn remap_entities_info(&self, entities_info: &mut HashMap<String, NodeEntitiesInfo>) {
        for node in entities_info.values_mut() {
            // TODO: replace with drain_filter when stable (https://github.com/rust-lang/rust/issues/43244)
            let mut i = 0;
            while i < node.reader_gid_seq.len() {
                // find a FromDdsRoute routing a remote reader with this gid
                match self.routes_from_dds.values().find(|route| {
                    route
                        .remote_routed_readers
                        .iter()
                        .any(|s| s.contains(&node.reader_gid_seq[i]))
                }) {
                    Some(route) => {
                        // replace the gid with route's reader's gid
                        let gid = get_guid(&route.serving_reader).unwrap();
                        trace!(
                            "ros_discovery_info remap reader {} -> {}",
                            node.reader_gid_seq[i],
                            gid
                        );
                        node.reader_gid_seq[i] = gid;
                        i += 1;
                    }
                    None => {
                        // remove the gid (not route found because either not allowed to be routed,
                        // either route already initiated by another reader)
                        trace!(
                            "ros_discovery_info remap reader {} -> NONE",
                            node.reader_gid_seq[i]
                        );
                        node.reader_gid_seq.remove(i);
                    }
                }
            }
            let mut i = 0;
            while i < node.writer_gid_seq.len() {
                // find a ToDdsRoute initiated by the writer with this gid
                match self.routes_to_dds.values().find(|route| {
                    route
                        .remote_routed_writers
                        .iter()
                        .any(|s| s.contains(&node.writer_gid_seq[i]))
                }) {
                    Some(route) => {
                        // replace the gid with route's writer's gid
                        let gid = get_guid(&route.serving_writer).unwrap();
                        trace!(
                            "ros_discovery_info remap writer {} -> {}",
                            node.writer_gid_seq[i],
                            gid
                        );
                        node.writer_gid_seq[i] = gid;
                        i += 1;
                    }
                    None => {
                        // remove the gid (not route found because either not allowed to be routed,
                        // either route already initiated by another writer)
                        trace!(
                            "ros_discovery_info remap writer {} -> NONE",
                            node.writer_gid_seq[i]
                        );
                        node.writer_gid_seq.remove(i);
                    }
                }
            }
        }
    }
}

//TODO replace when stable https://github.com/rust-lang/rust/issues/65816
#[inline]
pub(crate) fn vec_into_raw_parts<T>(v: Vec<T>) -> (*mut T, usize, usize) {
    let mut me = ManuallyDrop::new(v);
    (me.as_mut_ptr(), me.len(), me.capacity())
}

struct ChannelEvent {
    tx: Sender<()>,
}

#[async_trait]
impl Timed for ChannelEvent {
    async fn run(&mut self) {
        if self.tx.send(()).await.is_err() {
            warn!("Error sending periodic timer notification on channel");
        };
    }
}
