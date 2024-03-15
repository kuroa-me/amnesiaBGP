use ipnet::Ipv6Net;
use parking_lot::RwLock;
use prefix_trie::PrefixMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::message::Cidr;

pub struct RibNode {
    pub nlri: Cidr,
    pub loc: bool, // Selected Loc-RIB
}
pub type Rib = Arc<RwLock<PrefixMap<Ipv6Net, RibNode>>>;
pub type Vrf = HashMap<String, Arc<RwLock<Rib>>>;
