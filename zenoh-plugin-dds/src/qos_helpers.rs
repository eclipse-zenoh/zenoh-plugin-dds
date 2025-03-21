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
use cyclors::qos::{DurabilityKind, DurabilityService, History, Qos, Reliability, ReliabilityKind};

pub(crate) fn get_history_or_default(qos: &Qos) -> History {
    match &qos.history {
        None => History::default(),
        Some(history) => history.clone(),
    }
}

pub(crate) fn get_durability_service_or_default(qos: &Qos) -> DurabilityService {
    match &qos.durability_service {
        None => DurabilityService::default(),
        Some(durability_service) => durability_service.clone(),
    }
}

pub(crate) fn partition_is_empty(partition: &Option<Vec<String>>) -> bool {
    #[allow(clippy::all)] // Option::is_none_or() doesn't exist in Rust 1.75
    partition
        .as_ref()
        .map_or(true, |partition| partition.is_empty())
}

pub(crate) fn partition_contains(partition: &Option<Vec<String>>, name: &String) -> bool {
    partition
        .as_ref()
        .is_some_and(|partition| partition.contains(name))
}

pub(crate) fn is_writer_reliable(reliability: &Option<Reliability>) -> bool {
    #[allow(clippy::all)] // Option::is_none_or() doesn't exist in Rust 1.75
    reliability.as_ref().map_or(true, |reliability| {
        reliability.kind == ReliabilityKind::RELIABLE
    })
}

pub(crate) fn is_reader_reliable(reliability: &Option<Reliability>) -> bool {
    reliability
        .as_ref()
        .is_some_and(|reliability| reliability.kind == ReliabilityKind::RELIABLE)
}

pub(crate) fn is_transient_local(qos: &Qos) -> bool {
    qos.durability
        .as_ref()
        .is_some_and(|durability| durability.kind == DurabilityKind::TRANSIENT_LOCAL)
}
