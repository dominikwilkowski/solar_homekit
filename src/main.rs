mod solar_monitor;

#[tokio::main]
async fn main() {
	let _ = solar_monitor::SolarMonitor::new(vec!["10.0.0.101:502", "10.0.0.102:502"]).start().await;
}
