use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

#[derive(Clone)]
pub struct BgpGlobalConfig {
    pub as_: u32,
    pub router_id: Ipv4Addr,
}

#[derive(Clone)]
pub struct BgpGlobalBase {
    pub global: BgpGlobalConfig,
}

#[derive(Clone)]
pub enum PeerType {
    Internal,
    External,
}

#[derive(Clone)]
pub struct BgpNeighborBaseConfig {
    pub router_id: Ipv4Addr, //? NOT Part of the YANG model.
    pub peer_group: String,
    pub neighbor_address: IpAddr,
    pub neighbor_port: u16,
    pub enabled: bool,
    pub peer_as: u32,
    pub local_as: u32,
    pub peer_type: PeerType,
}

#[derive(Clone)]
pub struct BgpCommonNeighborGroupTimersConfig {
    pub connect_retry: u16,
    pub hold_time: u16,
    pub keepalive_internal: u16,
    pub minimum_advertisement_interval: u16,
    pub restart_time: u16,
}

#[derive(Clone)]
pub struct BgpNeighborBaseTimers {
    pub config: BgpCommonNeighborGroupTimersConfig,
}

#[derive(Clone)]
pub struct Neighbor {
    pub neighbor_address: IpAddr,
    pub config: BgpNeighborBaseConfig,
    pub timers: BgpNeighborBaseTimers,
}

#[derive(Clone)]
pub struct BgpPeerGroupBaseConfig {
    pub peer_group_name: String,
    pub peer_as: u32,
    pub local_as: u32,
    pub peer_type: PeerType,
}

#[derive(Clone)]
pub struct PeerGroup {
    pub peer_group_name: String,
    pub config: BgpPeerGroupBaseConfig,
}

pub type BgpNeighborList = HashMap<IpAddr, Neighbor>;
pub type BgpPeerGroupList = HashMap<String, PeerGroup>;

#[derive(Clone)]
pub struct Bgp {
    pub global: BgpGlobalBase,
    pub neighbors: BgpNeighborList,
    pub peer_groups: BgpPeerGroupList,
}

impl Bgp {
    //TODO: Delete later, testing purposes only
    pub fn new() -> Bgp {
        Bgp {
            global: BgpGlobalBase {
                global: BgpGlobalConfig {
                    as_: 65000,
                    router_id: Ipv4Addr::new(192, 0, 2, 1),
                },
            },
            neighbors: HashMap::new(),
            peer_groups: HashMap::new(),
        }
    }
}
