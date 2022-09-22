//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use async_trait::async_trait;
use cyclors::*;
use flume::{unbounded, Receiver, Sender};
use futures::select;
use git_version::git_version;
use log::{debug, error, info, trace, warn};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;
use std::convert::TryInto;
use std::env;
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::Duration;
use zenoh::buffers::SplitBuffer;
use zenoh::plugins::{Plugin, RunningPluginTrait, Runtime, ZenohPlugin};
use zenoh::prelude::r#async::AsyncResolve;
use zenoh::prelude::*;
use zenoh::publication::CongestionControl;
use zenoh::query::{ConsolidationMode, QueryTarget};
use zenoh::queryable::{Query, Queryable};
use zenoh::subscriber::Subscriber;
use zenoh::Result as ZResult;
use zenoh::Session;
use zenoh_collections::{Timed, TimedEvent, Timer};
use zenoh_core::{bail, zerror};
use zenoh_ext::group::{Group, GroupEvent, JoinEvent, LeaseExpiredEvent, LeaveEvent, Member};
use zenoh_ext::{PublicationCache, QueryingSubscriber, SessionExt};

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

macro_rules! ke_for_sure {
    ($val:expr) => {
        unsafe { keyexpr::from_str_unchecked($val) }
    };
}

lazy_static::lazy_static!(
    pub static ref LONG_VERSION: String = format!("{} built with {}", GIT_VERSION, env!("RUSTC_VERSION"));
    static ref LOG_PAYLOAD: bool = std::env::var("Z_LOG_PAYLOAD").is_ok();

    static ref KE_PREFIX_ADMIN_SPACE: &'static keyexpr = ke_for_sure!("@/service");
    static ref KE_PREFIX_ROUTE_TO_DDS: &'static keyexpr = ke_for_sure!("route/to_dds");
    static ref KE_PREFIX_ROUTE_FROM_DDS: &'static keyexpr = ke_for_sure!("route/from_dds");
    static ref KE_PREFIX_PUB_CACHE: &'static keyexpr = ke_for_sure!("@dds_pub_cache");
    static ref KE_PREFIX_FWD_DISCO: &'static keyexpr = ke_for_sure!("@dds_fwd_disco");

    static ref KE_ANY_1_SEGMENT: &'static keyexpr = ke_for_sure!("*");
    static ref KE_ANY_N_SEGMENT: &'static keyexpr = ke_for_sure!("**");
);

// CycloneDDS' localhost-only: set network interface address (shortened form of config would be
// possible, too, but I think it is clearer to spell it out completely).
// Empty configuration fragments are ignored, so it is safe to unconditionally append a comma.
const CYCLONEDDS_CONFIG_LOCALHOST_ONLY: &str = r#"<CycloneDDS><Domain><General><NetworkInterfaceAddress>127.0.0.1</NetworkInterfaceAddress></General></Domain></CycloneDDS>,"#;

const GROUP_NAME: &str = "zenoh-plugin-dds";

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
        if selector.key_expr.intersects(ke_for_sure!(&version_key)) {
            responses.push(zenoh::plugins::Response::new(
                version_key,
                GIT_VERSION.into(),
            ));
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

    // open zenoh-net Session
    let zsession = match zenoh::init(runtime)
        .aggregated_subscribers(config.generalise_subs.clone())
        .aggregated_publishers(config.generalise_pubs.clone())
        .res()
        .await
    {
        Ok(session) => Arc::new(session),
        Err(e) => {
            log::error!("Unable to init zenoh session for DDS plugin : {:?}", e);
            return;
        }
    };

    // create group member using the group_member_id if configured, or the Session ID otherwise
    let member_id = match config.group_member_id {
        Some(ref id) => id.clone(),
        None => zsession.zid().into_keyexpr(),
    };
    let member = Member::new(member_id.clone())
        .unwrap()
        .lease(config.group_lease);

    // if "localhost_only" is set, configure CycloneDDS to use only localhost interface
    if config.localhost_only {
        env::set_var(
            "CYCLONEDDS_URI",
            format!(
                "{}{}",
                CYCLONEDDS_CONFIG_LOCALHOST_ONLY,
                env::var("CYCLONEDDS_URI").unwrap_or_default()
            ),
        );
    }

    // create DDS Participant
    debug!(
        "Create DDS Participant with CYCLONEDDS_URI='{}'",
        env::var("CYCLONEDDS_URI").unwrap_or_default()
    );
    let dp = unsafe { dds_create_participant(config.domain, std::ptr::null(), std::ptr::null()) };
    debug!(
        "DDS plugin {} with member_id={} and using DDS Participant {}",
        zsession.zid(),
        member_id,
        get_guid(&dp).unwrap()
    );

    let mut dds_plugin = DdsPluginRuntime {
        config,
        zsession: &zsession,
        member,
        member_id,
        dp,
        discovered_writers: HashMap::<String, DdsEntity>::new(),
        discovered_readers: HashMap::<String, DdsEntity>::new(),
        routes_from_dds: HashMap::<OwnedKeyExpr, FromDdsRoute>::new(),
        routes_to_dds: HashMap::<OwnedKeyExpr, ToDdsRoute>::new(),
        admin_space: HashMap::<OwnedKeyExpr, AdminRef>::new(),
    };

    dds_plugin.run().await;
}

// An reference used in admin space to point to a struct (DdsEntity or Route) stored in another map
#[derive(Debug)]
enum AdminRef {
    DdsWriterEntity(String),
    DdsReaderEntity(String),
    FromDdsRoute(OwnedKeyExpr),
    ToDdsRoute(OwnedKeyExpr),
    Config,
    Version,
}

enum ZPublisher<'a> {
    Publisher(KeyExpr<'a>),
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
    // the list of remote writers served by this route (admin key expr)
    remote_routed_readers: Vec<OwnedKeyExpr>,
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
    Subscriber(Subscriber<'a, ()>),
    QueryingSubscriber(QueryingSubscriber<'a, ()>),
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
    // the list of remote writers served by this route (admin key expr)
    remote_routed_writers: Vec<OwnedKeyExpr>,
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
    member_id: OwnedKeyExpr,
    dp: dds_entity_t,
    // maps of all discovered DDS entities (indexed by DDS key)
    discovered_writers: HashMap<String, DdsEntity>,
    discovered_readers: HashMap<String, DdsEntity>,
    // maps of established routes from/to DDS (indexed by zenoh key expression)
    routes_from_dds: HashMap<OwnedKeyExpr, FromDdsRoute<'a>>,
    routes_to_dds: HashMap<OwnedKeyExpr, ToDdsRoute<'a>>,
    // admin space: index is the admin_keyexpr (relative to admin_prefix)
    // value is the JSon string to return to queries.
    admin_space: HashMap<OwnedKeyExpr, AdminRef>,
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
    fn is_allowed(&self, ke: &keyexpr) -> bool {
        if self.config.forward_discovery && ke.ends_with(ROS_DISCOVERY_INFO_TOPIC_NAME) {
            // If fwd-discovery mode is enabled, don't route "ros_discovery_info"
            return false;
        }
        match (&self.config.allow, &self.config.deny) {
            (Some(allow), None) => allow.is_match(ke),
            (None, Some(deny)) => !deny.is_match(ke),
            (Some(allow), Some(deny)) => allow.is_match(ke) && !deny.is_match(ke),
            (None, None) => true,
        }
    }

    // Return the read period if keyexpr matches one of the --dds-periodic-topics option
    fn get_read_period(&self, ke: &keyexpr) -> Option<Duration> {
        for (re, freq) in &self.config.max_frequencies {
            if re.is_match(ke) {
                return Some(Duration::from_secs_f32(1f32 / freq));
            }
        }
        None
    }

    fn get_admin_keyexpr(e: &DdsEntity, is_writer: bool) -> OwnedKeyExpr {
        format!(
            "participant/{}/{}/{}/{}",
            e.participant_key,
            if is_writer { "writer" } else { "reader" },
            e.key,
            e.topic_name
        )
        .try_into()
        .unwrap()
    }

    fn insert_dds_writer(&mut self, admin_keyexpr: OwnedKeyExpr, e: DdsEntity) {
        // insert reference in admin_space
        self.admin_space
            .insert(admin_keyexpr, AdminRef::DdsWriterEntity(e.key.clone()));

        // insert DdsEntity in dds_writer map
        self.discovered_writers.insert(e.key.clone(), e);
    }

    fn remove_dds_writer(&mut self, dds_key: &str) -> Option<(OwnedKeyExpr, DdsEntity)> {
        // remove from dds_writer map
        if let Some(e) = self.discovered_writers.remove(dds_key) {
            // remove from admin_space
            let admin_keyexpr = DdsPluginRuntime::get_admin_keyexpr(&e, true);
            self.admin_space.remove(&admin_keyexpr);
            Some((admin_keyexpr, e))
        } else {
            None
        }
    }

    fn insert_dds_reader(&mut self, admin_keyexpr: OwnedKeyExpr, e: DdsEntity) {
        // insert reference in admin_space
        self.admin_space
            .insert(admin_keyexpr, AdminRef::DdsReaderEntity(e.key.clone()));

        // insert DdsEntity in dds_reader map
        self.discovered_readers.insert(e.key.clone(), e);
    }

    fn remove_dds_reader(&mut self, dds_key: &str) -> Option<(OwnedKeyExpr, DdsEntity)> {
        // remove from dds_reader map
        if let Some(e) = self.discovered_readers.remove(dds_key) {
            // remove from admin space
            let admin_keyexpr = DdsPluginRuntime::get_admin_keyexpr(&e, false);
            self.admin_space.remove(&admin_keyexpr);
            Some((admin_keyexpr, e))
        } else {
            None
        }
    }

    fn insert_route_from_dds(&mut self, ke: OwnedKeyExpr, r: FromDdsRoute<'a>) {
        // insert reference in admin_space
        let admin_ke = *KE_PREFIX_ROUTE_FROM_DDS / &ke;
        self.admin_space
            .insert(admin_ke, AdminRef::FromDdsRoute(ke.clone()));

        // insert route in routes_from_dds map
        self.routes_from_dds.insert(ke, r);
    }

    fn insert_route_to_dds(&mut self, ke: OwnedKeyExpr, r: ToDdsRoute<'a>) {
        // insert reference in admin_space
        let admin_ke: OwnedKeyExpr = *KE_PREFIX_ROUTE_TO_DDS / &ke;
        self.admin_space
            .insert(admin_ke, AdminRef::ToDdsRoute(ke.clone()));

        // insert route in routes_from_dds map
        self.routes_to_dds.insert(ke, r);
    }

    async fn try_add_route_from_dds(
        &mut self,
        ke: OwnedKeyExpr,
        topic_name: &str,
        topic_type: &str,
        keyless: bool,
        reader_qos: Qos,
        congestion_ctrl: CongestionControl,
    ) -> RouteStatus {
        if !self.is_allowed(&ke) {
            info!(
                "Ignoring Publication for resource {} as it is not allowed (see your 'allow' or 'deny' configuration)",
                ke
            );
            return RouteStatus::NotAllowed;
        }

        if self.routes_from_dds.contains_key(&ke) {
            // TODO: check if there is no QoS conflict with existing route
            debug!(
                "Route from DDS to resource {} already exists -- ignoring",
                ke
            );
            return RouteStatus::Routed(ke);
        }

        // declare the zenoh resource and the publisher
        let declared_ke = match self.zsession.declare_keyexpr(ke.clone()).res().await {
            Ok(k) => k,
            Err(e) => {
                return RouteStatus::CreationFailure(format!(
                    "Failed to declare expression {}: {}",
                    keyless, e
                ))
            }
        };
        let zenoh_publisher: ZPublisher<'a> = if reader_qos.durability.kind
            == DurabilityKind::TRANSIENT_LOCAL
        {
            #[allow(non_upper_case_globals)]
            let history = match (reader_qos.history.kind, reader_qos.history.depth) {
                (HistoryKind::KEEP_LAST, n) => {
                    if keyless {
                        // only 1 instance => history=n
                        n as usize
                    } else if reader_qos.durability_service.max_instances == DDS_LENGTH_UNLIMITED {
                        // No limit! => history=MAX
                        usize::MAX
                    } else if reader_qos.durability_service.max_instances > 0 {
                        // Compute cache size as history.depth * durability_service.max_instances
                        // This makes the assumption that the frequency of publication is the same for all instances...
                        // But as we have no way to have 1 cache per-instance, there is no other choice.
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
                "Caching publications for TRANSIENT_LOCAL Writer on resource {} with history {} (Writer uses {:?} and DurabilityService.max_instances={})",
                ke, history, reader_qos.history, reader_qos.durability_service.max_instances
            );
            match self
                .zsession
                .declare_publication_cache(&declared_ke)
                .history(history)
                .queryable_prefix(*KE_PREFIX_PUB_CACHE / &self.member_id)
                .queryable_allowed_origin(Locality::Remote) // Note: don't reply to queries from local QueryingSubscribers
                .res()
                .await
            {
                Ok(pub_cache) => ZPublisher::PublicationCache(pub_cache),
                Err(e) => {
                    return RouteStatus::CreationFailure(format!(
                        "Failed create PublicationCache for key {} (rid={}): {}",
                        ke, declared_ke, e
                    ))
                }
            }
        } else {
            if let Err(e) = self
                .zsession
                .declare_publisher(declared_ke.clone())
                .res()
                .await
            {
                warn!(
                    "Failed to declare publisher for key {} (rid={}): {}",
                    ke, declared_ke, e
                );
            }
            ZPublisher::Publisher(declared_ke.clone())
        };

        let read_period = self.get_read_period(&ke);

        // create matching DDS Writer that forwards data coming from zenoh
        match create_forwarding_dds_reader(
            self.dp,
            topic_name.to_string(),
            topic_type.to_string(),
            keyless,
            reader_qos,
            declared_ke,
            self.zsession.clone(),
            read_period,
            congestion_ctrl,
        ) {
            Ok(dr) => {
                info!(
                    "New route: DDS Reader {} on '{}' => zenoh '{}' with type '{}'",
                    get_guid(&dr).unwrap_or_else(|e| format!("[ERROR: {}]", e)),
                    topic_name,
                    ke,
                    topic_type
                );

                self.insert_route_from_dds(
                    ke.clone(),
                    FromDdsRoute {
                        serving_reader: dr,
                        _zenoh_publisher: zenoh_publisher,
                        remote_routed_readers: vec![],
                        local_routed_writers: vec![],
                    },
                );
                RouteStatus::Routed(ke)
            }
            Err(e) => {
                error!(
                    "Failed to create route DDS '{}' => zenoh '{}: {}",
                    topic_name, ke, e
                );
                RouteStatus::CreationFailure(e)
            }
        }
    }

    async fn try_add_route_to_dds(
        &mut self,
        ke: OwnedKeyExpr,
        topic_name: &str,
        topic_type: &str,
        keyless: bool,
        writer_qos: Qos,
    ) -> RouteStatus {
        if !self.is_allowed(&ke) {
            info!(
                "Ignoring Subscription for resource {} as it is not allowed (see your 'allow' or 'deny' configuration)",
                ke
            );
            return RouteStatus::NotAllowed;
        }

        if self.routes_to_dds.contains_key(&ke) {
            // TODO: check if there is no type or QoS conflict with existing route
            debug!(
                "Route from resource {} to DDS already exists -- ignoring",
                ke
            );
            return RouteStatus::Routed(ke);
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
                    "New route: zenoh '{}' => DDS Writer {} on '{}' with type '{}'",
                    ke,
                    get_guid(&dw).unwrap_or_else(|e| format!("[ERROR: {}]", e)),
                    topic_name,
                    topic_type
                );

                let ton = topic_name.to_string();
                let tyn = topic_type.to_string();
                let keyless = keyless;
                let dp = self.dp;
                let subscriber_callback = move |s: Sample| {
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
                        let bs = s.value.payload.contiguous().into_owned();
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
                        let size: size_t = match len.try_into() {
                            Ok(s) => s,
                            Err(_) => {
                                warn!("Can't route data from zenoh {} to DDS '{}': excessive payload size ({})", s.key_expr, &ton, len);
                                return;
                            }
                        };
                        let fwdp =
                            cdds_ddsi_payload_create(st, ddsi_serdata_kind_SDK_DATA, ptr, size);
                        dds_writecdr(dw, fwdp as *mut ddsi_serdata);
                        drop(Vec::from_raw_parts(ptr, len, capacity));
                        cdds_sertopic_unref(st);
                    };
                };

                // create zenoh subscriber
                let zenoh_subscriber = if is_transient_local {
                    debug!(
                        "Query historical data from everybody for TRANSIENT_LOCAL Reader on resource {}",
                        ke
                    );
                    // query all PublicationCaches on "<KE_PREFIX_PUB_CACHE>/*/<routing_keyexpr>"
                    let query_selector: Selector =
                        (*KE_PREFIX_PUB_CACHE / *KE_ANY_1_SEGMENT / &ke).into();
                    let sub = match self
                        .zsession
                        .declare_querying_subscriber(ke.clone())
                        .callback(subscriber_callback)
                        .allowed_origin(Locality::Remote) // Allow only remote publications to avoid loops
                        .reliable()
                        .query_selector(query_selector)
                        .res()
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            return RouteStatus::CreationFailure(format!(
                                "Failed create QueryingSubscriber for key {}: {}",
                                ke, e
                            ))
                        }
                    };
                    ZSubscriber::QueryingSubscriber(sub)
                } else {
                    let sub = match self
                        .zsession
                        .declare_subscriber(ke.clone())
                        .callback(subscriber_callback)
                        .allowed_origin(Locality::Remote) // Allow only remote publications to avoid loops
                        .reliable()
                        .res()
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            return RouteStatus::CreationFailure(format!(
                                "Failed create Subscriber for key {}: {}",
                                ke, e
                            ))
                        }
                    };
                    ZSubscriber::Subscriber(sub)
                };

                self.insert_route_to_dds(
                    ke.clone(),
                    ToDdsRoute {
                        serving_writer: dw,
                        zenoh_subscriber,
                        remote_routed_writers: vec![],
                        local_routed_readers: vec![],
                    },
                );
                RouteStatus::Routed(ke)
            }
            Err(e) => {
                error!(
                    "Failed to create route zenoh '{}' => DDS '{}' : {}",
                    ke, topic_name, e
                );
                RouteStatus::CreationFailure(e)
            }
        }
    }

    fn get_admin_value(&self, admin_ref: &AdminRef) -> Result<Option<Value>, serde_json::Error> {
        match admin_ref {
            AdminRef::DdsReaderEntity(key) => self
                .discovered_readers
                .get(key)
                .map(serde_json::to_value)
                .transpose(),
            AdminRef::DdsWriterEntity(key) => self
                .discovered_writers
                .get(key)
                .map(serde_json::to_value)
                .transpose(),
            AdminRef::FromDdsRoute(zkey) => self
                .routes_from_dds
                .get(zkey)
                .map(serde_json::to_value)
                .transpose(),
            AdminRef::ToDdsRoute(zkey) => self
                .routes_to_dds
                .get(zkey)
                .map(serde_json::to_value)
                .transpose(),
            AdminRef::Config => Some(serde_json::to_value(self)).transpose(),
            AdminRef::Version => Ok(Some(Value::String(LONG_VERSION.clone()))),
        }
    }

    async fn treat_admin_query(&self, query: Query, admin_keyexpr_prefix: &keyexpr) {
        let selector = query.selector();
        debug!("Query on admin space: {:?}", selector);

        // get the list of sub-key expressions that will match the same stored keys than
        // the selector, if those keys had the admin_keyexpr_prefix.
        let sub_kes = selector.key_expr.strip_prefix(admin_keyexpr_prefix);
        if sub_kes.is_empty() {
            error!("Received query for admin space: '{}' - but it's not prefixed by admin_keyexpr_prefix='{}'", selector, admin_keyexpr_prefix);
            return;
        }

        // Get all matching keys/values
        let mut kvs: Vec<(KeyExpr, Value)> = Vec::with_capacity(sub_kes.len());
        for sub_ke in sub_kes {
            if sub_ke.contains('*') {
                // iterate over all admin space to find matching keys
                for (ke, admin_ref) in self.admin_space.iter() {
                    if sub_ke.intersects(ke) {
                        match self.get_admin_value(admin_ref) {
                            Ok(Some(v)) => kvs.push((ke.into(), v)),
                            Ok(None) => error!("INTERNAL ERROR: Dangling {:?}", admin_ref),
                            Err(e) => {
                                error!("INTERNAL ERROR serializing admin value as JSON: {}", e)
                            }
                        }
                    }
                }
            } else {
                // sub_ke correspond to 1 key - just get it.
                if let Some(admin_ref) = self.admin_space.get(sub_ke) {
                    match self.get_admin_value(admin_ref) {
                        Ok(Some(v)) => kvs.push((sub_ke.into(), v)),
                        Ok(None) => error!("INTERNAL ERROR: Dangling {:?}", admin_ref),
                        Err(e) => {
                            error!("INTERNAL ERROR serializing admin value as JSON: {}", e)
                        }
                    }
                }
            }
        }

        // send replies
        for (ke, v) in kvs.drain(..) {
            let admin_keyexpr = admin_keyexpr_prefix / &ke;
            if let Err(e) = query.reply(Ok(Sample::new(admin_keyexpr, v))).res().await {
                warn!("Error replying to admin query {:?}: {}", query, e);
            }
        }
    }

    async fn run(&mut self) {
        // join DDS plugins group
        let group = Group::join(self.zsession.clone(), GROUP_NAME, self.member.clone())
            .await
            .unwrap();
        let group_subscriber = group.subscribe().await;

        // run DDS discovery
        let (tx, dds_disco_rcv): (Sender<DiscoveryEvent>, Receiver<DiscoveryEvent>) = unbounded();
        run_discovery(self.dp, tx);

        // declare admin space queryable
        let admin_keyexpr_prefix =
            *KE_PREFIX_ADMIN_SPACE / &self.zsession.zid().into_keyexpr() / ke_for_sure!("dds");
        let admin_keyexpr_expr = (&admin_keyexpr_prefix) / *KE_ANY_N_SEGMENT;
        debug!("Declare admin space on {}", admin_keyexpr_expr);
        let admin_queryable = self
            .zsession
            .declare_queryable(admin_keyexpr_expr)
            .res()
            .await
            .expect("Failed to create AdminSpace queryable");

        // add plugin's config and version in admin space
        self.admin_space
            .insert("config".try_into().unwrap(), AdminRef::Config);
        self.admin_space
            .insert("version".try_into().unwrap(), AdminRef::Version);

        if self.config.forward_discovery {
            self.run_fwd_discovery_mode(
                &group_subscriber,
                &dds_disco_rcv,
                admin_keyexpr_prefix,
                &admin_queryable,
            )
            .await;
        } else {
            self.run_local_discovery_mode(
                &group_subscriber,
                &dds_disco_rcv,
                admin_keyexpr_prefix,
                &admin_queryable,
            )
            .await;
        }
    }

    fn topic_to_keyexpr(
        &self,
        topic_name: &str,
        scope: &Option<OwnedKeyExpr>,
        partition: Option<&str>,
    ) -> ZResult<OwnedKeyExpr> {
        // key_expr for a topic is: "<scope>/<partition>/<topic_name>" with <scope> and <partition> being optional
        match (scope, partition) {
            (Some(scope), Some(part)) => scope.join(&format!("{}/{}", part, topic_name)),
            (Some(scope), None) => scope.join(topic_name),
            (None, Some(part)) => format!("{}/{}", part, topic_name).try_into(),
            (None, None) => topic_name.try_into(),
        }
    }

    async fn run_local_discovery_mode(
        &mut self,
        group_subscriber: &Receiver<GroupEvent>,
        dds_disco_rcv: &Receiver<DiscoveryEvent>,
        admin_keyexpr_prefix: OwnedKeyExpr,
        admin_queryable: &Queryable<'_, flume::Receiver<Query>>,
    ) {
        debug!(r#"Run in "local discovery" mode"#);

        loop {
            select!(
                evt = dds_disco_rcv.recv_async() => {
                    match evt.unwrap() {
                        DiscoveryEvent::DiscoveredPublication {
                            mut entity
                        } => {
                            debug!("Discovered DDS Writer {} on {} with type '{}' and QoS: {:?}", entity.key, entity.topic_name, entity.type_name, entity.qos);
                            // get its admin_keyexpr
                            let admin_keyexpr = DdsPluginRuntime::get_admin_keyexpr(&entity, true);

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
                                let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, None).unwrap();
                                let route_status = self.try_add_route_from_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos, congestion_ctrl).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                        // add Writer's key in local_matched_writers list
                                        r.local_routed_writers.push(entity.key.clone());
                                    }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.qos.partitions {
                                    let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, Some(p)).unwrap();
                                    let route_status = self.try_add_route_from_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone(), congestion_ctrl).await;
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
                            self.insert_dds_writer(admin_keyexpr, entity);
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
                                            let ke = *KE_PREFIX_ROUTE_FROM_DDS / zkey;
                                            admin_space.remove(&ke);
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
                            debug!("Discovered DDS Reader {} on {} with type '{}' and QoS: {:?}", entity.key, entity.topic_name, entity.type_name, entity.qos);
                            let admin_keyexpr = DdsPluginRuntime::get_admin_keyexpr(&entity, false);

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
                                let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, None).unwrap();
                                let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                        // if route has been created, add this Reader in its routed_readers list
                                        r.local_routed_readers.push(entity.key.clone());
                                    }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in &entity.qos.partitions {
                                    let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, Some(p)).unwrap();
                                    let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone()).await;
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
                            self.insert_dds_reader(admin_keyexpr, entity);
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
                                            let ke = *KE_PREFIX_ROUTE_TO_DDS / zkey;
                                            admin_space.remove(&ke);
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

                group_event = group_subscriber.recv_async() => {
                    match group_event {
                        Ok(GroupEvent::Join(JoinEvent{member})) => {
                            debug!("New zenoh_dds_plugin detected: {}", member.id());
                            if let Ok(member_id) = keyexpr::new(member.id()) {
                                // make all QueryingSubscriber to query this new member
                                for (zkey, zsub) in &mut self.routes_to_dds {
                                    if let ZSubscriber::QueryingSubscriber(sub) = &mut zsub.zenoh_subscriber {
                                        let query_ke = *KE_PREFIX_PUB_CACHE / member_id / zkey;
                                        debug!("Query historical data from {} for TRANSIENT_LOCAL topic on: {}", member_id, query_ke);
                                        if let Err(e) = sub.query_on(Selector::from(&query_ke), QueryTarget::All, ConsolidationMode::None, Duration::from_secs(5)).res().await {
                                            warn!("Query on {} for TRANSIENT_LOCAL topic failed: {}", query_ke, e);
                                        }
                                    }
                                }
                            } else {
                                error!("Can't convert member id '{}' into a KeyExpr", member.id());
                            }
                        }
                        Ok(_) => {} // ignore other GroupEvents
                        Err(e) => warn!("Error receiving GroupEvent: {}", e)
                    }
                }

                get_request = admin_queryable.recv_async() => {
                    if let Ok(query) = get_request {
                        self.treat_admin_query(query, &admin_keyexpr_prefix).await;
                    } else {
                        warn!("AdminSpace queryable was closed!");
                    }
                }
            )
        }
    }

    async fn run_fwd_discovery_mode(
        &mut self,
        group_subscriber: &Receiver<GroupEvent>,
        dds_disco_rcv: &Receiver<DiscoveryEvent>,
        admin_keyexpr_prefix: OwnedKeyExpr,
        admin_queryable: &Queryable<'_, flume::Receiver<Query>>,
    ) {
        debug!(r#"Run in "forward discovery" mode"#);

        // The data space where all discovery info are fowarded:
        //   - writers discovery on <KE_PREFIX_FWD_DISCO>/<uuid>/[<scope>]/writer/<dds_entity_admin_key>
        //   - readers discovery on <KE_PREFIX_FWD_DISCO>/<uuid>/[<scope>]/reader/<dds_entity_admin_key>
        //   - ros_discovery_info on <KE_PREFIX_FWD_DISCO>/<uuid>/[<scope>]/ros_disco/<gid>
        // The PublicationCache is declared on <KE_PREFIX_FWD_DISCO>/<uuid>/[<scope>]/**
        // The QuerySubscriber is declared on  <KE_PREFIX_FWD_DISCO>/*/[<scope>]/**
        let uuid: OwnedKeyExpr = self.zsession.zid().try_into().unwrap();
        let fwd_key_prefix = if let Some(scope) = &self.config.scope {
            *KE_PREFIX_FWD_DISCO / &uuid / scope
        } else {
            *KE_PREFIX_FWD_DISCO / &uuid
        };
        let fwd_writers_key_prefix = &fwd_key_prefix / ke_for_sure!("writer");
        let fwd_readers_key_prefix = &fwd_key_prefix / ke_for_sure!("reader");
        let fwd_ros_discovery_key = &fwd_key_prefix / ke_for_sure!("ros_disco");
        let fwd_declare_publication_cache_key = &fwd_key_prefix / *KE_ANY_N_SEGMENT;
        let fwd_discovery_subscription_key = if let Some(scope) = &self.config.scope {
            *KE_PREFIX_FWD_DISCO / *KE_ANY_1_SEGMENT / scope / *KE_ANY_N_SEGMENT
        } else {
            *KE_PREFIX_FWD_DISCO / *KE_ANY_1_SEGMENT / *KE_ANY_N_SEGMENT
        };

        // Register prefixes for optimization
        let fwd_writers_key_prefix_key = self
            .zsession
            .declare_keyexpr(fwd_writers_key_prefix)
            .res()
            .await
            .expect("Failed to declare key expression for Fwd Discovery of writers");
        let fwd_readers_key_prefix_key = self
            .zsession
            .declare_keyexpr(fwd_readers_key_prefix)
            .res()
            .await
            .expect("Failed to declare key expression for Fwd Discovery of readers");
        let fwd_ros_discovery_key_declared = self
            .zsession
            .declare_keyexpr(&fwd_ros_discovery_key)
            .res()
            .await
            .expect("Failed to declare key expression for Fwd Discovery of ros_discovery");

        // Cache the publications on admin space for late joiners DDS plugins
        let _fwd_disco_pub_cache = self
            .zsession
            .declare_publication_cache(fwd_declare_publication_cache_key)
            .queryable_allowed_origin(Locality::Remote) // Note: don't reply to queries from local QueryingSubscribers
            .res()
            .await
            .expect("Failed to declare PublicationCache for Fwd Discovery");

        // Subscribe to remote DDS plugins publications of new Readers/Writers on admin space
        let mut fwd_disco_sub = self
            .zsession
            .declare_querying_subscriber(fwd_discovery_subscription_key)
            .allowed_origin(Locality::Remote) // Note: ignore my own publications
            .res()
            .await
            .expect("Failed to declare QueryingSubscriber for Fwd Discovery");

        // Manage ros_discovery_info topic, reading it periodically
        let ros_disco_mgr =
            RosDiscoveryInfoMgr::create(self.dp).expect("Failed to create RosDiscoveryInfoMgr");
        let timer = Timer::default();
        let (tx, ros_disco_timer_rcv): (Sender<()>, Receiver<()>) = unbounded();
        let ros_disco_timer_event = TimedEvent::periodic(
            Duration::from_millis(ROS_DISCOVERY_INFO_POLL_INTERVAL_MS),
            ChannelEvent { tx },
        );
        timer.add_async(ros_disco_timer_event).await;

        // The ParticipantEntitiesInfo to be re-published on ros_discovery_info (with this bridge's participant gid)
        let mut participant_info = ParticipantEntitiesInfo::new(
            get_guid(&self.dp).expect("Failed to get my Participant's guid"),
        );

        let scope = self.config.scope.clone();
        loop {
            select!(
                evt = dds_disco_rcv.recv_async() => {
                    match evt.unwrap() {
                        DiscoveryEvent::DiscoveredPublication {
                            entity
                        } => {
                            debug!("Discovered DDS Writer {} on {} with type '{}' and QoS: {:?} => advertise it", entity.key, entity.topic_name, entity.type_name, entity.qos);
                            // advertise the entity and its scope within admin space (bincode format)
                            let admin_keyexpr = DdsPluginRuntime::get_admin_keyexpr(&entity, true);
                            let fwd_ke = &fwd_writers_key_prefix_key / &admin_keyexpr;
                            let msg = (&entity, &scope);
                            let ser_msg = match bincode::serialize(&msg) {
                                Ok(s) => s,
                                Err(e) => { error!("INTERNAL ERROR: failed to serialize discovery message for {:?}: {}", entity, e); continue; }
                            };
                            if let Err(e) = self.zsession.put(&fwd_ke, ser_msg).congestion_control(CongestionControl::Block).res().await {
                                error!("INTERNAL ERROR: failed to publish discovery message on {}: {}", fwd_ke, e);
                            }

                            // store the writer in admin space
                            self.insert_dds_writer(admin_keyexpr, entity);
                        }

                        DiscoveryEvent::UndiscoveredPublication {
                            key,
                        } => {
                            debug!("Undiscovered DDS Writer {} => advertise it", key);
                            if let Some((admin_keyexpr, _)) = self.remove_dds_writer(&key) {
                                let fwd_ke = &fwd_writers_key_prefix_key / &admin_keyexpr;
                                // publish its deletion from admin space
                                if let Err(e) = self.zsession.delete(&fwd_ke).congestion_control(CongestionControl::Block).res().await {
                                    error!("INTERNAL ERROR: failed to publish undiscovery message on {:?}: {}", fwd_ke, e);
                                }
                            }
                        }

                        DiscoveryEvent::DiscoveredSubscription {
                            entity
                        } => {
                            debug!("Discovered DDS Reader {} on {} with type '{}' and QoS: {:?} => advertise it", entity.key, entity.topic_name, entity.type_name, entity.qos);
                            // advertise the entity and its scope within admin space (bincode format)
                            let admin_keyexpr = DdsPluginRuntime::get_admin_keyexpr(&entity, false);
                            let fwd_ke = &fwd_readers_key_prefix_key / &admin_keyexpr;
                            let msg = (&entity, &scope);
                            let ser_msg = match bincode::serialize(&msg) {
                                Ok(s) => s,
                                Err(e) => { error!("INTERNAL ERROR: failed to serialize discovery message for {:?}: {}", entity, e); continue; }
                            };
                            if let Err(e) = self.zsession.put(&fwd_ke, ser_msg).congestion_control(CongestionControl::Block).res().await {
                                error!("INTERNAL ERROR: failed to publish discovery message on {}: {}", fwd_ke, e);
                            }

                            // store the reader
                            self.insert_dds_reader(admin_keyexpr, entity);
                        }

                        DiscoveryEvent::UndiscoveredSubscription {
                            key,
                        } => {
                            debug!("Undiscovered DDS Reader {} => advertise it", key);
                            if let Some((admin_keyexpr, _)) = self.remove_dds_reader(&key) {
                                let fwd_ke = &fwd_readers_key_prefix_key / &admin_keyexpr;
                                // publish its deletion from admin space
                                if let Err(e) = self.zsession.delete(&fwd_ke).congestion_control(CongestionControl::Block).res().await {
                                    error!("INTERNAL ERROR: failed to publish undiscovery message on {:?}: {}", fwd_ke, e);
                                }
                            }
                        }
                    }
                },

                sample = fwd_disco_sub.recv_async() => {
                    let sample = sample.expect("Fwd Discovery subscriber was closed!");
                    let fwd_ke = &sample.key_expr;
                    debug!("Received forwarded discovery message on {}", fwd_ke);

                    // parse fwd_ke and extract the remote uuid, the discovery kind (reader|writer|ros_disco) and the remaining of the keyexpr
                    if let Some((remote_uuid, disco_kind, remaining_ke)) = Self::parse_fwd_discovery_keyexpr(fwd_ke) {
                        match disco_kind {
                            // it's a writer discovery message
                            "writer" => {
                                // reconstruct full admin keyexpr for this entity (i.e. with it's remote plugin's uuid)
                                let full_admin_keyexpr = *KE_PREFIX_ADMIN_SPACE / remote_uuid / ke_for_sure!("dds") / remaining_ke;
                                if sample.kind != SampleKind::Delete {
                                    // deserialize payload
                                    let (entity, scope) = match bincode::deserialize::<(DdsEntity, Option<OwnedKeyExpr>)>(&sample.payload.contiguous()) {
                                        Ok(x) => x,
                                        Err(e) => {
                                            warn!("Failed to deserialize discovery msg for {}: {}", full_admin_keyexpr, e);
                                            continue;
                                        }
                                    };
                                    // copy and adapt Writer's QoS for creation of a proxy Writer
                                    let mut qos = entity.qos.clone();
                                    qos.ignore_local_participant = true;

                                    // create 1 "to_dds" route per partition, or just 1 if no partition
                                    if entity.qos.partitions.is_empty() {
                                        let ke = self.topic_to_keyexpr(&entity.topic_name, &scope, None).unwrap();
                                        let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos).await;
                                        if let RouteStatus::Routed(ref route_key) = route_status {
                                            if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                                // add the writer's admin keyexpr to the list of remote_routed_writers
                                                r.remote_routed_writers.push(full_admin_keyexpr);
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
                                            let ke = self.topic_to_keyexpr(&entity.topic_name, &scope, Some(p)).unwrap();
                                            let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone()).await;
                                            if let RouteStatus::Routed(ref route_key) = route_status {
                                                if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                                    // add the writer's admin keyexpr to the list of remote_routed_writers
                                                    r.remote_routed_writers.push(full_admin_keyexpr.clone());
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
                                            route.remote_routed_writers.retain(|s| s != &full_admin_keyexpr);
                                            if route.remote_routed_writers.is_empty() {
                                                info!(
                                                    "Remove unused route: zenoh '{}' => DDS '{}'",
                                                    zkey, zkey
                                                );
                                                let ke = *KE_PREFIX_ROUTE_TO_DDS / zkey;
                                                admin_space.remove(&ke);
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
                                // reconstruct full admin keyexpr for this entity (i.e. with it's remote plugin's uuid)
                                let full_admin_keyexpr = *KE_PREFIX_ADMIN_SPACE / remote_uuid / ke_for_sure!("dds") / remaining_ke;
                                if sample.kind != SampleKind::Delete {
                                    // deserialize payload
                                    let (entity, scope) = match bincode::deserialize::<(DdsEntity, Option<OwnedKeyExpr>)>(&sample.payload.contiguous()) {
                                        Ok(x) => x,
                                        Err(e) => {
                                            warn!("Failed to deserialize discovery msg for {}: {}", full_admin_keyexpr, e);
                                            continue;
                                        }
                                    };
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
                                        let ke = self.topic_to_keyexpr(&entity.topic_name, &scope, None).unwrap();
                                        let route_status = self.try_add_route_from_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos, congestion_ctrl).await;
                                        if let RouteStatus::Routed(ref route_key) = route_status {
                                            if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                                // add the reader's admin keyexpr to the list of remote_routed_writers
                                                r.remote_routed_readers.push(full_admin_keyexpr);
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
                                            let ke = self.topic_to_keyexpr(&entity.topic_name, &scope, Some(p)).unwrap();
                                            let route_status = self.try_add_route_from_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone(), congestion_ctrl).await;
                                            if let RouteStatus::Routed(ref route_key) = route_status {
                                                if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                                    // add the reader's admin keyexpr to the list of remote_routed_writers
                                                    r.remote_routed_readers.push(full_admin_keyexpr.clone());
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
                                            route.remote_routed_readers.retain(|s| s != &full_admin_keyexpr);
                                            if route.remote_routed_readers.is_empty() {
                                                info!(
                                                    "Remove unused route: DDS '{}' => zenoh '{}'",
                                                    zkey, zkey
                                                );
                                                let ke = *KE_PREFIX_ROUTE_FROM_DDS / zkey;
                                                admin_space.remove(&ke);
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
                                    &*sample.payload.contiguous(),
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
                                        fwd_ke, e
                                    ),
                                }
                            }

                            x => {
                                error!("Unexpected forwarded discovery message received on invalid key {} (unkown kind: {}) ", fwd_ke, x);
                            }
                        }
                    }
                },

                group_event = group_subscriber.recv_async() => {
                    match group_event {
                        Ok(GroupEvent::Join(JoinEvent{member})) => {
                            debug!("New zenoh_dds_plugin detected: {}", member.id());
                            // query for past publications of discocvery messages from this new member
                            let key = if let Some(scope) = &self.config.scope {
                                *KE_PREFIX_FWD_DISCO / ke_for_sure!(member.id()) / scope / *KE_ANY_N_SEGMENT
                            } else {
                                *KE_PREFIX_FWD_DISCO / ke_for_sure!(member.id()) / *KE_ANY_N_SEGMENT
                            };
                            debug!("Query past discovery messages from {} on {}", member.id(), key);
                            if let Err(e) = fwd_disco_sub.query_on(Selector::from(&key), QueryTarget::All, ConsolidationMode::None, Duration::from_secs(5)).res().await {
                                warn!("Query on {} for discovery messages failed: {}", key, e);
                            }
                            // make all QueryingSubscriber to query this new member
                            for (zkey, zsub) in &mut self.routes_to_dds {
                                if let ZSubscriber::QueryingSubscriber(sub) = &mut zsub.zenoh_subscriber {
                                    let rkey = *KE_PREFIX_PUB_CACHE / ke_for_sure!(member.id()) / zkey;
                                    debug!("Query historical data from {} for TRANSIENT_LOCAL topic on: {}", member.id(), rkey);
                                    if let Err(e) = sub.query_on(Selector::from(&rkey), QueryTarget::All, ConsolidationMode::None, Duration::from_secs(5)).res().await {
                                        warn!("Query on {} for TRANSIENT_LOCAL topic failed: {}", rkey, e);
                                    }
                                }
                            }
                        }
                        Ok(GroupEvent::Leave(LeaveEvent{mid})) | Ok(GroupEvent::LeaseExpired(LeaseExpiredEvent{mid})) => {
                            debug!("Remote zenoh_dds_plugin left: {}", mid);
                            // remove all the references to the plugin's enities, removing no longer used routes
                            // and updating/re-publishing ParticipantEntitiesInfo
                            let admin_space = &mut self.admin_space;
                            let admin_subke = format!("@/service/{}/dds/", mid);
                            let mut participant_info_changed = false;
                            self.routes_to_dds.retain(|zkey, route| {
                                route.remote_routed_writers.retain(|s| !s.contains(&admin_subke));
                                if route.remote_routed_writers.is_empty() {
                                    info!(
                                        "Remove unused route: zenoh '{}' => DDS '{}'",
                                        zkey, zkey
                                    );
                                    let ke = *KE_PREFIX_ROUTE_TO_DDS / zkey;
                                    admin_space.remove(&ke);
                                    if let Ok(guid) = get_guid(&route.serving_writer) {
                                        participant_info.remove_writer_gid(&guid);
                                        participant_info_changed = true;
                                    } else {
                                        warn!("Failed to get guid for Writer serving the route zenoh '{}' => DDS '{}'. Can't update ros_discovery_info accordingly", zkey, zkey);
                                    }
                                    false
                                } else {
                                    true
                                }
                            });
                            self.routes_from_dds.retain(|zkey, route| {
                                route.remote_routed_readers.retain(|s| !s.contains(&admin_subke));
                                if route.remote_routed_readers.is_empty() {
                                    info!(
                                        "Remove unused route: DDS '{}' => zenoh '{}'",
                                        zkey, zkey
                                    );
                                    let ke = *KE_PREFIX_ROUTE_FROM_DDS / zkey;
                                    admin_space.remove(&ke);
                                    if let Ok(guid) = get_guid(&route.serving_reader) {
                                        participant_info.remove_reader_gid(&guid);
                                        participant_info_changed = true;
                                    } else {
                                        warn!("Failed to get guid for Reader serving the route DDS '{}' => zenoh '{}'. Can't update ros_discovery_info accordingly", zkey, zkey);
                                    }
                                    false
                                } else {
                                    true
                                }
                            });
                            if participant_info_changed {
                                debug!("Publishing up-to-date ros_discovery_info after leaving of plugin {}", mid);
                                participant_info.cleanup();
                                if let Err(e) = ros_disco_mgr.write(&participant_info) {
                                    error!("Error forwarding ros_discovery_info: {}", e);
                                }
                            }

                        }
                        Ok(_) => {}, // ignore other GroupEvent
                        Err(e) => warn!("Error receiving GroupEvent: {}", e)
                    }
                }

                get_request = admin_queryable.recv_async() => {
                    if let Ok(query) = get_request {
                        self.treat_admin_query(query, &admin_keyexpr_prefix).await;
                    } else {
                        warn!("AdminSpace queryable was closed!");
                    }
                }

                _ = ros_disco_timer_rcv.recv_async() => {
                    let infos = ros_disco_mgr.read();
                    for (gid, buf) in infos {
                        trace!("Received ros_discovery_info from DDS for {}, forward via zenoh: {}", gid, hex::encode(buf.contiguous()));
                        // forward the payload on zenoh
                        let ke = &fwd_ros_discovery_key_declared / ke_for_sure!(&gid);
                        if let Err(e) = self.zsession.put(ke, buf).res().await {
                            error!("Forward ROS discovery info failed: {}", e);
                        }
                    }
                }
            )
        }
    }

    fn parse_fwd_discovery_keyexpr(fwd_ke: &keyexpr) -> Option<(&keyexpr, &str, &keyexpr)> {
        // parse fwd_ke which have format: "KE_PREFIX_FWD_DISCO/<uuid>[/scope/possibly/multiple]/<disco_kind>/<remaining_ke...>"
        if !fwd_ke.starts_with(KE_PREFIX_FWD_DISCO.as_str()) {
            // publication on a key expression matching the fwd_ke: ignore it
            return None;
        }
        let mut remaining = &fwd_ke[KE_PREFIX_FWD_DISCO.len() + 1..];
        let uuid = if let Some(i) = remaining.find('/') {
            let uuid = ke_for_sure!(&remaining[..i]);
            remaining = &remaining[i..];
            uuid
        } else {
            error!(
                "Unexpected forwarded discovery message received on invalid key: {}",
                fwd_ke
            );
            return None;
        };
        let kind = if let Some(i) = remaining.find("/reader/") {
            remaining = &remaining[i + 8..];
            "reader"
        } else if let Some(i) = remaining.find("/writer/") {
            remaining = &remaining[i + 8..];
            "writer"
        } else if let Some(i) = remaining.find("/ros_disco/") {
            remaining = &remaining[i + 11..];
            "ros_disco"
        } else {
            error!("Unexpected forwarded discovery message received on invalid key: {} (no expected kind '/reader/', '/writer/' or '/ros_disco/')", fwd_ke);
            return None;
        };
        Some((uuid, kind, ke_for_sure!(remaining)))
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
                        if let Ok(gid) = get_guid(&route.serving_reader) {
                            trace!(
                                "ros_discovery_info remap reader {} -> {}",
                                node.reader_gid_seq[i],
                                gid
                            );
                            node.reader_gid_seq[i] = gid;
                            i += 1;
                        } else {
                            error!("Failed to get guid for Reader serving the a route. Can't remap in ros_discovery_info");
                        }
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
                        if let Ok(gid) = get_guid(&route.serving_writer) {
                            trace!(
                                "ros_discovery_info remap writer {} -> {}",
                                node.writer_gid_seq[i],
                                gid
                            );
                            node.writer_gid_seq[i] = gid;
                            i += 1;
                        } else {
                            error!("Failed to get guid for Writer serving the a route. Can't remap in ros_discovery_info");
                        }
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
        if self.tx.send(()).is_err() {
            warn!("Error sending periodic timer notification on channel");
        };
    }
}
