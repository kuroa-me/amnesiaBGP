use crate::connection::BgpConn;
use core::time::Duration;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task;

// Inspired from Tokio, roughly 30 years.
pub const FAR_FUTURE: Duration = Duration::from_secs(86400 * 365 * 30);

// Copied from FRR
pub const DFLT_BGP_TIMERS_CONNECT: Duration = Duration::from_secs(10);
pub const DFLT_BGP_HOLDTIME: Duration = Duration::from_secs(9);
pub const DFLT_BGP_KEEPALIVE: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum Event {
    ManualStart {},
    ManualStop {},
    AutomaticStart {},
    ManualStartWithPassiveTcpEstablishment {},
    AutomaticStartWithPassiveTcpEstablishment {},
    AutomaticStartWithDampPeerOscillations {},
    AutomaticStartWithDampPeerOscillationsAndPassiveTcpEstablishment {},
    AutomaticStop {},
    ConnectRetryTimerExpires {},
    HoldTimerExpires {},
    KeepaliveTimerExpires {},
    DelayOpenTimerExpires {},
    IdleHoldTimerExpires {},
    TcpConnectionValid {},
    TcpCRInvalid {},
    TcpCRAcked { conn: TcpStream },
    TcpConnectionConfirmed { conn: TcpStream },
    TcpConnectionFails {},
    BGPOpen {},
    BGPOpenWithDelayOpenTimerRunning {},
    BGPHeaderErr {},
    BGPOpenMsgErr {},
    BGPOpenCollisionDump {},
    NotifyMsgVerErr {},
    NotifyMsgErr {},
    KeepaliveMsg {},
    UpdateMsg {},
    UpdateMsgErr {},
}

pub struct SessionAttributes {
    pub connect_retry_counter: u64,
    pub connect_retry_time: Duration,
    pub hold_time: Duration,
    pub keepalive_time: Duration,
    pub delay_open_time: Duration,
    pub delay_open: bool,
}

impl SessionAttributes {
    pub fn new() -> SessionAttributes {
        SessionAttributes {
            connect_retry_counter: 0,
            connect_retry_time: DFLT_BGP_TIMERS_CONNECT,
            hold_time: DFLT_BGP_HOLDTIME,
            keepalive_time: DFLT_BGP_KEEPALIVE,
            delay_open_time: Duration::ZERO,
            delay_open: false,
        }
    }
}

pub struct Fsm<S> {
    pub state: S,
    pub peer: SocketAddr,
    pub attr: SessionAttributes,
}

pub struct Idle {}

pub struct Connect {
    pub tcp_conn_handle: Option<task::JoinHandle<()>>,
    pub tcp_conn: Option<TcpStream>,
}

pub struct Active {}

pub struct OpenSent {
    pub bgp_conn_handle: Option<task::JoinHandle<()>>,
    pub bgp_conn: Option<BgpConn>,
}

pub struct OpenConfirm {}

pub struct Established {}

impl Fsm<Idle> {
    pub fn new(peer: SocketAddr) -> Fsm<Idle> {
        Fsm {
            state: Idle {},
            peer,
            attr: SessionAttributes::new(),
        }
    }
}

impl From<Fsm<Idle>> for Fsm<Connect> {
    fn from(fsm: Fsm<Idle>) -> Fsm<Connect> {
        Fsm {
            state: Connect {
                tcp_conn_handle: None,
                tcp_conn: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Idle>> for Fsm<Active> {
    fn from(fsm: Fsm<Idle>) -> Fsm<Active> {
        Fsm {
            state: Active {},
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Connect>> for Fsm<Idle> {
    fn from(fsm: Fsm<Connect>) -> Fsm<Idle> {
        Fsm {
            state: Idle {},
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Connect>> for Fsm<Active> {
    fn from(fsm: Fsm<Connect>) -> Fsm<Active> {
        Fsm {
            state: Active {},
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Connect>> for Fsm<OpenSent> {
    fn from(fsm: Fsm<Connect>) -> Fsm<OpenSent> {
        Fsm {
            state: OpenSent {
                bgp_conn_handle: None,
                bgp_conn: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

pub enum State {
    Idle(Fsm<Idle>),
    Connect(Fsm<Connect>),
    Active(Fsm<Active>),
    OpenSent(Fsm<OpenSent>),
    OpenConfirm(Fsm<OpenConfirm>),
    Established(Fsm<Established>),
}

impl State {
    pub fn new(peer: SocketAddr) -> State {
        State::Idle(Fsm::new(peer))
    }
}

pub enum Operation {
    Stop,
    Reset,
    Update { duration: Duration },
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum TimerKind {
    ConnectRetry,
    Hold,
    Keepalive,
    DelayOpen,
    IdleHold,
}

impl TimerKind {
    pub fn new_vec() -> Vec<TimerKind> {
        vec![
            TimerKind::ConnectRetry,
            TimerKind::Hold,
            TimerKind::Keepalive,
            TimerKind::DelayOpen,
            TimerKind::IdleHold,
        ]
    }
}

impl Into<Event> for TimerKind {
    fn into(self) -> Event {
        match self {
            TimerKind::ConnectRetry => Event::ConnectRetryTimerExpires {},
            TimerKind::Hold => Event::HoldTimerExpires {},
            TimerKind::Keepalive => Event::KeepaliveTimerExpires {},
            TimerKind::DelayOpen => Event::DelayOpenTimerExpires {},
            TimerKind::IdleHold => Event::IdleHoldTimerExpires {},
        }
    }
}

pub enum Timer {
    ConnectRetry { op: Operation },
    Hold { op: Operation },
    Keepalive { op: Operation },
    DelayOpen { op: Operation },
    IdleHold { op: Operation },
}

// #[derive(Debug)]
// pub enum State {
//     Idle { peer: IpAddr },
//     Connect { tcp_conn: TcpStream },
//     Active {},
//     OpenSent {},
//     OpenConfirm {},
//     Established {},
// }

// impl State {
//     pub fn new(peer: IpAddr) -> State {
//         State::Idle { peer }
//     }
//     pub fn next(self, event: Event) -> State {
//         match (self, event) {
//             (State::Idle { peer }, Event::ManualStart {})
//             | (State::Idle { peer }, Event::AutomaticStart {}) => {
//                 let addr_port = SocketAddr::new(peer, 179);
//                 let conn = TcpStream::connect(addr_port);
//                 State::Connect { tcp_conn: conn }
//             }
//             (s, e) => s,
//         }
//     }
// }
