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
use crate::qos::{HistoryKind, Qos};
use async_std::task;
use cyclors::*;
use flume::Sender;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::os::raw;
use std::sync::Arc;
use std::time::Duration;
use zenoh::buffers::ZBuf;
use zenoh::prelude::*;
use zenoh::publication::CongestionControl;
use zenoh::Session;
use zenoh_core::SyncResolve;

const MAX_SAMPLES: u32 = 32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum RouteStatus {
    Routed(OwnedKeyExpr), // Routing is active, with the zenoh key expression used for the route
    NotAllowed,           // Routing was not allowed per configuration
    CreationFailure(String), // The route creation failed
    _QoSConflict,         // A route was already established but with conflicting QoS
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DdsEntity {
    pub(crate) key: String,
    pub(crate) participant_key: String,
    pub(crate) topic_name: String,
    pub(crate) type_name: String,
    pub(crate) keyless: bool,
    pub(crate) qos: Qos,
    pub(crate) routes: HashMap<String, RouteStatus>, // map of routes statuses indexed by partition ("*" only if no partition)
}

#[derive(Debug)]
pub(crate) enum DiscoveryEvent {
    DiscoveredPublication { entity: DdsEntity },
    UndiscoveredPublication { key: String },
    DiscoveredSubscription { entity: DdsEntity },
    UndiscoveredSubscription { key: String },
}

unsafe extern "C" fn on_data(dr: dds_entity_t, arg: *mut std::os::raw::c_void) {
    let btx = Box::from_raw(arg as *mut (bool, Sender<DiscoveryEvent>));
    let pub_discovery: bool = btx.0;
    let dp = dds_get_participant(dr);
    let mut dpih: dds_instance_handle_t = 0;
    let _ = dds_get_instance_handle(dp, &mut dpih);

    #[allow(clippy::uninit_assumed_init)]
    let mut si = MaybeUninit::<[dds_sample_info_t; MAX_SAMPLES as usize]>::uninit();
    let mut samples: [*mut ::std::os::raw::c_void; MAX_SAMPLES as usize] =
        [std::ptr::null_mut(); MAX_SAMPLES as usize];
    samples[0] = std::ptr::null_mut();

    let n = dds_take(
        dr,
        samples.as_mut_ptr() as *mut *mut raw::c_void,
        si.as_mut_ptr() as *mut dds_sample_info_t,
        MAX_SAMPLES.into(),
        MAX_SAMPLES,
    );
    let si = si.assume_init();

    for i in 0..n {
        let sample = samples[i as usize] as *mut dds_builtintopic_endpoint_t;
        if (*sample).participant_instance_handle == dpih {
            // Ignore discovery of entities created by our own participant
            continue;
        }
        let is_alive = si[i as usize].instance_state == dds_instance_state_DDS_IST_ALIVE;
        let key = hex::encode((*sample).key.v);

        if is_alive {
            let topic_name = match CStr::from_ptr((*sample).topic_name).to_str() {
                Ok(s) => s,
                Err(e) => {
                    warn!("Discovery of an invalid topic name: {}", e);
                    continue;
                }
            };
            if topic_name.starts_with("DCPS") {
                debug!(
                    "Ignoring discovery of {} ({} is a builtin topic)",
                    key, topic_name
                );
                continue;
            }

            let type_name = match CStr::from_ptr((*sample).type_name).to_str() {
                Ok(s) => s,
                Err(e) => {
                    warn!("Discovery of an invalid topic type: {}", e);
                    continue;
                }
            };
            let participant_key = hex::encode((*sample).participant_key.v);
            let keyless = (*sample).key.v[15] == 3 || (*sample).key.v[15] == 4;

            debug!(
                "Discovered DDS {} {} from Participant {} on {} with type {} (keyless: {})",
                if pub_discovery {
                    "publication"
                } else {
                    "subscription"
                },
                key,
                participant_key,
                topic_name,
                type_name,
                keyless
            );

            let qos = if pub_discovery {
                Qos::from_writer_qos_native((*sample).qos)
            } else {
                Qos::from_reader_qos_native((*sample).qos)
            };

            // send a DiscoveryEvent
            let entity = DdsEntity {
                key: key.clone(),
                participant_key: participant_key.clone(),
                topic_name: String::from(topic_name),
                type_name: String::from(type_name),
                keyless,
                qos,
                routes: HashMap::<String, RouteStatus>::new(),
            };

            if pub_discovery {
                send_discovery_event(&btx.1, DiscoveryEvent::DiscoveredPublication { entity });
            } else {
                send_discovery_event(&btx.1, DiscoveryEvent::DiscoveredSubscription { entity });
            }
        } else if pub_discovery {
            send_discovery_event(&btx.1, DiscoveryEvent::UndiscoveredPublication { key });
        } else {
            send_discovery_event(&btx.1, DiscoveryEvent::UndiscoveredSubscription { key });
        }
    }
    dds_return_loan(
        dr,
        samples.as_mut_ptr() as *mut *mut raw::c_void,
        MAX_SAMPLES as i32,
    );
    Box::into_raw(btx);
}

fn send_discovery_event(sender: &Sender<DiscoveryEvent>, event: DiscoveryEvent) {
    if let Err(e) = sender.try_send(event) {
        error!(
            "INTERNAL ERROR sending DiscoveryEvent to internal channel: {:?}",
            e
        );
    }
}

pub(crate) fn run_discovery(dp: dds_entity_t, tx: Sender<DiscoveryEvent>) {
    unsafe {
        let ptx = Box::new((true, tx.clone()));
        let stx = Box::new((false, tx));
        let sub_listener = dds_create_listener(Box::into_raw(ptx) as *mut std::os::raw::c_void);
        dds_lset_data_available(sub_listener, Some(on_data));

        let _pr = dds_create_reader(
            dp,
            DDS_BUILTIN_TOPIC_DCPSPUBLICATION,
            std::ptr::null(),
            sub_listener,
        );

        let sub_listener = dds_create_listener(Box::into_raw(stx) as *mut std::os::raw::c_void);
        dds_lset_data_available(sub_listener, Some(on_data));
        let _sr = dds_create_reader(
            dp,
            DDS_BUILTIN_TOPIC_DCPSSUBSCRIPTION,
            std::ptr::null(),
            sub_listener,
        );
    }
}

unsafe extern "C" fn data_forwarder_listener(dr: dds_entity_t, arg: *mut std::os::raw::c_void) {
    let pa = arg as *mut (String, KeyExpr, Arc<Session>, CongestionControl);
    let mut zp: *mut cdds_ddsi_payload = std::ptr::null_mut();
    #[allow(clippy::uninit_assumed_init)]
    let mut si = MaybeUninit::<[dds_sample_info_t; 1]>::uninit();
    while cdds_take_blob(dr, &mut zp, si.as_mut_ptr() as *mut dds_sample_info_t) > 0 {
        let si = si.assume_init();
        if si[0].valid_data {
            let bs = Vec::from_raw_parts((*zp).payload, (*zp).size as usize, (*zp).size as usize);
            let rbuf = ZBuf::from(bs);
            if *crate::LOG_PAYLOAD {
                log::trace!(
                    "Route data from DDS {} to zenoh key={} - payload: {:?}",
                    &(*pa).0,
                    &(*pa).1,
                    rbuf
                );
            } else {
                log::trace!("Route data from DDS {} to zenoh key={}", &(*pa).0, &(*pa).1);
            }
            let _ = (*pa)
                .2
                .put(&(*pa).1, rbuf)
                .congestion_control((*pa).3)
                .res_sync();
            (*zp).payload = std::ptr::null_mut();
        }
        cdds_serdata_unref(zp as *mut ddsi_serdata);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn create_forwarding_dds_reader(
    dp: dds_entity_t,
    topic_name: String,
    type_name: String,
    keyless: bool,
    mut qos: Qos,
    z_key: KeyExpr,
    z: Arc<Session>,
    read_period: Option<Duration>,
    congestion_ctrl: CongestionControl,
) -> Result<dds_entity_t, String> {
    let cton = CString::new(topic_name.clone()).unwrap().into_raw();
    let ctyn = CString::new(type_name).unwrap().into_raw();

    unsafe {
        let t = cdds_create_blob_topic(dp, cton, ctyn, keyless);

        match read_period {
            None => {
                // Use a Listener to route data as soon as it arrives
                let arg = Box::new((topic_name, z_key, z, congestion_ctrl));
                let sub_listener =
                    dds_create_listener(Box::into_raw(arg) as *mut std::os::raw::c_void);
                dds_lset_data_available(sub_listener, Some(data_forwarder_listener));
                let qos_native = qos.to_qos_native();
                let reader = dds_create_reader(dp, t, qos_native, sub_listener);
                Qos::delete_qos_native(qos_native);
                if reader >= 0 {
                    let res =
                        dds_reader_wait_for_historical_data(reader, crate::qos::DDS_100MS_DURATION);
                    if res < 0 {
                        log::error!(
                            "Error calling dds_reader_wait_for_historical_data(): {}",
                            CStr::from_ptr(dds_strretcode(-res))
                                .to_str()
                                .unwrap_or("unrecoverable DDS retcode")
                        );
                    }
                    Ok(reader)
                } else {
                    Err(format!(
                        "Error creating DDS Reader: {}",
                        CStr::from_ptr(dds_strretcode(-reader))
                            .to_str()
                            .unwrap_or("unrecoverable DDS retcode")
                    ))
                }
            }
            Some(period) => {
                // Use a periodic task that takes data to route from a Reader with KEEP_LAST 1
                qos.history.kind = HistoryKind::KEEP_LAST;
                qos.history.depth = 1;
                let qos_native = qos.to_qos_native();
                let reader = dds_create_reader(dp, t, qos_native, std::ptr::null());
                let z_key = z_key.into_owned();
                task::spawn(async move {
                    // loop while reader's instance handle remain the same
                    // (if reader was deleted, its dds_entity_t value might have been
                    // reused by a new entity... don't trust it! Only trust instance handle)
                    let mut original_handle: dds_instance_handle_t = 0;
                    dds_get_instance_handle(reader, &mut original_handle);
                    let mut handle: dds_instance_handle_t = 0;
                    while dds_get_instance_handle(reader, &mut handle) == DDS_RETCODE_OK as i32 {
                        if handle != original_handle {
                            break;
                        }

                        async_std::task::sleep(period).await;
                        let mut zp: *mut cdds_ddsi_payload = std::ptr::null_mut();
                        #[allow(clippy::uninit_assumed_init)]
                        let mut si = MaybeUninit::<[dds_sample_info_t; 1]>::uninit();
                        while cdds_take_blob(
                            reader,
                            &mut zp,
                            si.as_mut_ptr() as *mut dds_sample_info_t,
                        ) > 0
                        {
                            let si = si.assume_init();
                            if si[0].valid_data {
                                log::trace!(
                                    "Route (periodic) data to zenoh resource with rid={}",
                                    z_key
                                );
                                let bs = Vec::from_raw_parts(
                                    (*zp).payload,
                                    (*zp).size as usize,
                                    (*zp).size as usize,
                                );
                                let rbuf = ZBuf::from(bs);
                                let _ = z
                                    .put(&z_key, rbuf)
                                    .congestion_control(congestion_ctrl)
                                    .res_sync();
                                (*zp).payload = std::ptr::null_mut();
                            }
                            cdds_serdata_unref(zp as *mut ddsi_serdata);
                        }
                    }
                });
                Ok(reader)
            }
        }
    }
}

pub fn create_forwarding_dds_writer(
    dp: dds_entity_t,
    topic_name: String,
    type_name: String,
    keyless: bool,
    qos: Qos,
) -> Result<dds_entity_t, String> {
    let cton = CString::new(topic_name).unwrap().into_raw();
    let ctyn = CString::new(type_name).unwrap().into_raw();

    unsafe {
        let t = cdds_create_blob_topic(dp, cton, ctyn, keyless);
        let qos_native = qos.to_qos_native();
        let writer = dds_create_writer(dp, t, qos_native, std::ptr::null_mut());
        Qos::delete_qos_native(qos_native);
        if writer >= 0 {
            Ok(writer)
        } else {
            Err(format!(
                "Error creating DDS Writer: {}",
                CStr::from_ptr(dds_strretcode(-writer))
                    .to_str()
                    .unwrap_or("unrecoverable DDS retcode")
            ))
        }
    }
}

pub fn delete_dds_entity(entity: dds_entity_t) -> Result<(), String> {
    unsafe {
        let r = dds_delete(entity);
        match r {
            0 | DDS_RETCODE_ALREADY_DELETED => Ok(()),
            e => Err(format!("Error deleting DDS entity - retcode={e}")),
        }
    }
}

pub fn get_guid(entity: &dds_entity_t) -> Result<String, String> {
    unsafe {
        let mut guid = dds_guid_t { v: [0; 16] };
        let r = dds_get_guid(*entity, &mut guid);
        if r == 0 {
            Ok(hex::encode(guid.v))
        } else {
            Err(format!("Error getting GUID of DDS entity - retcode={r}"))
        }
    }
}

pub fn serialize_entity_guid<S>(entity: &dds_entity_t, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match get_guid(entity) {
        Ok(guid) => s.serialize_str(&guid),
        Err(_) => s.serialize_str("UNKOWN_GUID"),
    }
}
