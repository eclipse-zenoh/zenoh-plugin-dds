use std::{
    ffi::{CStr, CString},
    os::raw::c_char,
};

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
use cyclors::*;
use derivative::Derivative;
use serde::{Deserialize, Serialize};

use crate::vec_into_raw_parts;

pub const DDS_INFINITE_TIME: i64 = 0x7FFFFFFFFFFFFFFF;
pub const DDS_100MS_DURATION: i64 = 100 * 1_000_000;
pub const DDS_1S_DURATION: i64 = 1_000_000_000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Qos {
    pub durability: Durability,
    pub durability_service: DurabilityService,
    pub reliability: Reliability,
    pub deadline: Deadline,
    pub latency_budget: LatencyBudget,
    pub ownership: Ownership,
    pub liveliness: Liveliness,
    pub destination_order: DestinationOrder,
    pub history: History,
    pub partitions: Vec<String>,
    pub userdata: Vec<u8>,
    #[serde(skip)]
    pub ignore_local_participant: bool,
}

impl Qos {
    fn from_qos_native_with_reliability(qos: *mut dds_qos_t, reliability: Reliability) -> Self {
        unsafe {
            // durability
            let mut dur_kind: dds_durability_kind_t = dds_durability_kind_DDS_DURABILITY_VOLATILE;
            let durability = if dds_qget_durability(qos, &mut dur_kind) {
                Durability {
                    kind: DurabilityKind::from(&dur_kind),
                }
            } else {
                Durability::default()
            };

            // durability service
            let mut service_cleanup_delay: dds_duration_t = 0;
            let mut history_kind: dds_history_kind_t = dds_history_kind_DDS_HISTORY_KEEP_LAST;
            let mut history_depth = 1i32;
            let mut max_samples = i32::MAX;
            let mut max_instances = i32::MAX;
            let mut max_samples_per_instance = i32::MAX;
            let durability_service = if dds_qget_durability_service(
                qos,
                &mut service_cleanup_delay,
                &mut history_kind,
                &mut history_depth,
                &mut max_samples,
                &mut max_instances,
                &mut max_samples_per_instance,
            ) {
                DurabilityService {
                    service_cleanup_delay,
                    history_kind: HistoryKind::from(&history_kind),
                    history_depth,
                    max_samples,
                    max_instances,
                    max_samples_per_instance,
                }
            } else {
                DurabilityService::default()
            };

            // deadline
            let mut deadline_period: dds_duration_t = DDS_INFINITE_TIME;
            let deadline = if dds_qget_deadline(qos, &mut deadline_period) {
                Deadline {
                    period: deadline_period,
                }
            } else {
                Deadline::default()
            };

            // latency_budget
            let mut lat_budget_dur: dds_duration_t = 0;
            let latency_budget = if dds_qget_latency_budget(qos, &mut lat_budget_dur) {
                LatencyBudget {
                    duration: lat_budget_dur,
                }
            } else {
                LatencyBudget::default()
            };

            // ownership
            let mut own_kind: dds_ownership_kind_t = dds_ownership_kind_DDS_OWNERSHIP_SHARED;
            let ownership = if dds_qget_ownership(qos, &mut own_kind) {
                Ownership {
                    kind: OwnershipKind::from(&own_kind),
                }
            } else {
                Ownership::default()
            };

            // liveliness
            let mut liv_kind: dds_liveliness_kind_t = dds_liveliness_kind_DDS_LIVELINESS_AUTOMATIC;
            let mut liv_lease_duration: dds_duration_t = DDS_INFINITE_TIME;
            let liveliness = if dds_qget_liveliness(qos, &mut liv_kind, &mut liv_lease_duration) {
                Liveliness {
                    kind: LivelinessKind::from(&liv_kind),
                    lease_duration: liv_lease_duration,
                }
            } else {
                Liveliness::default()
            };

            // destination_order
            let mut order_kind: dds_destination_order_kind_t =
                dds_destination_order_kind_DDS_DESTINATIONORDER_BY_RECEPTION_TIMESTAMP;
            let destination_order = if dds_qget_destination_order(qos, &mut order_kind) {
                DestinationOrder {
                    kind: DestinationOrderKind::from(&order_kind),
                }
            } else {
                DestinationOrder::default()
            };

            // history
            let mut hist_kind: dds_history_kind_t = dds_history_kind_DDS_HISTORY_KEEP_LAST;
            let mut hist_depth: i32 = 1;
            let history = if dds_qget_history(qos, &mut hist_kind, &mut hist_depth) {
                let kind = HistoryKind::from(&hist_kind);
                History {
                    kind,
                    depth: hist_depth,
                }
            } else {
                History::default()
            };

            // partitions
            let mut n = 0u32;
            let mut ps: *mut *mut ::std::os::raw::c_char = std::ptr::null_mut();
            let partitions = if dds_qget_partition(
                qos,
                &mut n as *mut u32,
                &mut ps as *mut *mut *mut ::std::os::raw::c_char,
            ) {
                let mut partitions: Vec<String> = Vec::with_capacity(n as usize);
                for k in 0..n {
                    let p = CStr::from_ptr(*(ps.offset(k as isize))).to_str().unwrap();
                    partitions.push(String::from(p));
                }
                partitions
            } else {
                Vec::new()
            };

            // userdata
            let mut sz: size_t = 0;
            let mut value: *mut ::std::os::raw::c_void = std::ptr::null_mut();
            let userdata = if dds_qget_userdata(
                qos,
                &mut value as *mut *mut ::std::os::raw::c_void,
                &mut sz as *mut size_t,
            ) {
                Vec::from_raw_parts(
                    value as *mut ::std::os::raw::c_uchar,
                    sz as usize,
                    sz as usize,
                )
            } else {
                Vec::new()
            };

            Self {
                durability,
                durability_service,
                reliability,
                deadline,
                latency_budget,
                ownership,
                liveliness,
                destination_order,
                history,
                partitions,
                userdata,
                ignore_local_participant: false,
            }
        }
    }

    pub fn from_writer_qos_native(qos: *mut dds_qos_t) -> Self {
        unsafe {
            // try to get reliability, defaulting to RELIABLE for a Writer
            let mut rel_kind: dds_reliability_kind_t = 0;
            let mut rel_max_blocking_time: dds_duration_t = 0;
            let reliability =
                if dds_qget_reliability(qos, &mut rel_kind, &mut rel_max_blocking_time) {
                    Reliability {
                        kind: ReliabilityKind::from(&rel_kind),
                        max_blocking_time: rel_max_blocking_time,
                    }
                } else {
                    Reliability {
                        kind: ReliabilityKind::RELIABLE,
                        max_blocking_time: DDS_100MS_DURATION,
                    }
                };
            Self::from_qos_native_with_reliability(qos, reliability)
        }
    }

    pub fn from_reader_qos_native(qos: *mut dds_qos_t) -> Self {
        unsafe {
            // try to get reliability, defaulting to BEST_EFFORT for a Reader
            let mut rel_kind: dds_reliability_kind_t = 0;
            let mut rel_max_blocking_time: dds_duration_t = 0;
            let reliability =
                if dds_qget_reliability(qos, &mut rel_kind, &mut rel_max_blocking_time) {
                    Reliability {
                        kind: ReliabilityKind::from(&rel_kind),
                        max_blocking_time: rel_max_blocking_time,
                    }
                } else {
                    Reliability {
                        kind: ReliabilityKind::BEST_EFFORT,
                        max_blocking_time: DDS_100MS_DURATION,
                    }
                };
            Self::from_qos_native_with_reliability(qos, reliability)
        }
    }

    pub fn to_qos_native(&self) -> *mut dds_qos_t {
        unsafe {
            let qos = dds_qos_create();

            // reliability
            dds_qset_reliability(
                qos,
                self.reliability.kind as dds_reliability_kind_t,
                self.reliability.max_blocking_time,
            );
            // durability
            dds_qset_durability(qos, self.durability.kind as dds_durability_kind_t);
            // durability service
            dds_qset_durability_service(
                qos,
                self.durability_service.service_cleanup_delay,
                self.durability_service.history_kind as dds_history_kind_t,
                self.durability_service.history_depth,
                self.durability_service.max_samples,
                self.durability_service.max_instances,
                self.durability_service.max_samples_per_instance,
            );
            // deadline
            dds_qset_deadline(qos, self.deadline.period);
            // latency_budget
            dds_qset_latency_budget(qos, self.latency_budget.duration);
            // ownership
            dds_qset_ownership(qos, self.ownership.kind as dds_ownership_kind_t);
            // liveliness
            dds_qset_liveliness(
                qos,
                self.liveliness.kind as dds_liveliness_kind_t,
                self.liveliness.lease_duration,
            );
            // destination_order
            dds_qset_destination_order(
                qos,
                self.destination_order.kind as dds_destination_order_kind_t,
            );
            // history
            dds_qset_history(
                qos,
                self.history.kind as dds_history_kind_t,
                self.history.depth,
            );
            // partitions
            if !self.partitions.is_empty() {
                let mut vcs: Vec<CString> = Vec::with_capacity(self.partitions.len());
                let mut vptr: Vec<*const c_char> = Vec::with_capacity(self.partitions.len());
                for p in &self.partitions {
                    let cs = CString::new(p.as_str()).unwrap();
                    vptr.push(cs.as_ptr());
                    vcs.push(cs);
                }
                let (ptr, len, cap) = vec_into_raw_parts(vptr);
                dds_qset_partition(qos, len as u32, ptr);
                drop(Vec::from_raw_parts(ptr, len, cap));
            }
            // userdata
            if !self.userdata.is_empty() {
                let (ptr, len, _) = vec_into_raw_parts(self.userdata.clone());
                dds_qset_userdata(qos, ptr as *const ::std::os::raw::c_void, len as size_t);
            }
            // ignore_local_participant
            if self.ignore_local_participant {
                dds_qset_ignorelocal(qos, dds_ignorelocal_kind_DDS_IGNORELOCAL_PARTICIPANT);
            }
            qos
        }
    }

    pub fn delete_qos_native(qos: *mut dds_qos_t) {
        unsafe {
            dds_delete_qos(qos);
        }
    }
}

impl Default for Qos {
    fn default() -> Self {
        unsafe {
            let qos = dds_qos_create();
            Qos::from_reader_qos_native(qos)
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct Durability {
    #[derivative(Default(value = "DurabilityKind::VOLATILE"))]
    pub kind: DurabilityKind,
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub enum DurabilityKind {
    VOLATILE = dds_durability_kind_DDS_DURABILITY_VOLATILE as isize,
    TRANSIENT_LOCAL = dds_durability_kind_DDS_DURABILITY_TRANSIENT_LOCAL as isize,
    TRANSIENT = dds_durability_kind_DDS_DURABILITY_TRANSIENT as isize,
    PERSISTENT = dds_durability_kind_DDS_DURABILITY_PERSISTENT as isize,
}

impl From<&dds_durability_kind_t> for DurabilityKind {
    fn from(from: &dds_durability_kind_t) -> Self {
        #[allow(non_upper_case_globals)]
        match from {
            &dds_durability_kind_DDS_DURABILITY_VOLATILE => DurabilityKind::VOLATILE,
            &dds_durability_kind_DDS_DURABILITY_TRANSIENT_LOCAL => DurabilityKind::TRANSIENT_LOCAL,
            &dds_durability_kind_DDS_DURABILITY_TRANSIENT => DurabilityKind::TRANSIENT,
            &dds_durability_kind_DDS_DURABILITY_PERSISTENT => DurabilityKind::PERSISTENT,
            x => panic!("Invalid numeric value for DurabilityKind: {}", x),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct DurabilityService {
    #[derivative(Default(value = "0"))]
    pub service_cleanup_delay: dds_duration_t,
    #[derivative(Default(value = "HistoryKind::KEEP_LAST"))]
    pub history_kind: HistoryKind,
    #[derivative(Default(value = "1"))]
    pub history_depth: i32,
    #[derivative(Default(value = "i32::MAX"))]
    pub max_samples: i32,
    #[derivative(Default(value = "i32::MAX"))]
    pub max_instances: i32,
    #[derivative(Default(value = "i32::MAX"))]
    pub max_samples_per_instance: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Reliability {
    pub kind: ReliabilityKind,
    pub max_blocking_time: dds_duration_t,
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub enum ReliabilityKind {
    BEST_EFFORT = dds_reliability_kind_DDS_RELIABILITY_BEST_EFFORT as isize,
    RELIABLE = dds_reliability_kind_DDS_RELIABILITY_RELIABLE as isize,
}

impl From<&dds_reliability_kind_t> for ReliabilityKind {
    fn from(from: &dds_reliability_kind_t) -> Self {
        #[allow(non_upper_case_globals)]
        match from {
            &dds_reliability_kind_DDS_RELIABILITY_BEST_EFFORT => ReliabilityKind::BEST_EFFORT,
            &dds_reliability_kind_DDS_RELIABILITY_RELIABLE => ReliabilityKind::RELIABLE,
            x => panic!("Invalid numeric value for ReliabilityKind: {}", x),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct Deadline {
    #[derivative(Default(value = "DDS_INFINITE_TIME"))]
    pub period: dds_duration_t,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct LatencyBudget {
    #[derivative(Default(value = "0"))]
    pub duration: dds_duration_t,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct DestinationOrder {
    #[derivative(Default(value = "DestinationOrderKind::BY_RECEPTION_TIMESTAMP"))]
    pub kind: DestinationOrderKind,
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub enum DestinationOrderKind {
    BY_RECEPTION_TIMESTAMP =
        dds_destination_order_kind_DDS_DESTINATIONORDER_BY_RECEPTION_TIMESTAMP as isize,
    BY_SOURCE_TIMESTAMP =
        dds_destination_order_kind_DDS_DESTINATIONORDER_BY_SOURCE_TIMESTAMP as isize,
}

impl From<&dds_destination_order_kind_t> for DestinationOrderKind {
    fn from(from: &dds_ownership_kind_t) -> Self {
        #[allow(non_upper_case_globals)]
        match from {
            &dds_destination_order_kind_DDS_DESTINATIONORDER_BY_RECEPTION_TIMESTAMP => {
                DestinationOrderKind::BY_RECEPTION_TIMESTAMP
            }
            &dds_destination_order_kind_DDS_DESTINATIONORDER_BY_SOURCE_TIMESTAMP => {
                DestinationOrderKind::BY_SOURCE_TIMESTAMP
            }
            x => panic!("Invalid numeric value for DestinationOrderKind: {}", x),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct Liveliness {
    #[derivative(Default(value = "LivelinessKind::AUTOMATIC"))]
    pub kind: LivelinessKind,
    #[derivative(Default(value = "DDS_INFINITE_TIME"))]
    pub lease_duration: dds_duration_t,
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub enum LivelinessKind {
    AUTOMATIC = dds_liveliness_kind_DDS_LIVELINESS_AUTOMATIC as isize,
    MANUAL_BY_PARTICIPANT = dds_liveliness_kind_DDS_LIVELINESS_MANUAL_BY_PARTICIPANT as isize,
    MANUAL_BY_TOPIC = dds_liveliness_kind_DDS_LIVELINESS_MANUAL_BY_TOPIC as isize,
}

impl From<&dds_liveliness_kind_t> for LivelinessKind {
    fn from(from: &dds_reliability_kind_t) -> Self {
        #[allow(non_upper_case_globals)]
        match from {
            &dds_liveliness_kind_DDS_LIVELINESS_AUTOMATIC => LivelinessKind::AUTOMATIC,
            &dds_liveliness_kind_DDS_LIVELINESS_MANUAL_BY_PARTICIPANT => {
                LivelinessKind::MANUAL_BY_PARTICIPANT
            }
            &dds_liveliness_kind_DDS_LIVELINESS_MANUAL_BY_TOPIC => LivelinessKind::MANUAL_BY_TOPIC,
            x => panic!("Invalid numeric value for ReliabilityKind: {}", x),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct Ownership {
    #[derivative(Default(value = "OwnershipKind::SHARED"))]
    pub kind: OwnershipKind,
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub enum OwnershipKind {
    SHARED = dds_ownership_kind_DDS_OWNERSHIP_SHARED as isize,
    EXCLUSIVE = dds_ownership_kind_DDS_OWNERSHIP_EXCLUSIVE as isize,
}

impl From<&dds_ownership_kind_t> for OwnershipKind {
    fn from(from: &dds_ownership_kind_t) -> Self {
        #[allow(non_upper_case_globals)]
        match from {
            &dds_ownership_kind_DDS_OWNERSHIP_SHARED => OwnershipKind::SHARED,
            &dds_ownership_kind_DDS_OWNERSHIP_EXCLUSIVE => OwnershipKind::EXCLUSIVE,
            x => panic!("Invalid numeric value for OwnershipKind: {}", x),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct History {
    #[derivative(Default(value = "HistoryKind::KEEP_LAST"))]
    pub kind: HistoryKind,
    #[derivative(Default(value = "1"))]
    pub depth: i32,
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub enum HistoryKind {
    KEEP_LAST = dds_history_kind_DDS_HISTORY_KEEP_LAST as isize,
    KEEP_ALL = dds_history_kind_DDS_HISTORY_KEEP_ALL as isize,
}

impl From<&dds_history_kind_t> for HistoryKind {
    fn from(from: &dds_history_kind_t) -> Self {
        #[allow(non_upper_case_globals)]
        match from {
            &dds_history_kind_DDS_HISTORY_KEEP_LAST => HistoryKind::KEEP_LAST,
            &dds_history_kind_DDS_HISTORY_KEEP_ALL => HistoryKind::KEEP_ALL,
            x => panic!("Invalid numeric value for HistoryKind: {}", x),
        }
    }
}

#[test]
fn test_qos_serialization() {
    let native = unsafe { dds_create_qos() };
    let qos = Qos::from_writer_qos_native(native);
    let json = serde_json::to_string(&qos).unwrap();

    println!("{json}");
    let qos2 = serde_json::from_str::<Qos>(&json).unwrap();
    assert!(qos == qos2);

    let bincode = bincode::serialize(&qos).unwrap();
    println!("len={} : {:x?}", bincode.len(), &bincode);
    let qos3 = bincode::deserialize::<Qos>(&bincode).unwrap();
    assert!(qos == qos3);
}

#[test]
fn test_to_native() {
    let native = unsafe { dds_create_qos() };
    let mut qos = Qos::from_writer_qos_native(native);

    let native2 = qos.to_qos_native();
    let qos2 = Qos::from_writer_qos_native(native2);
    assert!(qos == qos2);

    qos.partitions.push("P1".to_string());
    qos.partitions.push("P2".to_string());

    let native3 = qos.to_qos_native();
    let qos3 = Qos::from_writer_qos_native(native3);
    assert!(qos3.partitions.len() == 2);
    assert!(qos == qos3);
}
