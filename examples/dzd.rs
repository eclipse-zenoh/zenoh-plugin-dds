#![feature(vec_into_raw_parts)]

use zplugin_dds::*;
use clap::{App, Arg};
use futures::prelude::*;
use zenoh::net::*;
use cyclors::*;
use std::collections::HashMap;
use log::{debug};
use std::sync::Arc;
use async_std::task;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::ffi::CString;

fn parse_args() -> (Config, String) {
    let args = App::new("dzd zenoh router for DDS")
        .arg(Arg::from_usage(
            "-e, --peer=[LOCATOR]...  'Peer locators used to initiate the zenoh session.'",
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

    let scope: String = args.value_of("scope")
        .map(|s| String::from(s))
        .or_else(|| Some(String::from("")))
        .unwrap();

    let config = Config::default()
        .add_peers(
            args.values_of("peer")
                .map(|p| p.collect())
                .or_else(|| Some(vec![]))
                .unwrap())
        .add_listeners(
            args.values_of("listener")
                .map(|p| p.collect())
                .or_else(|| Some(vec![]))
                .unwrap())
        .mode(
            args.value_of("mode")
                .map(|m| Config::parse_mode(m))
                .unwrap()
                .unwrap())
        .local_routing(false);

    (config, scope)
}


#[async_std::main]
async fn main() {
    env_logger::init();
    let mut rid_map = HashMap::<String, ResourceId>::new();
    let mut rd_map = HashMap::<String, dds_entity_t>::new();
    let mut _wr_map = HashMap::<String, dds_entity_t>::new();
    let (config, scope) = parse_args();
    let dp = unsafe {
        dds_create_participant(DDS_DOMAIN_DEFAULT, std::ptr::null(), std::ptr::null())
    };
    let z = Arc::new(open(config, None).await.unwrap());
    let (tx, rx): (Sender<MatchedEntity>, Receiver<MatchedEntity>) = channel();
    run_discovery(dp, tx);
    while let Ok(me) = rx.recv() {
        match me  {
            MatchedEntity::DiscoveredPublication {topic_name, type_name, keyless, partition, qos} => {
                debug!("DiscoveredPublication({}, {}, {:?}", topic_name, type_name, partition);
                let key = match partition {
                    Some(p) => format!("{}/{}/{}", scope, p, topic_name),
                    None => format!("{}/{}", scope, topic_name)
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
                        let dr: dds_entity_t =
                            create_forwarding_dds_reader(dp, topic_name, type_name, keyless, qos, rid, z.clone());
                        rd_map.insert(key, dr);
                    },
                    _ => {
                        debug!("Already forwarding matching subscription {} -- ignoring", topic_name);
                    }

                }
            },
            MatchedEntity::UndiscoveredPublication {topic_name, type_name, partition} => {
                debug!("UndiscoveredPublication({}, {}, {:?}", topic_name, type_name, partition);
            },
            MatchedEntity::DiscoveredSubscription {topic_name, type_name, keyless, partition, qos} => {
                debug!("DiscoveredSubscription({}, {}, {:?}", topic_name, type_name, partition);
                let key = match partition {
                    Some(p) => format!("{}/{}/{}", scope, p, topic_name),
                    None => format!("{}/{}", scope, topic_name)
                };
                debug!("Creating Subscriber for {}", key);
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
                let  rsel = rkey.into();
                let zc = z.clone();
                task::spawn(async move {
                    let wr = create_forwarding_dds_writer(dp, topic_name.clone(), type_name.clone(), keyless, qos);
                    let mut sub = zc.declare_subscriber(&rsel, &sub_info).await.unwrap();
                    let stream = sub.stream();
                    while let Some(d) = stream.next().await {
                        debug!("Received data on zenoh subscriber for resource {}", d.res_name);
                        unsafe {
                            let bs = d.payload.to_vec();
                            let (ptr, len, _) = bs.into_raw_parts();
                            let cton = CString::new(topic_name.clone()).unwrap().into_raw();
                            let ctyn = CString::new(type_name.clone()).unwrap().into_raw();
                            let st = cdds_create_blob_sertopic(dp, cton, ctyn, keyless);
                            let fwdp = cdds_ddsi_payload_create(st, ddsi_serdata_kind_SDK_DATA, ptr, len as u64);
                            dds_writecdr(wr, fwdp as *mut ddsi_serdata);
                        };

                    }
                });
            },
            MatchedEntity::UndiscoveredSubscription {topic_name, type_name, partition} => {
                debug!("UndiscoveredSubscription({}, {}, {:?}", topic_name, type_name, partition);
            },

        }
    }
}
