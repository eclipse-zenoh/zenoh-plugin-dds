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
use cyclors::*;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::json;

pub const DDS_INFINITE_TIME: i64 = 0x7FFFFFFFFFFFFFFF;

#[derive(Debug)]
pub struct QosHolder(pub *mut dds_qos_t);
unsafe impl Send for QosHolder {}
unsafe impl Sync for QosHolder {}

impl QosHolder {
    pub fn is_transient_local(&self) -> bool {
        unsafe {
            let mut dur_kind: dds_durability_kind_t = dds_durability_kind_DDS_DURABILITY_VOLATILE;
            dds_qget_durability(self.0, &mut dur_kind)
                && dur_kind == dds_durability_kind_DDS_DURABILITY_TRANSIENT_LOCAL
        }
    }

    pub fn history_length(&self) -> usize {
        unsafe {
            let mut kind = dds_history_kind_DDS_HISTORY_KEEP_LAST;
            let mut depth = 1;
            dds_qget_history(self.0, &mut kind, &mut depth);
            if kind == dds_history_kind_DDS_HISTORY_KEEP_ALL {
                usize::MAX
            } else {
                depth as usize
            }
        }
    }
}

impl Serialize for QosHolder {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut qos = serializer.serialize_struct("qos", 7)?;
        #[allow(non_upper_case_globals)]
        unsafe {
            // durability
            let mut dur_kind: dds_durability_kind_t = dds_durability_kind_DDS_DURABILITY_VOLATILE;
            if dds_qget_durability(self.0, &mut dur_kind) {
                let d = match dur_kind {
                    dds_durability_kind_DDS_DURABILITY_VOLATILE => json!({"kind": "VOLATILE"}),
                    dds_durability_kind_DDS_DURABILITY_TRANSIENT_LOCAL => {
                        json!({"kind": "TRANSIENT_LOCAL"})
                    }
                    dds_durability_kind_DDS_DURABILITY_TRANSIENT => json!({"kind": "TRANSIENT"}),
                    dds_durability_kind_DDS_DURABILITY_PERSISTENT => json!({"kind": "PERSISTENT"}),
                    x => json!({ "kind": x }),
                };
                qos.serialize_field("durability", &d)?;
            } else {
                qos.serialize_field("durability", "FAILED TO RETRIEVE")?;
            }

            // reliability
            let mut rel_kind: dds_reliability_kind_t =
                dds_reliability_kind_DDS_RELIABILITY_BEST_EFFORT;
            let mut rel_max_blocking_time: dds_duration_t = 0;
            if dds_qget_reliability(self.0, &mut rel_kind, &mut rel_max_blocking_time) {
                let k = match rel_kind {
                    dds_reliability_kind_DDS_RELIABILITY_BEST_EFFORT => {
                        json!({"kind": "BEST_EFFORT", "max_blocking_time": rel_max_blocking_time})
                    }
                    dds_reliability_kind_DDS_RELIABILITY_RELIABLE => {
                        json!({"kind": "RELIABLE", "max_blocking_time": rel_max_blocking_time})
                    }
                    x => {
                        json!({"kind": x, "max_blocking_time": rel_max_blocking_time})
                    }
                };
                qos.serialize_field("reliability", &k)?;
            } else {
                qos.serialize_field("reliability", "FAILED TO RETRIEVE")?;
            }

            // deadline
            let mut deadline_period: dds_duration_t = DDS_INFINITE_TIME;
            dds_qget_deadline(self.0, &mut deadline_period);
            qos.serialize_field("deadline", &json!({ "period": deadline_period }))?;

            // latency_budget
            let mut lat_budget: dds_duration_t = 0;
            dds_qget_latency_budget(self.0, &mut lat_budget);
            qos.serialize_field("latency_budget", &json!({ "duration": lat_budget }))?;

            // ownership
            let mut own_kind: dds_ownership_kind_t = dds_ownership_kind_DDS_OWNERSHIP_SHARED;
            if dds_qget_ownership(self.0, &mut own_kind) {
                let k = match own_kind {
                    dds_ownership_kind_DDS_OWNERSHIP_SHARED => {
                        json!({"kind": "SHARED"})
                    }
                    dds_ownership_kind_DDS_OWNERSHIP_EXCLUSIVE => {
                        json!({"kind": "EXCLUSIVE"})
                    }
                    x => {
                        json!({ "kind": x })
                    }
                };
                qos.serialize_field("ownership", &k)?;
            } else {
                qos.serialize_field("ownership", "FAILED TO RETRIEVE")?;
            }

            // liveliness
            let mut liv_kind: dds_liveliness_kind_t = dds_liveliness_kind_DDS_LIVELINESS_AUTOMATIC;
            let mut liv_lease_duration: dds_duration_t = DDS_INFINITE_TIME;
            if dds_qget_liveliness(self.0, &mut liv_kind, &mut liv_lease_duration) {
                let k = match liv_kind {
                    dds_liveliness_kind_DDS_LIVELINESS_AUTOMATIC => {
                        json!({"kind": "AUTOMATIC", "lease_duration": liv_lease_duration})
                    }
                    dds_liveliness_kind_DDS_LIVELINESS_MANUAL_BY_PARTICIPANT => {
                        json!({"kind": "MANUAL_BY_PARTICIPANT", "lease_duration": liv_lease_duration})
                    }
                    dds_liveliness_kind_DDS_LIVELINESS_MANUAL_BY_TOPIC => {
                        json!({"kind": "MANUAL_BY_TOPIC", "lease_duration": liv_lease_duration})
                    }
                    x => {
                        json!({"kind": x, "lease_duration": liv_lease_duration})
                    }
                };
                qos.serialize_field("liveliness", &k)?;
            } else {
                qos.serialize_field("liveliness", "FAILED TO RETRIEVE")?;
            }

            // destination_order
            let mut order_kind: dds_destination_order_kind_t =
                dds_destination_order_kind_DDS_DESTINATIONORDER_BY_RECEPTION_TIMESTAMP;
            if dds_qget_destination_order(self.0, &mut order_kind) {
                let k = match order_kind {
                    dds_destination_order_kind_DDS_DESTINATIONORDER_BY_RECEPTION_TIMESTAMP => {
                        json!({"kind": "BY_RECEPTION_TIMESTAMP"})
                    }
                    dds_destination_order_kind_DDS_DESTINATIONORDER_BY_SOURCE_TIMESTAMP => {
                        json!({"kind": "BY_SOURCE_TIMESTAMP"})
                    }
                    x => {
                        json!({ "kind": x })
                    }
                };
                qos.serialize_field("destination_order", &k)?;
            } else {
                qos.serialize_field("destination_order", "FAILED TO RETRIEVE")?;
            }

            // (Presentation?)
        }

        qos.end()
    }
}
