//! WiFi station, DHCP and the TCP line server.
//!
//! Three long-lived tasks:
//!
//! - [`wifi_task`] owns the radio controller: it connects, re-connects forever
//!   if the AP disappears, and samples the RSSI while the link is up.
//! - [`link_task`] watches the DHCP lease and publishes the `# wifi connected
//!   ip=...` / `# wifi disconnected` status lines into the shared line ring.
//! - [`tcp_task`] is the line server on [`TCP_PORT`]. It keeps TWO sockets so a
//!   new connection can always be accepted while one is in use: the fresh
//!   connection then replaces the old one. That is what stops a half-dead TCP
//!   session - the normal outcome of rebooting the logging server - from
//!   locking everyone out until the sniffer is power-cycled.
//!
//! The TCP cursor into the line ring lives in `tcp_task` and therefore survives
//! client disconnects, which is what gives a reconnecting client its replay of
//! everything produced while it was away.

use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either};
use embassy_net::tcp::{State, TcpSocket};
use embassy_net::{DhcpConfig, Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write as _;
use esp_hal::rng::Rng;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config as WifiConfig, ControllerConfig, Interface, WifiController,
};
use static_cell::StaticCell;

use modbus_sniffer_core as sniffer;
use sniffer::{Chunk, Line, Marker};

use crate::{CommandBuffer, DEVICE_NAME, RSSI_UNKNOWN, WIFI_IP, WIFI_RSSI, WIFI_UP};

/// The line server's TCP port.
pub const TCP_PORT: u16 = 4000;

/// DHCP hostname, so the server can reach the sniffer by name.
const HOSTNAME: &str = "modbus-sniffer";

/// Sockets the stack has to manage: the two TCP sockets plus the DHCP client.
const SOCKETS: usize = 3;

/// Per-socket buffers. The RX buffer only ever carries short commands; the TX
/// buffer wants room so a replay burst is not written one segment at a time.
const TCP_RX_BUF: usize = 512;
const TCP_TX_BUF: usize = 4096;

/// How long a connection may go unacknowledged before the stack gives up, and
/// how often an idle connection is probed. Without this a client that vanished
/// without a FIN (power cut, crashed server) would hold the socket forever.
const TCP_TIMEOUT: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE: Duration = Duration::from_secs(10);

/// Delay before retrying a failed association attempt.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// How often the RSSI is re-sampled while connected.
const RSSI_INTERVAL: Duration = Duration::from_secs(5);

static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();
static RX_A: StaticCell<[u8; TCP_RX_BUF]> = StaticCell::new();
static TX_A: StaticCell<[u8; TCP_TX_BUF]> = StaticCell::new();
static RX_B: StaticCell<[u8; TCP_RX_BUF]> = StaticCell::new();
static TX_B: StaticCell<[u8; TCP_TX_BUF]> = StaticCell::new();

/// Bring up the radio, the network stack and the line server.
///
/// Returns `false` (after publishing a status line of its own) if the radio
/// cannot be initialised; the sniffer carries on over USB either way.
pub fn start(
    spawner: &Spawner,
    wifi: esp_hal::peripherals::WIFI<'static>,
    ssid: &'static str,
    password: &'static str,
) -> bool {
    let mut controller = match WifiController::new(wifi, ControllerConfig::default()) {
        Ok(controller) => controller,
        Err(_) => {
            crate::publish_text("# wifi disabled (radio init failed)\r\n");
            return false;
        }
    };

    let Ok(station) = station_config(ssid, password) else {
        crate::publish_text("# wifi disabled (SSID or password rejected)\r\n");
        return false;
    };
    if controller.set_config(&station).is_err() {
        crate::publish_text("# wifi disabled (station config rejected)\r\n");
        return false;
    }

    let interface = Interface::station();

    // The radio is initialised by now, so the RNG is a true RNG.
    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());

    let mut dhcp = DhcpConfig::default();
    let mut hostname = heapless09::String::new();
    if hostname.push_str(HOSTNAME).is_ok() {
        dhcp.hostname = Some(hostname);
    }

    let (stack, runner) = embassy_net::new(
        interface,
        embassy_net::Config::dhcpv4(dhcp),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(wifi_task(controller).unwrap());
    spawner.spawn(link_task(stack).unwrap());
    spawner.spawn(tcp_task(stack).unwrap());
    true
}

fn station_config(ssid: &str, password: &str) -> Result<WifiConfig, ()> {
    let ssid = ssid.try_into().map_err(|_| ())?;
    let authentication = if password.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::WpaWpa2Personal(password.try_into().map_err(|_| ())?)
    };
    Ok(WifiConfig::Station(
        StationConfig::default()
            .with_ssid(ssid)
            .with_authentication(authentication),
    ))
}

// ---------------------------------------------------------------------------
// Radio and stack
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

/// Associate, stay associated, and keep the RSSI fresh. Never returns.
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    loop {
        if controller.connect_async().await.is_err() {
            Timer::after(RECONNECT_DELAY).await;
            continue;
        }

        // Both of these only need &controller, so they can run together.
        let disconnected = controller.wait_for_disconnect_async();
        let sample_rssi = async {
            loop {
                match controller.rssi() {
                    Ok(rssi) => WIFI_RSSI.store(rssi, Ordering::Relaxed),
                    Err(_) => WIFI_RSSI.store(RSSI_UNKNOWN, Ordering::Relaxed),
                }
                Timer::after(RSSI_INTERVAL).await;
            }
        };
        let _ = select(disconnected, sample_rssi).await;

        WIFI_RSSI.store(RSSI_UNKNOWN, Ordering::Relaxed);
        // Guard against a connect/drop loop spinning the CPU.
        Timer::after(Duration::from_secs(1)).await;
    }
}

/// Publish the link status lines as the DHCP lease comes and goes.
#[embassy_executor::task]
async fn link_task(stack: Stack<'static>) {
    loop {
        stack.wait_config_up().await;

        let mut line = Line::new();
        let ip = stack.config_v4().map(|cfg| cfg.address.address().octets());
        if let Some([a, b, c, d]) = ip {
            WIFI_IP.store(u32::from_be_bytes([a, b, c, d]), Ordering::Relaxed);
            let _ = core::fmt::Write::write_fmt(
                &mut line,
                format_args!("# wifi connected ip={a}.{b}.{c}.{d}\r\n"),
            );
        }
        WIFI_UP.store(true, Ordering::Relaxed);
        if !line.is_empty() {
            crate::publish_text(&line);
        }

        stack.wait_config_down().await;

        WIFI_UP.store(false, Ordering::Relaxed);
        WIFI_IP.store(0, Ordering::Relaxed);
        crate::publish_text("# wifi disconnected\r\n");
    }
}

// ---------------------------------------------------------------------------
// TCP line server
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn tcp_task(stack: Stack<'static>) {
    let mut socket_a = TcpSocket::new(
        stack,
        RX_A.init([0; TCP_RX_BUF]),
        TX_A.init([0; TCP_TX_BUF]),
    );
    let mut socket_b = TcpSocket::new(
        stack,
        RX_B.init([0; TCP_RX_BUF]),
        TX_B.init([0; TCP_TX_BUF]),
    );
    socket_a.set_timeout(Some(TCP_TIMEOUT));
    socket_a.set_keep_alive(Some(TCP_KEEPALIVE));
    socket_b.set_timeout(Some(TCP_TIMEOUT));
    socket_b.set_keep_alive(Some(TCP_KEEPALIVE));

    // Survives every client, which is what makes the replay work.
    let mut cursor = 0u64;
    let mut serve_a = true;
    let mut already_connected = false;

    loop {
        let (active, idle) = if serve_a {
            (&mut socket_a, &mut socket_b)
        } else {
            (&mut socket_b, &mut socket_a)
        };

        if !already_connected && active.accept(TCP_PORT).await.is_err() {
            active.abort();
            let _ = active.flush().await;
            Timer::after(Duration::from_millis(200)).await;
            continue;
        }
        already_connected = false;

        serve_client(active, idle, &mut cursor).await;

        // abort() rather than close(): a RST frees the socket immediately
        // instead of parking it in TIME_WAIT, and we may need it again at once.
        active.abort();
        let _ = active.flush().await;

        // `accept()` puts the socket in LISTEN *before* it awaits, so dropping
        // that future leaves `idle` listening. Decide what happened by looking
        // at the socket rather than at which select branch won - if the old
        // client went away in the same instant a new one arrived, the accept
        // may have completed without being the branch that was reported.
        if matches!(idle.state(), State::Closed | State::Listen) {
            // Nobody took over. Stop it listening, so exactly one socket is
            // listening on the port at any time and no connection can land on a
            // socket nobody is serving.
            idle.abort();
        } else {
            serve_a = !serve_a;
            already_connected = true;
        }
    }
}

/// Serve one client until it goes away or a new connection arrives on `idle`.
async fn serve_client(
    active: &mut TcpSocket<'static>,
    idle: &mut TcpSocket<'static>,
    cursor: &mut u64,
) {
    // Exactly one hello line, before the replay, with the uptime as of now so
    // the server can map device uptime onto wall-clock time.
    let mut hello = Line::new();
    let replay = crate::line_bus().pending(*cursor);
    let hello_ok = sniffer::format_hello(
        DEVICE_NAME,
        env!("CARGO_PKG_VERSION"),
        Instant::now().as_millis(),
        crate::current_bus(),
        replay,
        &mut hello,
    )
    .is_ok();
    if hello_ok && active.write_all(hello.as_bytes()).await.is_err() {
        return;
    }

    // Replies belong to the requester only, so they are handed to the writer
    // instead of going into the ring. One task, hence a NoopRawMutex.
    let replies: Channel<NoopRawMutex, Line, 2> = Channel::new();
    let (mut reader, mut writer) = active.split();

    let pump = async {
        let mut buf = [0u8; sniffer::MAX_LINE_LEN];
        let mut marker = Marker::new();
        loop {
            let out = select(
                crate::line_bus().next(sniffer::CONSUMER_TCP, cursor, &mut buf),
                replies.receive(),
            )
            .await;

            let written = match out {
                Either::First(Chunk::Line(len)) => writer.write_all(&buf[..len]).await,
                Either::First(Chunk::Dropped(missed)) => {
                    crate::count_dropped_lines(missed);
                    if sniffer::format_dropped_lines(missed, &mut marker).is_ok() {
                        writer.write_all(marker.as_bytes()).await
                    } else {
                        Ok(())
                    }
                }
                // `next` only returns Empty if it is told not to wait.
                Either::First(Chunk::Empty) => Ok(()),
                Either::Second(reply) => writer.write_all(reply.as_bytes()).await,
            };
            if written.is_err() || writer.flush().await.is_err() {
                return;
            }
        }
    };

    let commands = async {
        let mut command = CommandBuffer::new();
        let mut chunk = [0u8; 64];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    for &byte in &chunk[..n] {
                        if let Some(reply) = command.feed(byte).await {
                            replies.send(reply).await;
                        }
                    }
                }
            }
        }
    };

    // The caller inspects `idle` afterwards to see whether a new client took
    // over, so the branch that won does not matter here.
    let _ = select3(pump, commands, idle.accept(TCP_PORT)).await;
}
