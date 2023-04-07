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
use crate::dds_mgt::delete_dds_entity;
use crate::qos::{Durability, History, Qos, Reliability, DDS_INFINITE_TIME};
use cdr::{CdrLe, Infinite};
use cyclors::*;
use log::warn;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::convert::TryInto;
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    mem::MaybeUninit,
};
use zenoh::buffers::ZBuf;

pub(crate) const ROS_DISCOVERY_INFO_TOPIC_NAME: &str = "ros_discovery_info";
const ROS_DISCOVERY_INFO_TOPIC_TYPE: &str = "rmw_dds_common::msg::dds_::ParticipantEntitiesInfo_";

pub(crate) struct RosDiscoveryInfoMgr {
    participant: dds_entity_t,
    reader: dds_entity_t,
    writer: dds_entity_t,
}

impl Drop for RosDiscoveryInfoMgr {
    fn drop(&mut self) {
        if let Err(e) = delete_dds_entity(self.reader) {
            warn!(
                "Error dropping DDS reader on {}: {}",
                ROS_DISCOVERY_INFO_TOPIC_NAME, e
            );
        }
        if let Err(e) = delete_dds_entity(self.writer) {
            warn!(
                "Error dropping DDS writer on {}: {}",
                ROS_DISCOVERY_INFO_TOPIC_NAME, e
            );
        }
    }
}

impl RosDiscoveryInfoMgr {
    pub(crate) fn create(participant: dds_entity_t) -> Result<RosDiscoveryInfoMgr, String> {
        let cton = CString::new(ROS_DISCOVERY_INFO_TOPIC_NAME)
            .unwrap()
            .into_raw();
        let ctyn = CString::new(ROS_DISCOVERY_INFO_TOPIC_TYPE)
            .unwrap()
            .into_raw();

        unsafe {
            // Create topic (for reader/writer creation)
            let t = cdds_create_blob_topic(participant, cton, ctyn, true);

            // Create reader
            let mut qos = Qos::default();
            qos.reliability = Reliability {
                kind: crate::qos::ReliabilityKind::RELIABLE,
                max_blocking_time: DDS_INFINITE_TIME,
            };
            qos.durability = Durability {
                kind: crate::qos::DurabilityKind::TRANSIENT_LOCAL,
            };
            // Note: KEEP_ALL to not loose any sample (topic is keyless). A periodic task should take samples from history.
            qos.history = History {
                kind: crate::qos::HistoryKind::KEEP_ALL,
                depth: 0,
            };
            qos.ignore_local_participant = true;
            let qos_native = qos.to_qos_native();
            let reader = dds_create_reader(participant, t, qos_native, std::ptr::null());
            Qos::delete_qos_native(qos_native);
            if reader < 0 {
                return Err(format!(
                    "Error creating DDS Reader on {}: {}",
                    ROS_DISCOVERY_INFO_TOPIC_NAME,
                    CStr::from_ptr(dds_strretcode(-reader))
                        .to_str()
                        .unwrap_or("unrecoverable DDS retcode")
                ));
            }

            // Create writer
            let mut qos = Qos::default();
            qos.reliability = Reliability {
                kind: crate::qos::ReliabilityKind::RELIABLE,
                max_blocking_time: DDS_INFINITE_TIME,
            };
            qos.durability = Durability {
                kind: crate::qos::DurabilityKind::TRANSIENT_LOCAL,
            };
            qos.history = History {
                kind: crate::qos::HistoryKind::KEEP_LAST,
                depth: 1,
            };
            qos.ignore_local_participant = true;
            let qos_native = qos.to_qos_native();
            let writer = dds_create_writer(participant, t, qos_native, std::ptr::null());
            Qos::delete_qos_native(qos_native);
            if writer < 0 {
                return Err(format!(
                    "Error creating DDS Writer on {}: {}",
                    ROS_DISCOVERY_INFO_TOPIC_NAME,
                    CStr::from_ptr(dds_strretcode(-writer))
                        .to_str()
                        .unwrap_or("unrecoverable DDS retcode")
                ));
            }

            drop(CString::from_raw(cton));
            drop(CString::from_raw(ctyn));

            Ok(RosDiscoveryInfoMgr {
                participant,
                reader,
                writer,
            })
        }
    }

    pub(crate) fn read(&self) -> HashMap<String, ZBuf> {
        unsafe {
            let mut zp: *mut cdds_ddsi_payload = std::ptr::null_mut();
            #[allow(clippy::uninit_assumed_init)]
            let mut si = MaybeUninit::<[dds_sample_info_t; 1]>::uninit();
            // Place read samples into a map indexed by Participant gid. Thus we only keep the last update for each
            let mut result: HashMap<String, ZBuf> = HashMap::new();
            while cdds_take_blob(
                self.reader,
                &mut zp,
                si.as_mut_ptr() as *mut dds_sample_info_t,
            ) > 0
            {
                let si = si.assume_init();
                if si[0].valid_data {
                    let bs = Vec::from_raw_parts(
                        (*zp).payload,
                        (*zp).size as usize,
                        (*zp).size as usize,
                    );
                    // No need to deserialize the full payload. Just read the Participant gid (16 bytes after the 4 bytes of CDR header)
                    let gid = hex::encode(&bs[4..20]);
                    let buf = ZBuf::from(bs);
                    result.insert(gid, buf);

                    (*zp).payload = std::ptr::null_mut();
                }
                cdds_serdata_unref(zp as *mut ddsi_serdata);
            }
            result
        }
    }

    pub(crate) fn write(&self, info: &ParticipantEntitiesInfo) -> Result<(), String> {
        unsafe {
            let buf = cdr::serialize::<_, _, CdrLe>(info, Infinite)
                .map_err(|e| format!("Error serializing ParticipantEntitiesInfo: {e}"))?;

            // create sertype (Unfortunatelly cdds_ddsi_payload_create() takes *mut ddsi_sertype. And keeping it in Self would make it not Send)
            let ctyn = CString::new(ROS_DISCOVERY_INFO_TOPIC_TYPE)
                .unwrap()
                .into_raw();
            let sertype = cdds_create_blob_sertype(self.participant, ctyn, true);
            drop(CString::from_raw(ctyn));

            // As per the Vec documentation (see https://doc.rust-lang.org/std/vec/struct.Vec.html#method.into_raw_parts)
            // the only way to correctly releasing it is to create a vec using from_raw_parts
            // and then have its destructor do the cleanup.
            // Thus, while tempting to just pass the raw pointer to cyclone and then free it from C,
            // that is not necessarily safe or guaranteed to be leak free.
            // TODO replace when stable https://github.com/rust-lang/rust/issues/65816
            let (ptr, len, capacity) = crate::vec_into_raw_parts(buf);
            let fwdp = cdds_ddsi_payload_create(
                sertype,
                ddsi_serdata_kind_SDK_DATA,
                ptr,
                len.try_into().map_err(|e| {
                    format!("Error creating payload for ParticipantEntitiesInfo: {e}")
                })?,
            );
            dds_writecdr(self.writer, fwdp as *mut ddsi_serdata);
            drop(Vec::from_raw_parts(ptr, len, capacity));
            cdds_sertype_unref(sertype);
            Ok(())
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct NodeEntitiesInfo {
    pub node_namespace: String,
    pub node_name: String,
    #[serde(
        serialize_with = "serialize_gids",
        deserialize_with = "deserialize_gids"
    )]
    pub reader_gid_seq: Vec<String>,
    #[serde(
        serialize_with = "serialize_gids",
        deserialize_with = "deserialize_gids"
    )]
    pub writer_gid_seq: Vec<String>,
}

impl std::fmt::Display for NodeEntitiesInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Node {}{} : {} pub / {} sub",
            self.node_namespace,
            self.node_name,
            self.reader_gid_seq.len(),
            self.writer_gid_seq.len()
        )?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct ParticipantEntitiesInfo {
    #[serde(serialize_with = "serialize_gid", deserialize_with = "deserialize_gid")]
    pub gid: String,
    #[serde(
        serialize_with = "serialize_node_entities_info_seq",
        deserialize_with = "deserialize_node_entities_info_seq"
    )]
    pub node_entities_info_seq: HashMap<String, NodeEntitiesInfo>,
}

impl ParticipantEntitiesInfo {
    pub(crate) fn new(gid: String) -> Self {
        ParticipantEntitiesInfo {
            gid,
            node_entities_info_seq: HashMap::new(),
        }
    }
}

impl std::fmt::Display for ParticipantEntitiesInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "participant {} : [", self.gid)?;
        for i in self.node_entities_info_seq.values() {
            write!(f, "({i}), ")?;
        }
        write!(f, "]")?;
        Ok(())
    }
}

impl ParticipantEntitiesInfo {
    // Update with a new map of NodeEntitiesInfo, and cleanup the possibly NodeEntitiesInfo (no readers, no writers)
    pub(crate) fn update_with(&mut self, nodes_entities: HashMap<String, NodeEntitiesInfo>) {
        self.node_entities_info_seq.extend(nodes_entities);
        self.cleanup();
    }

    pub(crate) fn remove_reader_gid(&mut self, reader_gid: &str) {
        for node in self.node_entities_info_seq.values_mut() {
            node.reader_gid_seq.retain(|gid| gid != reader_gid);
        }
    }

    pub(crate) fn remove_writer_gid(&mut self, writer_gid: &str) {
        for node in self.node_entities_info_seq.values_mut() {
            node.writer_gid_seq.retain(|gid| gid != writer_gid);
        }
    }

    // remove the empty NodeEntitiesInfo (no readers, no writers)
    pub(crate) fn cleanup(&mut self) {
        self.node_entities_info_seq
            .retain(|_, node| !node.reader_gid_seq.is_empty() && !node.writer_gid_seq.is_empty());
    }
}

fn serialize_gid<S>(gid: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut buf = hex::decode(gid).map_err(|e| {
        serde::ser::Error::custom(format!("Failed to decode gid {gid} as hex: {e}"))
    })?;
    // Gid size in ROS messages in 24 bytes (The DDS gid is usually 16 bytes). Resize the buffer
    buf.resize(24, 0);
    serde::Serialize::serialize(
        TryInto::<&[u8; 24]>::try_into(&buf[..24]).unwrap(),
        serializer,
    )
}

fn deserialize_gid<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let gid: [u8; 24] = Deserialize::deserialize(deserializer)?;
    // NOTE: a DDS gid is 16 bytes only. ignore the last 8 bytes
    Ok(hex::encode(&gid[..16]))
}

fn serialize_gids<S>(gids: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut seq = serializer.serialize_seq(Some(gids.len()))?;
    for s in gids {
        let mut buf = hex::decode(s).map_err(|e| {
            serde::ser::Error::custom(format!("Failed to decode gid {s} as hex: {e}"))
        })?;
        // Gid size in ROS messages in 24 bytes (The DDS gid is usually 16 bytes). Resize the buffer
        buf.resize(24, 0);
        seq.serialize_element(TryInto::<&[u8; 24]>::try_into(&buf[..24]).unwrap())?;
    }
    seq.end()
}

fn deserialize_gids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let gids: Vec<[u8; 24]> = Deserialize::deserialize(deserializer)?;
    // NOTE: a DDS gid is 16 bytes only. ignore the last 8 bytes
    Ok(gids.iter().map(|gid| hex::encode(&gid[..16])).collect())
}

fn serialize_node_entities_info_seq<S>(
    entities: &HashMap<String, NodeEntitiesInfo>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut seq = serializer.serialize_seq(Some(entities.len()))?;
    for entity in entities.values() {
        seq.serialize_element(entity)?;
    }
    seq.end()
}

fn deserialize_node_entities_info_seq<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, NodeEntitiesInfo>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut entities: Vec<NodeEntitiesInfo> = Deserialize::deserialize(deserializer)?;
    let mut map: HashMap<String, NodeEntitiesInfo> = HashMap::with_capacity(entities.len());
    for entity in entities.drain(..) {
        let key = format!("{}{}", entity.node_namespace, entity.node_name);
        map.insert(key, entity);
    }
    Ok(map)
}
