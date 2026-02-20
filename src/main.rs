mod solar_monitor;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use solar_monitor::{Config, SolarMonitor};

#[tokio::main]
async fn main() {
	let _ = SolarMonitor::new(Config {
		inverters: vec![
			SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 101)), 502),
			SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 102)), 502),
		],
		meter_ip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 102)), 502),
		..Default::default()
	})
	.start()
	.await;
}
