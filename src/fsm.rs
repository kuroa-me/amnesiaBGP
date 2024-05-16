use crate::connection::BgpConn;
use crate::error::Error;
use crate::message::*;
use core::time::Duration;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task;

// Inspired from Tokio, roughly 30 years.
pub const FAR_FUTURE: Duration = Duration::from_secs(86400 * 365 * 30);

pub const DFLT_KEEPALIVE_FACTOR: u32 = 3;

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
    TcpConnectionConfirmed { bgp_conn: BgpConn },
    TcpConnectionFails {},
    BGPOpen { msg: OpenMsg, remote_syn: bool },
    BGPOpenWithDelayOpenTimerRunning { msg: OpenMsg, remote_syn: bool },
    BGPHeaderErr { err: Error },
    BGPOpenMsgErr { err: Error },
    OpenCollisionDump {},
    NotifyMsgVerErr { err: Error },
    NotifyMsgErr {},
    KeepaliveMsg {},
    UpdateMsg {},
    UpdateMsgErr {},

    // Custom Events
    ProcessOneDone { conn: BgpConn },
}

pub struct SessionAttributes {
    pub connect_retry_counter: u64,
    pub connect_retry_time: Duration,
    pub hold_time: Duration,
    pub keepalive_time: Duration,
    pub delay_open_time: Duration,
    pub delay_open: bool,
    pub send_notification_without_open: bool,
    pub damp_peer_oscillations: bool,

    //TODO: Current timer_loop implementation does not support querying the timer for remaining time.
    pub cur_connect_retry_time: Duration,
    pub cur_hold_time: Duration,
    pub cur_keepalive_time: Duration,
    pub cur_delay_open_time: Duration,
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
            send_notification_without_open: false,
            damp_peer_oscillations: false,

            cur_connect_retry_time: FAR_FUTURE,
            cur_hold_time: FAR_FUTURE,
            cur_keepalive_time: FAR_FUTURE,
            cur_delay_open_time: FAR_FUTURE,
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
    // pub tcp_conn: Option<TcpStream>,
    pub bgp_conn_handle: Option<task::JoinHandle<()>>,
    pub bgp_out_tx: Option<mpsc::Sender<Message>>,
}

pub struct Active {
    pub tcp_conn_handle: Option<task::JoinHandle<()>>,
    pub bgp_conn_handle: Option<task::JoinHandle<()>>,
    pub bgp_out_tx: Option<mpsc::Sender<Message>>,
}

pub struct OpenSent {
    pub bgp_conn_handle: Option<task::JoinHandle<()>>,
    pub bgp_out_tx: Option<mpsc::Sender<Message>>,
    pub local_open: Option<OpenMsg>,
    pub remote_syn: bool,
    pub collision_conn: Option<BgpConn>,
}

pub struct OpenConfirm {
    pub bgp_conn_handle: Option<task::JoinHandle<()>>,
    pub bgp_out_tx: Option<mpsc::Sender<Message>>,
    pub peer_open: Option<OpenMsg>,
    pub local_open: Option<OpenMsg>,
    pub remote_syn: bool,
    pub collision_conn: Option<BgpConn>,
}

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
                // tcp_conn: None,
                bgp_conn_handle: None,
                bgp_out_tx: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Idle>> for Fsm<Active> {
    fn from(fsm: Fsm<Idle>) -> Fsm<Active> {
        Fsm {
            state: Active {
                tcp_conn_handle: None,
                bgp_conn_handle: None,
                bgp_out_tx: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Connect>> for Fsm<Idle> {
    fn from(fsm: Fsm<Connect>) -> Fsm<Idle> {
        if let Some(tcp_conn_handle) = fsm.state.tcp_conn_handle {
            tcp_conn_handle.abort();
        }
        if let Some(bgp_conn_handle) = fsm.state.bgp_conn_handle {
            bgp_conn_handle.abort();
        }
        Fsm {
            state: Idle {},
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Connect>> for Fsm<Active> {
    fn from(fsm: Fsm<Connect>) -> Fsm<Active> {
        if let Some(tcp_conn_handle) = fsm.state.tcp_conn_handle {
            tcp_conn_handle.abort();
        }
        Fsm {
            state: Active {
                tcp_conn_handle: None,
                bgp_conn_handle: fsm.state.bgp_conn_handle,
                bgp_out_tx: fsm.state.bgp_out_tx,
            },
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
                bgp_out_tx: None,
                local_open: None,
                remote_syn: false,
                collision_conn: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Connect>> for Fsm<OpenConfirm> {
    fn from(fsm: Fsm<Connect>) -> Fsm<OpenConfirm> {
        Fsm {
            state: OpenConfirm {
                bgp_conn_handle: fsm.state.bgp_conn_handle,
                bgp_out_tx: fsm.state.bgp_out_tx,
                peer_open: None,
                local_open: None,
                remote_syn: false,
                collision_conn: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Active>> for Fsm<Idle> {
    fn from(fsm: Fsm<Active>) -> Fsm<Idle> {
        if let Some(bgp_conn_handle) = fsm.state.bgp_conn_handle {
            bgp_conn_handle.abort();
        }
        Fsm {
            state: Idle {},
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Active>> for Fsm<Connect> {
    fn from(fsm: Fsm<Active>) -> Fsm<Connect> {
        Fsm {
            state: Connect {
                tcp_conn_handle: fsm.state.tcp_conn_handle,
                bgp_conn_handle: fsm.state.bgp_conn_handle,
                bgp_out_tx: fsm.state.bgp_out_tx,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Active>> for Fsm<OpenSent> {
    fn from(fsm: Fsm<Active>) -> Fsm<OpenSent> {
        Fsm {
            state: OpenSent {
                bgp_conn_handle: fsm.state.bgp_conn_handle,
                bgp_out_tx: fsm.state.bgp_out_tx,
                local_open: None,
                remote_syn: true,
                collision_conn: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<Active>> for Fsm<OpenConfirm> {
    fn from(fsm: Fsm<Active>) -> Fsm<OpenConfirm> {
        Fsm {
            state: OpenConfirm {
                bgp_conn_handle: fsm.state.bgp_conn_handle,
                bgp_out_tx: fsm.state.bgp_out_tx,
                peer_open: None,
                local_open: None,
                remote_syn: true,
                collision_conn: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<OpenSent>> for Fsm<Idle> {
    fn from(fsm: Fsm<OpenSent>) -> Fsm<Idle> {
        if let Some(bgp_conn_handle) = fsm.state.bgp_conn_handle {
            bgp_conn_handle.abort();
        }
        Fsm {
            state: Idle {},
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<OpenSent>> for Fsm<Active> {
    fn from(fsm: Fsm<OpenSent>) -> Fsm<Active> {
        if let Some(bgp_conn_handle) = fsm.state.bgp_conn_handle {
            bgp_conn_handle.abort();
        }
        Fsm {
            state: Active {
                tcp_conn_handle: None,
                bgp_conn_handle: None,
                bgp_out_tx: None,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<OpenSent>> for Fsm<OpenConfirm> {
    fn from(fsm: Fsm<OpenSent>) -> Fsm<OpenConfirm> {
        Fsm {
            state: OpenConfirm {
                bgp_conn_handle: fsm.state.bgp_conn_handle,
                bgp_out_tx: fsm.state.bgp_out_tx,
                peer_open: None,
                local_open: fsm.state.local_open,
                remote_syn: fsm.state.remote_syn,
                collision_conn: fsm.state.collision_conn,
            },
            peer: fsm.peer,
            attr: fsm.attr,
        }
    }
}

impl From<Fsm<OpenConfirm>> for Fsm<Idle> {
    fn from(fsm: Fsm<OpenConfirm>) -> Fsm<Idle> {
        if let Some(bgp_conn_handle) = fsm.state.bgp_conn_handle {
            bgp_conn_handle.abort();
        }
        Fsm {
            state: Idle {},
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

    pub fn get_attr(&self) -> &SessionAttributes {
        match self {
            State::Idle(fsm) => &fsm.attr,
            State::Connect(fsm) => &fsm.attr,
            State::Active(fsm) => &fsm.attr,
            State::OpenSent(fsm) => &fsm.attr,
            State::OpenConfirm(fsm) => &fsm.attr,
            State::Established(fsm) => &fsm.attr,
        }
    }
}

pub enum Operation {
    Stop,
    Restart,
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
