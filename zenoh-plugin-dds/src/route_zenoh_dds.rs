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

use std::{
    collections::HashSet,
    ffi::CStr,
    fmt,
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
    time::Duration,
};

use cyclors::{
    dds_entity_t, dds_get_entity_sertype, dds_strretcode, dds_writecdr, ddsi_serdata_from_ser_iov,
    ddsi_serdata_kind_SDK_DATA, ddsi_sertype, ddsrt_iovec_t,
};
use serde::{Serialize, Serializer};
use zenoh::{
    key_expr::{keyexpr, KeyExpr, OwnedKeyExpr},
    pubsub::Subscriber,
    query::{ConsolidationMode, QueryTarget, ReplyKeyExpr, Selector},
    sample::{Locality, Sample},
    Session, Wait,
};
use zenoh_ext::{FetchingSubscriber, SubscriberBuilderExt};

use crate::{
    dds_mgt::*, qos::Qos, vec_into_raw_parts, DdsPluginRuntime, KE_ANY_1_SEGMENT,
    KE_PREFIX_PUB_CACHE, LOG_PAYLOAD,
};

type AtomicDDSEntity = AtomicI32;
const DDS_ENTITY_NULL: dds_entity_t = 0;

enum ZSubscriber {
    Subscriber(Subscriber<()>),
    FetchingSubscriber(FetchingSubscriber<()>),
}

impl ZSubscriber {
    fn key_expr(&self) -> &KeyExpr<'static> {
        match self {
            ZSubscriber::Subscriber(s) => s.key_expr(),
            ZSubscriber::FetchingSubscriber(s) => s.key_expr(),
        }
    }
}

// a route from Zenoh to DDS
#[allow(clippy::upper_case_acronyms)]
#[derive(Serialize)]
pub(crate) struct RouteZenohDDS<'a> {
    // the zenoh session
    #[serde(skip)]
    zenoh_session: &'a Arc<Session>,
    // the zenoh subscriber receiving data to be re-published by the DDS Writer
    #[serde(skip)]
    zenoh_subscriber: ZSubscriber,
    // the DDS topic name for re-publication
    topic_name: String,
    // the DDS topic type
    topic_type: String,
    // is DDS topic keyess
    keyless: bool,
    // the local DDS Writer created to serve the route (i.e. re-publish to DDS data coming from zenoh)
    // can be DDS_ENTITY_NULL in "forward discovery" mode, when the route is created because of the discovery
    // of a local DDS Reader, and the forwarded discovery msg for the DDS Writer didn't arrive yet.
    #[serde(serialize_with = "serialize_atomic_entity_guid")]
    dds_writer: Arc<AtomicDDSEntity>,
    // the list of remote writers served by this route (admin key expr)
    remote_routed_writers: HashSet<OwnedKeyExpr>,
    // the list of local readers served by this route (entity keys)
    local_routed_readers: HashSet<String>,
}

impl Drop for RouteZenohDDS<'_> {
    fn drop(&mut self) {
        self.delete_dds_writer();
    }
}

impl fmt::Display for RouteZenohDDS<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Route Zenoh->DDS ({} -> {})",
            self.zenoh_subscriber.key_expr(),
            self.topic_name
        )
    }
}

fn serialize_atomic_entity_guid<S>(entity: &Arc<AtomicDDSEntity>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match entity.load(std::sync::atomic::Ordering::Relaxed) {
        DDS_ENTITY_NULL => s.serialize_str(""),
        entity => serialize_entity_guid(&entity, s),
    }
}

impl RouteZenohDDS<'_> {
    pub(crate) async fn new<'a>(
        plugin: &DdsPluginRuntime<'a>,
        ke: OwnedKeyExpr,
        querying_subscriber: bool,
        topic_name: String,
        topic_type: String,
        keyless: bool,
    ) -> Result<RouteZenohDDS<'a>, String> {
        tracing::debug!(
            "Route Zenoh->DDS ({} -> {}): creation with topic_type={} querying_subscriber={}",
            ke,
            topic_name,
            topic_type,
            querying_subscriber
        );

        // Initiate an Arc<AtomicDDSEntity> to DDS_ENTITY_NULL for the DDS Writer
        let dds_writer = Arc::new(AtomicDDSEntity::from(DDS_ENTITY_NULL));
        // Clone it for the subscriber_callback
        let arc_dw = dds_writer.clone();

        // Callback routing data received by Zenoh subscriber to DDS Writer (if set)
        let ton = topic_name.clone();
        let subscriber_callback = move |s: Sample| {
            let dw = arc_dw.load(Ordering::Relaxed);
            if dw != DDS_ENTITY_NULL {
                do_route_data(s, &ton, dw);
            } else {
                // delay the routing of data for few ms in case this publication arrived
                // before the discovery message provoking the creation of the Data Writer
                tracing::debug!(
                    "Route Zenoh->DDS ({} -> {}): data arrived but no DDS Writer yet to route it... wait 3s for discovery forwarding msg",
                    s.key_expr(),
                    &ton
                );
                let arc_dw2 = arc_dw.clone();
                let ton2 = ton.clone();
                let ke = s.key_expr().clone();
                tokio::task::spawn(async move {
                    for _ in 1..30 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let dw = arc_dw2.load(Ordering::Relaxed);
                        if dw != DDS_ENTITY_NULL {
                            do_route_data(s, &ton2, dw);
                            break;
                        } else {
                            tracing::warn!(
                                "Route Zenoh->DDS ({} -> {}): still no DDS Writer after 3s - drop incoming data!",
                                ke,
                                &ton2
                            );
                        }
                    }
                });
            }
        };

        // create zenoh subscriber
        let zenoh_subscriber = if querying_subscriber {
            // query all PublicationCaches on "<routing_keyexpr>/<KE_PREFIX_PUB_CACHE>/*"
            let query_selector: Selector = (&ke / *KE_PREFIX_PUB_CACHE / *KE_ANY_1_SEGMENT).into();
            tracing::debug!(
                    "Route Zenoh->DDS ({} -> {}): query historical data from everybody for TRANSIENT_LOCAL Reader on {}",
                    ke,
                    topic_name,
                    query_selector
                );

            let sub = plugin
                .zsession
                .declare_subscriber(ke.clone())
                .callback(subscriber_callback)
                .allowed_origin(Locality::Remote) // Allow only remote publications to avoid loops
                .querying()
                .query_timeout(plugin.config.queries_timeout)
                .query_selector(query_selector)
                .query_accept_replies(ReplyKeyExpr::Any)
                .await
                .map_err(|e| {
                    format!(
                        "Route Zenoh->DDS ({ke} -> {topic_name}): failed to create FetchingSubscriber: {e}"
                    )
                })?;
            ZSubscriber::FetchingSubscriber(sub)
        } else {
            let sub = plugin
                .zsession
                .declare_subscriber(ke.clone())
                .callback(subscriber_callback)
                .allowed_origin(Locality::Remote) // Allow only remote publications to avoid loops
                .await
                .map_err(|e| {
                    format!(
                        "Route Zenoh->DDS ({ke} -> {topic_name}): failed to create Subscriber: {e}"
                    )
                })?;
            ZSubscriber::Subscriber(sub)
        };

        Ok(RouteZenohDDS {
            zenoh_session: plugin.zsession,
            zenoh_subscriber,
            topic_name,
            topic_type,
            keyless,
            dds_writer,
            remote_routed_writers: HashSet::new(),
            local_routed_readers: HashSet::new(),
        })
    }

    pub(crate) fn set_dds_writer(
        &self,
        data_participant: dds_entity_t,
        type_info: &Option<TypeInfo>,
        writer_qos: Qos,
    ) -> Result<(), String> {
        // check if dds_writer was already set
        let old = self.dds_writer.load(Ordering::SeqCst);

        if old == DDS_ENTITY_NULL {
            tracing::debug!("{}: create DDS Writer", self);
            let dw = create_forwarding_dds_writer(
                data_participant,
                self.topic_name.clone(),
                self.topic_type.clone(),
                type_info,
                self.keyless,
                writer_qos,
            )?;
            if self
                .dds_writer
                .compare_exchange(DDS_ENTITY_NULL, dw, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                // another task managed to create the DDS Writer before this task
                // delete the DDS Writer created here
                tracing::debug!(
                    "{}: delete DDS Writer since another task created one concurrently",
                    self
                );
                if let Err(e) = delete_dds_entity(dw) {
                    tracing::warn!(
                        "{}: failed to delete DDS Writer created in concurrence of another task: {}",
                        self, e
                    )
                }
            }
        }
        Ok(())
    }

    pub(crate) fn delete_dds_writer(&self) {
        let dds_entity = self
            .dds_writer
            .swap(DDS_ENTITY_NULL, std::sync::atomic::Ordering::Relaxed);
        if dds_entity != DDS_ENTITY_NULL {
            if let Err(e) = delete_dds_entity(dds_entity) {
                tracing::warn!("{}: error deleting DDS Writer:  {}", self, e);
            }
        }
    }

    /// If this route uses a FetchingSubscriber, query for historical publications
    /// using the specified Selector. Otherwise, do nothing.
    pub(crate) async fn query_historical_publications<'a, F>(
        &mut self,
        selector: F,
        query_timeout: Duration,
    ) where
        F: Fn() -> Selector<'a>,
    {
        if let ZSubscriber::FetchingSubscriber(sub) = &mut self.zenoh_subscriber {
            let s = selector();
            tracing::debug!(
                "Route Zenoh->DDS ({} -> {}): query historical publications from {}",
                sub.key_expr(),
                self.topic_name,
                s
            );
            if let Err(e) = sub
                .fetch({
                    let session = &self.zenoh_session;
                    let s = s.clone();
                    move |cb| {
                        session
                            .get(&s)
                            .target(QueryTarget::All)
                            .consolidation(ConsolidationMode::None)
                            .accept_replies(ReplyKeyExpr::Any)
                            .timeout(query_timeout)
                            .callback(cb)
                            .wait()
                    }
                })
                .await
            {
                tracing::warn!(
                    "{}: query for historical publications on {} failed: {}",
                    self,
                    s,
                    e
                );
            }
        }
    }

    pub(crate) fn dds_writer_guid(&self) -> Result<String, String> {
        get_guid(&self.dds_writer.load(Ordering::Relaxed))
    }

    pub(crate) fn add_remote_routed_writer(&mut self, admin_ke: OwnedKeyExpr) {
        self.remote_routed_writers.insert(admin_ke);
    }

    pub(crate) fn remove_remote_routed_writer(&mut self, admin_ke: &keyexpr) {
        self.remote_routed_writers.remove(admin_ke);
    }

    /// Remove all Writers reference with admin keyexpr containing "sub_ke"
    pub(crate) fn remove_remote_routed_writers_containing(&mut self, sub_ke: &str) {
        self.remote_routed_writers.retain(|s| !s.contains(sub_ke));
    }

    pub(crate) fn has_remote_routed_writer(&self) -> bool {
        !self.remote_routed_writers.is_empty()
    }

    pub(crate) fn is_routing_remote_writer(&self, entity_key: &str) -> bool {
        self.remote_routed_writers
            .iter()
            .any(|s| s.contains(entity_key))
    }

    pub(crate) fn add_local_routed_reader(&mut self, entity_key: String) {
        self.local_routed_readers.insert(entity_key);
    }

    pub(crate) fn remove_local_routed_reader(&mut self, entity_key: &str) {
        self.local_routed_readers.remove(entity_key);
    }

    pub(crate) fn has_local_routed_reader(&self) -> bool {
        !self.local_routed_readers.is_empty()
    }
}

fn do_route_data(s: Sample, topic_name: &str, data_writer: dds_entity_t) {
    if *LOG_PAYLOAD {
        tracing::trace!(
            "Route Zenoh->DDS ({} -> {}): routing data - payload: {:?}",
            s.key_expr(),
            &topic_name,
            s.payload()
        );
    } else {
        tracing::trace!(
            "Route Zenoh->DDS ({} -> {}): routing data",
            s.key_expr(),
            &topic_name
        );
    }

    unsafe {
        let bs = s.payload().to_bytes().to_vec();
        // As per the Vec documentation (see https://doc.rust-lang.org/std/vec/struct.Vec.html#method.into_raw_parts)
        // the only way to correctly releasing it is to create a vec using from_raw_parts
        // and then have its destructor do the cleanup.
        // Thus, while tempting to just pass the raw pointer to cyclone and then free it from C,
        // that is not necessarily safe or guaranteed to be leak free.
        // TODO replace when stable https://github.com/rust-lang/rust/issues/65816
        let (ptr, len, capacity) = vec_into_raw_parts(bs);

        let data_out: ddsrt_iovec_t;
        #[cfg(not(target_os = "windows"))]
        {
            data_out = ddsrt_iovec_t {
                iov_base: ptr as *mut std::ffi::c_void,
                iov_len: len,
            };
        }
        #[cfg(target_os = "windows")]
        {
            data_out = ddsrt_iovec_t {
                iov_base: ptr as *mut std::ffi::c_void,
                iov_len: len as u32,
            };
        }

        let mut sertype_ptr: *const ddsi_sertype = std::ptr::null_mut();
        let ret = dds_get_entity_sertype(data_writer, &mut sertype_ptr);
        if ret < 0 {
            tracing::warn!(
                "Route Zenoh->DDS ({} -> {}): can't route data; sertype lookup failed ({})",
                s.key_expr(),
                topic_name,
                CStr::from_ptr(dds_strretcode(ret))
                    .to_str()
                    .unwrap_or("unrecoverable DDS retcode")
            );
            return;
        }

        let fwdp =
            ddsi_serdata_from_ser_iov(sertype_ptr, ddsi_serdata_kind_SDK_DATA, 1, &data_out, len);

        dds_writecdr(data_writer, fwdp);
        drop(Vec::from_raw_parts(ptr, len, capacity));
    }
}
