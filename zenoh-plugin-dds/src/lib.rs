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
use cyclors::qos::{
    DurabilityService, History, IgnoreLocal, IgnoreLocalKind, Qos, Reliability, ReliabilityKind,
    DDS_100MS_DURATION, DDS_1S_DURATION,
};
use cyclors::*;
use flume::{unbounded, Receiver, Sender};
use futures::select;
use git_version::git_version;
use log::{debug, error, info, trace, warn};
use route_dds_zenoh::RouteDDSZenoh;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;
use std::convert::TryInto;
use std::env;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::Duration;
use zenoh::buffers::SplitBuffer;
use zenoh::liveliness::LivelinessToken;
use zenoh::plugins::{Plugin, RunningPluginTrait, Runtime, ZenohPlugin};
use zenoh::prelude::r#async::AsyncResolve;
use zenoh::prelude::r#sync::SyncResolve;
use zenoh::prelude::*;
use zenoh::publication::CongestionControl;
use zenoh::query::{ConsolidationMode, QueryTarget};
use zenoh::queryable::{Query, Queryable};
use zenoh::Result as ZResult;
use zenoh::Session;
use zenoh_core::{bail, zerror};
use zenoh_ext::{SessionExt, SubscriberBuilderExt};
use zenoh_util::{Timed, TimedEvent, Timer};

pub mod config;
mod dds_mgt;
mod qos_helpers;
mod ros_discovery;
mod route_dds_zenoh;
mod route_zenoh_dds;
use config::Config;
use dds_mgt::*;

use crate::qos_helpers::*;
use crate::ros_discovery::{
    NodeEntitiesInfo, ParticipantEntitiesInfo, RosDiscoveryInfoMgr, ROS_DISCOVERY_INFO_TOPIC_NAME,
};
use crate::route_zenoh_dds::RouteZenohDDS;

pub const GIT_VERSION: &str = git_version!(prefix = "v", cargo_prefix = "v");

macro_rules! ke_for_sure {
    ($val:expr) => {
        unsafe { keyexpr::from_str_unchecked($val) }
    };
}

macro_rules! member_id {
    ($val:expr) => {
        $val.key_expr.as_str().split('/').last().unwrap()
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
    static ref KE_PREFIX_LIVELINESS_GROUP: &'static keyexpr = ke_for_sure!("zenoh-plugin-dds");

    static ref KE_ANY_1_SEGMENT: &'static keyexpr = ke_for_sure!("*");
    static ref KE_ANY_N_SEGMENT: &'static keyexpr = ke_for_sure!("**");
);

// CycloneDDS' localhost-only: set network interface address (shortened form of config would be
// possible, too, but I think it is clearer to spell it out completely).
// Empty configuration fragments are ignored, so it is safe to unconditionally append a comma.
const CYCLONEDDS_CONFIG_LOCALHOST_ONLY: &str = r#"<CycloneDDS><Domain><General><Interfaces><NetworkInterface address="127.0.0.1" multicast="true"/></Interfaces></General></Domain></CycloneDDS>,"#;

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
        .res_async()
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
    let member = match zsession
        .liveliness()
        .declare_token(*KE_PREFIX_LIVELINESS_GROUP / &member_id)
        .res_async()
        .await
    {
        Ok(member) => member,
        Err(e) => {
            log::error!("Unable todeclare liveliness token for DDS plugin : {:?}", e);
            return;
        }
    };

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
        _member: member,
        member_id,
        dp,
        discovered_participants: HashMap::<String, DdsParticipant>::new(),
        discovered_writers: HashMap::<String, DdsEntity>::new(),
        discovered_readers: HashMap::<String, DdsEntity>::new(),
        routes_from_dds: HashMap::<OwnedKeyExpr, RouteDDSZenoh>::new(),
        routes_to_dds: HashMap::<OwnedKeyExpr, RouteZenohDDS>::new(),
        admin_space: HashMap::<OwnedKeyExpr, AdminRef>::new(),
    };

    dds_plugin.run().await;
}

// An reference used in admin space to point to a struct (DdsEntity or Route) stored in another map
#[derive(Debug)]
enum AdminRef {
    DdsParticipant(String),
    DdsWriterEntity(String),
    DdsReaderEntity(String),
    FromDdsRoute(OwnedKeyExpr),
    ToDdsRoute(OwnedKeyExpr),
    Config,
    Version,
}

pub(crate) struct DdsPluginRuntime<'a> {
    config: Config,
    // Note: &'a Arc<Session> here to keep the ownership of Session outside this struct
    // and be able to store the publishers/subscribers it creates in this same struct.
    zsession: &'a Arc<Session>,
    _member: LivelinessToken<'a>,
    member_id: OwnedKeyExpr,
    dp: dds_entity_t,
    // maps of all discovered DDS entities (indexed by DDS key)
    discovered_participants: HashMap<String, DdsParticipant>,
    discovered_writers: HashMap<String, DdsEntity>,
    discovered_readers: HashMap<String, DdsEntity>,
    // maps of established routes from/to DDS (indexed by zenoh key expression)
    routes_from_dds: HashMap<OwnedKeyExpr, RouteDDSZenoh<'a>>,
    routes_to_dds: HashMap<OwnedKeyExpr, RouteZenohDDS<'a>>,
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
                .map(|(re, freq)| format!("{re}={freq}"))
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

    fn get_participant_admin_keyexpr(e: &DdsParticipant) -> OwnedKeyExpr {
        format!("participant/{}", e.key,).try_into().unwrap()
    }

    fn get_entity_admin_keyexpr(e: &DdsEntity, is_writer: bool) -> OwnedKeyExpr {
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

    fn insert_dds_participant(&mut self, admin_keyexpr: OwnedKeyExpr, e: DdsParticipant) {
        // insert reference in admin space
        self.admin_space
            .insert(admin_keyexpr, AdminRef::DdsParticipant(e.key.clone()));

        // insert DdsParticipant in discovered_participants map
        self.discovered_participants.insert(e.key.clone(), e);
    }

    fn remove_dds_participant(&mut self, dds_key: &str) -> Option<(OwnedKeyExpr, DdsParticipant)> {
        // remove fron participants map
        if let Some(e) = self.discovered_participants.remove(dds_key) {
            // remove from admin_space
            let admin_keyexpr = DdsPluginRuntime::get_participant_admin_keyexpr(&e);
            self.admin_space.remove(&admin_keyexpr);
            Some((admin_keyexpr, e))
        } else {
            None
        }
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
            let admin_keyexpr = DdsPluginRuntime::get_entity_admin_keyexpr(&e, true);
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
            let admin_keyexpr = DdsPluginRuntime::get_entity_admin_keyexpr(&e, false);
            self.admin_space.remove(&admin_keyexpr);
            Some((admin_keyexpr, e))
        } else {
            None
        }
    }

    fn insert_route_from_dds(&mut self, ke: OwnedKeyExpr, r: RouteDDSZenoh<'a>) {
        // insert reference in admin_space
        let admin_ke = *KE_PREFIX_ROUTE_FROM_DDS / &ke;
        self.admin_space
            .insert(admin_ke, AdminRef::FromDdsRoute(ke.clone()));

        // insert route in routes_from_dds map
        self.routes_from_dds.insert(ke, r);
    }

    fn insert_route_to_dds(&mut self, ke: OwnedKeyExpr, r: RouteZenohDDS<'a>) {
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

        // create route DDS->Zenoh
        match RouteDDSZenoh::new(
            self,
            topic_name.into(),
            topic_type.into(),
            keyless,
            reader_qos,
            ke.clone(),
            congestion_ctrl,
        )
        .await
        {
            Ok(route) => {
                info!("{}: created with topic_type={}", route, topic_type);
                self.insert_route_from_dds(ke.clone(), route);
                RouteStatus::Routed(ke)
            }
            Err(e) => {
                error!(
                    "Route DDS->Zenoh ({} -> {}): creation failed: {}",
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
        is_transient: bool,
        writer_qos: Option<Qos>,
    ) -> RouteStatus {
        if !self.is_allowed(&ke) {
            info!(
                "Ignoring Subscription for resource {} as it is not allowed (see your 'allow' or 'deny' configuration)",
                ke
            );
            return RouteStatus::NotAllowed;
        }

        if let Some(route) = self.routes_to_dds.get(&ke) {
            // TODO: check if there is no type or QoS conflict with existing route
            debug!(
                "Route from resource {} to DDS already exists -- ignoring",
                ke
            );
            // #102: in forwarding mode, it might happen that the route have been created but without DDS Writer
            //       (just to declare the Zenoh Subscriber). Thus, try to set a DDS Writer to the route here.
            //       If already set, nothing will happen.
            if let Some(qos) = writer_qos {
                if let Err(e) = route.set_dds_writer(self.dp, qos) {
                    error!(
                        "{}: failed to set a DDS Writer after creation: {}",
                        route, e
                    );
                    return RouteStatus::CreationFailure(e);
                }
            }
            return RouteStatus::Routed(ke);
        }

        // create route Zenoh->DDS
        match RouteZenohDDS::new(
            self,
            ke.clone(),
            is_transient,
            topic_name.into(),
            topic_type.into(),
            keyless,
        )
        .await
        {
            Ok(route) => {
                // if writer_qos is set, add a DDS Writer to the route
                if let Some(qos) = writer_qos {
                    if let Err(e) = route.set_dds_writer(self.dp, qos) {
                        error!(
                            "Route Zenoh->DDS ({} -> {}): creation failed: {}",
                            ke, topic_name, e
                        );
                        return RouteStatus::CreationFailure(e);
                    }
                }

                info!("{}: created with topic_type={}", route, topic_type);
                self.insert_route_to_dds(ke.clone(), route);
                RouteStatus::Routed(ke)
            }
            Err(e) => {
                error!(
                    "Route Zenoh->DDS ({} -> {}): creation failed: {}",
                    ke, topic_name, e
                );
                RouteStatus::CreationFailure(e)
            }
        }
    }

    fn get_admin_value(&self, admin_ref: &AdminRef) -> Result<Option<Value>, serde_json::Error> {
        match admin_ref {
            AdminRef::DdsParticipant(key) => self
                .discovered_participants
                .get(key)
                .map(serde_json::to_value)
                .transpose(),
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
            if let Err(e) = query
                .reply(Ok(Sample::new(admin_keyexpr, v)))
                .res_async()
                .await
            {
                warn!("Error replying to admin query {:?}: {}", query, e);
            }
        }
    }

    async fn run(&mut self) {
        let group_subscriber = self
            .zsession
            .liveliness()
            .declare_subscriber(*KE_PREFIX_LIVELINESS_GROUP / *KE_ANY_N_SEGMENT)
            .querying()
            .with(flume::unbounded())
            .res_async()
            .await
            .expect("Failed to create Liveliness Subscriber");

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
            .res_async()
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
            (Some(scope), Some(part)) => scope.join(&format!("{part}/{topic_name}")),
            (Some(scope), None) => scope.join(topic_name),
            (None, Some(part)) => format!("{part}/{topic_name}").try_into(),
            (None, None) => topic_name.try_into(),
        }
    }

    async fn run_local_discovery_mode(
        &mut self,
        group_subscriber: &Receiver<Sample>,
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
                            let admin_keyexpr = DdsPluginRuntime::get_entity_admin_keyexpr(&entity, true);

                            let qos = adapt_writer_qos_for_reader(&entity.qos);
                            // CongestionControl to be used when re-publishing over zenoh: Blocking if Writer is RELIABLE (since we don't know what is remote Reader's QoS)
                            let congestion_ctrl = match (self.config.reliable_routes_blocking, is_writer_reliable(&entity.qos.reliability)) {
                                (true, true) => CongestionControl::Block,
                                _ => CongestionControl::Drop,
                            };

                            // create 1 route per partition, or just 1 if no partition
                            if partition_is_empty(&entity.qos.partition) {
                                let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, None).unwrap();
                                let route_status = self.try_add_route_from_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos, congestion_ctrl).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                        // add Writer's key to the route
                                        r.add_local_routed_writer(entity.key.clone());
                                    }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in entity.qos.partition.as_deref().unwrap() {
                                    let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, Some(p)).unwrap();
                                    let route_status = self.try_add_route_from_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone(), congestion_ctrl).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                            // if route has been created, add this Writer in its routed_writers list
                                            r.add_local_routed_writer(entity.key.clone());
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
                            if let Some((_, e)) = self.remove_dds_writer(&key) {
                                debug!("Undiscovered DDS Writer {} on topic {}", key, e.topic_name);
                                // remove it from all the active routes refering it (deleting the route if no longer used)
                                let admin_space = &mut self.admin_space;
                                self.routes_from_dds.retain(|zkey, route| {
                                        route.remove_local_routed_writer(&key);
                                        if !route.has_local_routed_writer() {
                                            info!(
                                                "{}: remove it as no longer unused (no local DDS Writer left)",
                                                route
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
                            let admin_keyexpr = DdsPluginRuntime::get_entity_admin_keyexpr(&entity, false);

                            let qos = adapt_reader_qos_for_writer(&entity.qos);

                            // create 1 route per partition, or just 1 if no partition
                            if partition_is_empty(&entity.qos.partition) {
                                let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, None).unwrap();
                                let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, is_transient_local(&qos), Some(qos)).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                        // if route has been created, add this Reader in its routed_readers list
                                        r.add_local_routed_reader(entity.key.clone());
                                    }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in entity.qos.partition.as_deref().unwrap() {
                                    let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, Some(p)).unwrap();
                                    let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, is_transient_local(&qos), Some(qos.clone())).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                            // if route has been created, add this Reader in its routed_readers list
                                            r.add_local_routed_reader(entity.key.clone());
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
                            if let Some((_, e)) = self.remove_dds_reader(&key) {
                                debug!("Undiscovered DDS Reader {} on topic {}", key, e.topic_name);
                                // remove it from all the active routes refering it (deleting the route if no longer used)
                                let admin_space = &mut self.admin_space;
                                self.routes_to_dds.retain(|zkey, route| {
                                        route.remove_local_routed_reader(&key);
                                        if !route.has_local_routed_reader() {
                                            info!(
                                                "{}: remove it as no longer unused (no local DDS Reader left)",
                                                route
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

                        DiscoveryEvent::DiscoveredParticipant {
                            entity,
                        } => {
                            debug!("Discovered DDS Participant {}", entity.key);
                            let admin_keyexpr = DdsPluginRuntime::get_participant_admin_keyexpr(&entity);

                            // store the participant
                            self.insert_dds_participant(admin_keyexpr, entity);
                        }

                        DiscoveryEvent::UndiscoveredParticipant {
                            key,
                        } => {
                            if let Some((_, _)) = self.remove_dds_participant(&key) {
                                debug!("Undiscovered DDS Participant {}", key);
                            }
                        }
                    }
                },

                group_event = group_subscriber.recv_async() => {
                    match group_event.as_ref().map(|s|s.kind) {
                        Ok(SampleKind::Put) => {
                            let mid = member_id!(group_event.as_ref().unwrap());
                            debug!("New zenoh_dds_plugin detected: {}", mid);
                            if let Ok(member_id) = keyexpr::new(mid) {
                                // make all QueryingSubscriber to query this new member
                                for (zkey, route) in &mut self.routes_to_dds {
                                    route.query_historical_publications(|| (*KE_PREFIX_PUB_CACHE / member_id / zkey).into(), self.config.queries_timeout).await;
                                }
                            } else {
                                error!("Can't convert member id '{}' into a KeyExpr", mid);
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
        group_subscriber: &Receiver<Sample>,
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
            .res_async()
            .await
            .expect("Failed to declare key expression for Fwd Discovery of writers");
        let fwd_readers_key_prefix_key = self
            .zsession
            .declare_keyexpr(fwd_readers_key_prefix)
            .res_async()
            .await
            .expect("Failed to declare key expression for Fwd Discovery of readers");
        let fwd_ros_discovery_key_declared = self
            .zsession
            .declare_keyexpr(&fwd_ros_discovery_key)
            .res_async()
            .await
            .expect("Failed to declare key expression for Fwd Discovery of ros_discovery");

        // Cache the publications on admin space for late joiners DDS plugins
        let _fwd_disco_pub_cache = self
            .zsession
            .declare_publication_cache(fwd_declare_publication_cache_key)
            .queryable_allowed_origin(Locality::Remote) // Note: don't reply to queries from local QueryingSubscribers
            .res_async()
            .await
            .expect("Failed to declare PublicationCache for Fwd Discovery");

        // Subscribe to remote DDS plugins publications of new Readers/Writers on admin space
        let mut fwd_disco_sub = self
            .zsession
            .declare_subscriber(fwd_discovery_subscription_key)
            .querying()
            .allowed_origin(Locality::Remote) // Note: ignore my own publications
            .query_timeout(self.config.queries_timeout)
            .res_async()
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
                            let admin_keyexpr = DdsPluginRuntime::get_entity_admin_keyexpr(&entity, true);
                            let fwd_ke = &fwd_writers_key_prefix_key / &admin_keyexpr;
                            let msg = (&entity, &scope);
                            let ser_msg = match bincode::serialize(&msg) {
                                Ok(s) => s,
                                Err(e) => { error!("INTERNAL ERROR: failed to serialize discovery message for {:?}: {}", entity, e); continue; }
                            };
                            if let Err(e) = self.zsession.put(&fwd_ke, ser_msg).congestion_control(CongestionControl::Block).res_async().await {
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
                                if let Err(e) = self.zsession.delete(&fwd_ke).congestion_control(CongestionControl::Block).res_async().await {
                                    error!("INTERNAL ERROR: failed to publish undiscovery message on {:?}: {}", fwd_ke, e);
                                }
                            }
                        }

                        DiscoveryEvent::DiscoveredSubscription {
                            mut entity
                        } => {
                            debug!("Discovered DDS Reader {} on {} with type '{}' and QoS: {:?} => advertise it", entity.key, entity.topic_name, entity.type_name, entity.qos);

                            // #102: create a local "to_dds" route, but only with the Zenoh Subscriber (not the DDS Writer)
                            // create 1 route per partition, or just 1 if no partition
                            if partition_is_empty(&entity.qos.partition) {
                                let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, None).unwrap();
                                let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, is_transient_local(&entity.qos), None).await;
                                if let RouteStatus::Routed(ref route_key) = route_status {
                                    if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                        // if route has been created, add this Reader in its routed_readers list
                                        r.add_local_routed_reader(entity.key.clone());
                                    }
                                }
                                entity.routes.insert("*".to_string(), route_status);
                            } else {
                                for p in entity.qos.partition.as_deref().unwrap() {
                                    let ke = self.topic_to_keyexpr(&entity.topic_name, &self.config.scope, Some(p)).unwrap();
                                    let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, is_transient_local(&entity.qos), None).await;
                                    if let RouteStatus::Routed(ref route_key) = route_status {
                                        if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                            // if route has been created, add this Reader in its routed_readers list
                                            r.add_local_routed_reader(entity.key.clone());
                                        }
                                    }
                                    entity.routes.insert(p.clone(), route_status);
                                }
                            }

                            // advertise the entity and its scope within admin space (bincode format)
                            let admin_keyexpr = DdsPluginRuntime::get_entity_admin_keyexpr(&entity, false);
                            let fwd_ke = &fwd_readers_key_prefix_key / &admin_keyexpr;
                            let msg = (&entity, &scope);
                            let ser_msg = match bincode::serialize(&msg) {
                                Ok(s) => s,
                                Err(e) => { error!("INTERNAL ERROR: failed to serialize discovery message for {:?}: {}", entity, e); continue; }
                            };
                            if let Err(e) = self.zsession.put(&fwd_ke, ser_msg).congestion_control(CongestionControl::Block).res_async().await {
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
                                if let Err(e) = self.zsession.delete(&fwd_ke).congestion_control(CongestionControl::Block).res_async().await {
                                    error!("INTERNAL ERROR: failed to publish undiscovery message on {:?}: {}", fwd_ke, e);
                                }
                            }
                            // #102: also remove the Reader from all the active routes refering it,
                            // deleting the route if it has no longer local Reader nor remote Writer.
                            let admin_space = &mut self.admin_space;
                            self.routes_to_dds.retain(|zkey, route| {
                                    route.remove_local_routed_reader(&key);
                                    if !route.has_local_routed_reader() && !route.has_remote_routed_writer(){
                                        info!(
                                            "{}: remove it as no longer unused (no local DDS Reader nor remote DDS Writer left)",
                                            route
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

                        DiscoveryEvent::DiscoveredParticipant {
                            entity,
                        } => {
                            debug!("Discovered DDS Participant {}", entity.key);
                            let admin_keyexpr = DdsPluginRuntime::get_participant_admin_keyexpr(&entity);

                            // store the participant
                            self.insert_dds_participant(admin_keyexpr, entity);
                        }

                        DiscoveryEvent::UndiscoveredParticipant {
                            key,
                        } => {
                            if let Some((_, _)) = self.remove_dds_participant(&key) {
                                debug!("Undiscovered DDS Participant {}", key);
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
                                    let qos = adapt_writer_qos_for_proxy_writer(&entity.qos);

                                    // create 1 "to_dds" route per partition, or just 1 if no partition
                                    if partition_is_empty(&entity.qos.partition) {
                                        let ke = self.topic_to_keyexpr(&entity.topic_name, &scope, None).unwrap();
                                        let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, is_transient_local(&qos), Some(qos)).await;
                                        if let RouteStatus::Routed(ref route_key) = route_status {
                                            if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                                // add the writer's admin keyexpr to the list of remote_routed_writers
                                                r.add_remote_routed_writer(full_admin_keyexpr);
                                                // check amongst local Readers is some are matching (only wrt. topic_name and partition. TODO: consider qos match also)
                                                for reader in self.discovered_readers.values_mut() {
                                                    if reader.topic_name == entity.topic_name && partition_is_empty(&reader.qos.partition) {
                                                        r.add_local_routed_reader(reader.key.clone());
                                                        reader.routes.insert("*".to_string(), route_status.clone());
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        for p in entity.qos.partition.as_deref().unwrap() {
                                            let ke = self.topic_to_keyexpr(&entity.topic_name, &scope, Some(p)).unwrap();
                                            let route_status = self.try_add_route_to_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, is_transient_local(&qos), Some(qos.clone())).await;
                                            if let RouteStatus::Routed(ref route_key) = route_status {
                                                if let Some(r) = self.routes_to_dds.get_mut(route_key) {
                                                    // add the writer's admin keyexpr to the list of remote_routed_writers
                                                    r.add_remote_routed_writer(full_admin_keyexpr.clone());
                                                    // check amongst local Readers is some are matching (only wrt. topic_name and partition. TODO: consider qos match also)
                                                    for reader in self.discovered_readers.values_mut() {
                                                        if reader.topic_name == entity.topic_name && partition_contains(&reader.qos.partition, p) {
                                                            r.add_local_routed_reader(reader.key.clone());
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
                                            route.remove_remote_routed_writer(&full_admin_keyexpr);
                                            // #102: don't delete the route if there is still a
                                            // local Reader. But delete its DDS Writer if there are
                                            // no more remote Writer
                                            if !route.has_remote_routed_writer() {
                                                route.delete_dds_writer();
                                            }
                                            if !route.has_local_routed_reader() {
                                                info!(
                                                    "{}: remove it as no longer unused (no remote DDS Writer nor local DDS Reader left)",
                                                    route
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
                                    let qos = adapt_reader_qos_for_proxy_reader(&entity.qos);

                                    // CongestionControl to be used when re-publishing over zenoh: Blocking if Reader is RELIABLE (since Writer will also be, otherwise no matching)
                                    let congestion_ctrl = match (self.config.reliable_routes_blocking, is_reader_reliable(&entity.qos.reliability)) {
                                        (true, true) => CongestionControl::Block,
                                        _ => CongestionControl::Drop,
                                    };

                                    // create 1 'from_dds" route per partition, or just 1 if no partition
                                    if partition_is_empty(&entity.qos.partition) {
                                        let ke = self.topic_to_keyexpr(&entity.topic_name, &scope, None).unwrap();
                                        let route_status = self.try_add_route_from_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos, congestion_ctrl).await;
                                        if let RouteStatus::Routed(ref route_key) = route_status {
                                            if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                                // add the reader's admin keyexpr to the list of remote_routed_writers
                                                r.add_remote_routed_reader(full_admin_keyexpr);
                                                // check amongst local Writers is some are matching (only wrt. topic_name and partition. TODO: consider qos match also)
                                                for writer in self.discovered_writers.values_mut() {
                                                    if writer.topic_name == entity.topic_name && partition_is_empty(&writer.qos.partition) {
                                                        r.add_local_routed_writer(writer.key.clone());
                                                        writer.routes.insert("*".to_string(), route_status.clone());
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        for p in &entity.qos.partition.unwrap() {
                                            let ke = self.topic_to_keyexpr(&entity.topic_name, &scope, Some(p)).unwrap();
                                            let route_status = self.try_add_route_from_dds(ke, &entity.topic_name, &entity.type_name, entity.keyless, qos.clone(), congestion_ctrl).await;
                                            if let RouteStatus::Routed(ref route_key) = route_status {
                                                if let Some(r) = self.routes_from_dds.get_mut(route_key) {
                                                    // add the reader's admin keyexpr to the list of remote_routed_writers
                                                    r.add_remote_routed_reader(full_admin_keyexpr.clone());
                                                    // check amongst local Writers is some are matching (only wrt. topic_name and partition. TODO: consider qos match also)
                                                    for writer in self.discovered_writers.values_mut() {
                                                        if writer.topic_name == entity.topic_name && partition_contains(&writer.qos.partition, p) {
                                                            r.add_local_routed_writer(writer.key.clone());
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
                                            route.remove_remote_routed_reader(&full_admin_keyexpr);
                                            if !route.has_remote_routed_reader() {
                                                info!(
                                                    "{}: remove it as no longer unused (no remote DDS Reader left)",
                                                    route
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
                    match group_event.as_ref().map(|s|s.kind) {
                        Ok(SampleKind::Put) => {
                            let mid = member_id!(group_event.as_ref().unwrap());
                            debug!("New zenoh_dds_plugin detected: {}", mid);
                            // query for past publications of discocvery messages from this new member
                            let key = if let Some(scope) = &self.config.scope {
                                *KE_PREFIX_FWD_DISCO / ke_for_sure!(mid) / scope / *KE_ANY_N_SEGMENT
                            } else {
                                *KE_PREFIX_FWD_DISCO / ke_for_sure!(mid) / *KE_ANY_N_SEGMENT
                            };
                            debug!("Query past discovery messages from {} on {}", mid, key);
                            if let Err(e) = fwd_disco_sub.fetch( |cb| {
                                use zenoh_core::SyncResolve;
                                self.zsession.get(Selector::from(&key))
                                    .callback(cb)
                                    .target(QueryTarget::All)
                                    .consolidation(ConsolidationMode::None)
                                    .timeout(self.config.queries_timeout)
                                    .res_sync()
                            }).res_async().await
                            {
                                warn!("Query on {} for discovery messages failed: {}", key, e);
                            }
                            // make all QueryingSubscriber to query this new member
                            for (zkey, route) in &mut self.routes_to_dds {
                                route.query_historical_publications(|| (*KE_PREFIX_PUB_CACHE / ke_for_sure!(mid) / zkey).into(), self.config.queries_timeout).await;
                            }
                        }
                        Ok(SampleKind::Delete) => {
                            let mid = member_id!(group_event.as_ref().unwrap());
                            debug!("Remote zenoh_dds_plugin left: {}", mid);
                            // remove all the references to the plugin's enities, removing no longer used routes
                            // and updating/re-publishing ParticipantEntitiesInfo
                            let admin_space = &mut self.admin_space;
                            let admin_subke = format!("@/service/{mid}/dds/");
                            let mut participant_info_changed = false;
                            self.routes_to_dds.retain(|zkey, route| {
                                route.remove_remote_routed_writers_containing(&admin_subke);
                                if !route.has_remote_routed_writer() {
                                    info!(
                                        "{}: remove it as no longer unused (no remote DDS Writer left)",
                                        route
                                    );
                                    let ke = *KE_PREFIX_ROUTE_TO_DDS / zkey;
                                    admin_space.remove(&ke);
                                    if let Ok(guid) = route.dds_writer_guid() {
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
                                route.remove_remote_routed_readers_containing(&admin_subke);
                                if !route.has_remote_routed_reader() {
                                    info!(
                                        "{}: remove it as no longer unused (no remote DDS Reader left)",
                                        route
                                    );
                                    let ke = *KE_PREFIX_ROUTE_FROM_DDS / zkey;
                                    admin_space.remove(&ke);
                                    if let Ok(guid) = route.dds_reader_guid() {
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
                        trace!("Received ros_discovery_info from DDS for {}, forward via zenoh: {}", gid, hex::encode(buf.as_slice()));
                        // forward the payload on zenoh
                        let ke = &fwd_ros_discovery_key_declared / ke_for_sure!(&gid);
                        if let Err(e) = self.zsession.put(ke, buf.as_slice()).res_sync() {
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
                // find a RouteDDSZenoh routing a remote reader with this gid
                match self
                    .routes_from_dds
                    .values()
                    .find(|route| route.is_routing_remote_reader(&node.reader_gid_seq[i]))
                {
                    Some(route) => {
                        // replace the gid with route's reader's gid
                        if let Ok(gid) = route.dds_reader_guid() {
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
                match self
                    .routes_to_dds
                    .values()
                    .find(|route| route.is_routing_remote_writer(&node.writer_gid_seq[i]))
                {
                    Some(route) => {
                        // replace the gid with route's writer's gid
                        if let Ok(gid) = route.dds_writer_guid() {
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

// Copy and adapt Writer's QoS for creation of a matching Reader
fn adapt_writer_qos_for_reader(qos: &Qos) -> Qos {
    let mut reader_qos = qos.clone();

    // Unset any writer QoS that doesn't apply to data readers
    reader_qos.durability_service = None;
    reader_qos.ownership_strength = None;
    reader_qos.transport_priority = None;
    reader_qos.lifespan = None;
    reader_qos.writer_data_lifecycle = None;
    reader_qos.writer_batching = None;

    // Unset proprietary QoS which shouldn't apply
    reader_qos.properties = None;
    reader_qos.entity_name = None;

    // Ignore own messages
    reader_qos.ignore_local = Some(IgnoreLocal {
        kind: IgnoreLocalKind::PARTICIPANT,
    });

    // Set default Reliability QoS if not set for writer
    if reader_qos.reliability.is_none() {
        reader_qos.reliability = Some({
            Reliability {
                kind: ReliabilityKind::BEST_EFFORT,
                max_blocking_time: DDS_100MS_DURATION,
            }
        });
    }

    reader_qos
}

// Copy and adapt Writer's QoS for creation of a proxy Writer
fn adapt_writer_qos_for_proxy_writer(qos: &Qos) -> Qos {
    let mut writer_qos = qos.clone();

    // Unset proprietary QoS which shouldn't apply
    writer_qos.properties = None;
    writer_qos.entity_name = None;

    // Ignore own messages
    writer_qos.ignore_local = Some(IgnoreLocal {
        kind: IgnoreLocalKind::PARTICIPANT,
    });

    writer_qos
}

// Copy and adapt Reader's QoS for creation of a matching Writer
fn adapt_reader_qos_for_writer(qos: &Qos) -> Qos {
    let mut writer_qos = qos.clone();

    // Unset any reader QoS that doesn't apply to data writers
    writer_qos.time_based_filter = None;
    writer_qos.reader_data_lifecycle = None;
    writer_qos.properties = None;
    writer_qos.entity_name = None;

    // Ignore own messages
    writer_qos.ignore_local = Some(IgnoreLocal {
        kind: IgnoreLocalKind::PARTICIPANT,
    });

    // if Reader is TRANSIENT_LOCAL, configure durability_service QoS with same history as the Reader.
    // This is because CycloneDDS is actually using durability_service.history for transient_local historical data.
    if is_transient_local(qos) {
        let history = qos
            .history
            .as_ref()
            .map_or(History::default(), |history| history.clone());

        writer_qos.durability_service = Some(DurabilityService {
            service_cleanup_delay: 60 * DDS_1S_DURATION,
            history_kind: history.kind,
            history_depth: history.depth,
            max_samples: DDS_LENGTH_UNLIMITED,
            max_instances: DDS_LENGTH_UNLIMITED,
            max_samples_per_instance: DDS_LENGTH_UNLIMITED,
        });
    }
    // Workaround for the DDS Writer to correctly match with a FastRTPS Reader
    writer_qos.reliability = match writer_qos.reliability {
        Some(mut reliability) => {
            reliability.max_blocking_time = reliability.max_blocking_time.saturating_add(1);
            Some(reliability)
        }
        _ => {
            let mut reliability = Reliability::default();
            reliability.max_blocking_time = reliability.max_blocking_time.saturating_add(1);
            Some(reliability)
        }
    };

    writer_qos
}

// Copy and adapt Reader's QoS for creation of a proxy Reader
fn adapt_reader_qos_for_proxy_reader(qos: &Qos) -> Qos {
    let mut reader_qos = qos.clone();

    // Unset proprietary QoS which shouldn't apply
    reader_qos.properties = None;
    reader_qos.entity_name = None;

    // Ignore own messages
    reader_qos.ignore_local = Some(IgnoreLocal {
        kind: IgnoreLocalKind::PARTICIPANT,
    });

    reader_qos
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
