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
#![recursion_limit = "256"]

use clap::{Arg, ArgMatches};
use dds::Participant;
use futures::prelude::*;
use futures::select;
use log::{debug, info};
use zenoh::net::*;
use zenoh_router::runtime::Runtime;

#[no_mangle]
pub fn get_expected_args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    // We will get a custom prefix for where to store our resources as an argument
    vec![
        Arg::from_usage("--storage-selector 'The selection of resources to be stored'")
            .default_value("/cdds/**"),
    ]
}

#[no_mangle]
pub fn start(runtime: Runtime, args: &'static ArgMatches<'_>) {
    async_std::task::spawn(run(runtime, args));
}

async fn check_dds_cdr() -> Option<Vec<u8>> {
    // TODO(esteve): implement peeking into DDS topics
    // e.g.
    // - dds_take
    // - check if there is data
    // - return it as a byte vector
    let dds_take_has_data = false;
    if dds_take_has_data {
        Some(vec![])
    } else {
        None
    }
}

async fn run(runtime: Runtime, args: &'static ArgMatches<'_>) {
    env_logger::init();

    let session = Session::init(runtime).await;

    let sub_info = SubInfo {
        reliability: Reliability::Reliable,
        mode: SubMode::Push,
        period: None,
    };

    let selector: ResKey = args.value_of("storage-selector").unwrap().into();
    debug!("Run cdds-plugin with storage-selector={}", selector);

    debug!("Declaring Subscriber on {}", selector);
    let mut sub = session
        .declare_subscriber(&selector, &sub_info)
        .await
        .unwrap();

    // Resource paths are made from /PARTITION_NAME/TOPIC
    let resource_path = "/resource/name";

    print!("Declaring Resource {}", resource_path);
    let rid = session
        .declare_resource(&resource_path.into())
        .await
        .unwrap();

    println!("Declaring Publisher on {}", rid);
    let publisher = session.declare_publisher(&rid.into()).await.unwrap();

    let participant = Participant::new(0);

    loop {
        select!(
            sample = sub.stream().next().fuse() => {
                let sample = sample.unwrap();
                info!("Received data ('{}': '{}')", sample.res_name, sample.payload);
                // TODO(esteve): Republish to Cyclone DDS
            },

            future = check_dds_cdr().fuse() => {
                match future {
                    Some(data) => {
                        session.write(&rid.into(), data.into()).await.unwrap();
                    }
                    None => println!("No data received"),
                }
            },
        );
    }
}
