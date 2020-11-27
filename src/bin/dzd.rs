#![feature(vec_into_raw_parts)]

use async_std::task;
use clap::{App, Arg};
use cyclors::*;
use futures::prelude::*;
use log::debug;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use zenoh::net::*;
use zenoh::Properties;
use zplugin_dds::*;

fn parse_args() -> (Properties, String) {
    let args = App::new("dzd zenoh router for DDS")
        .arg(Arg::from_usage(
            "-e, --peer=[LOCATOR]...  'Peer locator used to initiate the zenoh session.'",
        ))
        .arg(Arg::from_usage(
            "-l, --listener=[LOCATOR]...   'Locators to listen on.'",
        ))
        .arg(Arg::from_usage(
            "-s, --scope=[String]...   'A string used as prefix to scope DDS traffic.'",
        ))
        .arg(
            Arg::from_usage("-m, --mode=[MODE]  'The zenoh session mode.")
                .possible_values(&["peer", "client"])
                .default_value("client"),
        )
        .get_matches();

    let scope: String = args
        .value_of("scope")
        .map(|s| String::from(s))
        .or_else(|| Some(String::from("")))
        .unwrap();

    let mut config: Properties = Properties::default();
    config.insert("ZN_LOCAL_ROUTING_KEY".into(), "false".into());
    config.insert("ZN_MODE_KEY".into(), args.value_of("mode").unwrap().into());
    if let Some(locator) = args.value_of("peer") {
        config.insert("ZN_PEER_KEY".into(), locator.into());
    };

    (config, scope)
}

#[async_std::main]
async fn main() {
    env_logger::init();
    let (config, scope) = parse_args();
    let dp =
        unsafe { dds_create_participant(DDS_DOMAIN_DEFAULT, std::ptr::null(), std::ptr::null()) };
    let z = Arc::new(open(config.into()).await.unwrap());
    let (tx, rx): (Sender<MatchedEntity>, Receiver<MatchedEntity>) = channel();
    run_discovery(dp, tx);
    let mut rid_map = HashMap::<String, ResourceId>::new();
    let mut rd_map = HashMap::<String, dds_entity_t>::new();
    let mut wr_map = HashMap::<String, dds_entity_t>::new();
    let _zsub_map = HashMap::<String, CallbackSubscriber>::new();
    while let Ok(me) = rx.recv() {
        match me {
            MatchedEntity::DiscoveredPublication {
                topic_name,
                type_name,
                keyless,
                partition,
                qos,
            } => {
                debug!(
                    "DiscoveredPublication({}, {}, {:?}",
                    topic_name, type_name, partition
                );
                let key = match partition {
                    Some(p) => format!("{}/{}/{}", scope, p, topic_name),
                    None => format!("{}/{}", scope, topic_name),
                };
                debug!("Declaring resource {}", key);
                match rd_map.get(&key) {
                    None => {
                        let rkey = ResKey::RName(key.clone());
                        let nrid = z.declare_resource(&rkey).await.unwrap();
                        let rid = ResKey::RId(nrid);
                        let _ = z.declare_publisher(&rid).await;
                        rid_map.insert(key.clone(), nrid);
                        debug!("Creating Forwarding Reader for: {}", key);
                        let dr: dds_entity_t = create_forwarding_dds_reader(
                            dp,
                            topic_name,
                            type_name,
                            keyless,
                            qos,
                            rid,
                            z.clone(),
                        );
                        rd_map.insert(key, dr);
                    }
                    _ => {
                        debug!(
                            "Already forwarding matching subscription {} -- ignoring",
                            topic_name
                        );
                    }
                }
            }
            MatchedEntity::UndiscoveredPublication {
                topic_name,
                type_name,
                partition,
            } => {
                debug!(
                    "UndiscoveredPublication({}, {}, {:?}",
                    topic_name, type_name, partition
                );
            }
            MatchedEntity::DiscoveredSubscription {
                topic_name,
                type_name,
                keyless,
                partition,
                qos,
            } => {
                debug!(
                    "DiscoveredSubscription({}, {}, {:?}",
                    topic_name, type_name, partition
                );
                let key = match &partition {
                    Some(p) => format!("{}/{}/{}", scope, p, topic_name),
                    None => format!("{}/{}", scope, topic_name),
                };

                if let Some(wr) = match wr_map.get(&key) {
                    Some(_) => {
                        debug!(
                            "The Subscription({}, {}, {:?} is aready handled, IGNORING",
                            topic_name, type_name, partition
                        );
                        None
                    }
                    None => {
                        let wr = create_forwarding_dds_writer(
                            dp,
                            topic_name.clone(),
                            type_name.clone(),
                            keyless,
                            qos,
                        );
                        wr_map.insert(key.clone(), wr);
                        Some(wr)
                    }
                } {
                    debug!(
                        "The Subscription({}, {}, {:?} is new setting up zenoh and DDS endpoings",
                        topic_name, type_name, partition
                    );
                    let nrid = match rid_map.get(&key) {
                        Some(nrid) => *nrid,
                        None => {
                            let rkey = ResKey::RName(key.clone());
                            z.declare_resource(&rkey).await.unwrap()
                        }
                    };
                    rid_map.insert(key.clone(), nrid);
                    let rkey = ResKey::RId(nrid);
                    let sub_info = SubInfo {
                        reliability: Reliability::Reliable,
                        mode: SubMode::Push,
                        period: None,
                    };
                    let rsel = rkey.into();

                    let zn = z.clone();
                    task::spawn(async move {
                        let mut sub = zn.declare_subscriber(&rsel, &sub_info).await.unwrap();
                        let stream = sub.stream();
                        while let Some(d) = stream.next().await {
                            let ton = topic_name.clone();
                            let tyn = type_name.clone();
                            unsafe {
                                let bs = d.payload.to_vec();
                                // As per the Vec documentation (see https://doc.rust-lang.org/std/vec/struct.Vec.html#method.into_raw_parts)
                                // the only way to correctly releasing it is to create a vec using from_raw_parts
                                // and then have its destructor do the cleanup.
                                // Thus, while tempting to just pass the raw pointer to cyclone and then free it from C,
                                // that is not necessarily safe or guaranteed to be leak free.
                                let (ptr, len, capacity) = bs.into_raw_parts();
                                let cton = CString::new(ton).unwrap().into_raw();
                                let ctyn = CString::new(tyn).unwrap().into_raw();
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
                                    len as u64,
                                );
                                dds_writecdr(wr, fwdp as *mut ddsi_serdata);
                                drop(Vec::from_raw_parts(ptr, len, capacity));
                                cdds_sertopic_unref(st);
                            };
                        }
                    });
                }
            }
            MatchedEntity::UndiscoveredSubscription {
                topic_name,
                type_name,
                partition,
            } => {
                debug!(
                    "UndiscoveredSubscription({}, {}, {:?}",
                    topic_name, type_name, partition
                );
            }
        }
    }
}
