use std::{net::SocketAddr, sync::Arc, time::Duration};

use futures::lock::Mutex;
use hap::{
	Config, HapType, MacAddress, Pin, Result,
	accessory::{
		AccessoryCategory, AccessoryInformation, HapAccessory, bridge::BridgeAccessory, light_sensor::LightSensorAccessory,
	},
	server::{IpServer, Server},
	storage::{FileStorage, Storage},
};
use log::{error, info, warn};
use tokio_modbus::prelude::*;

pub struct InverterReading {
	pub power_watts: i32,
	pub daily_kwh: f32,
}

pub struct SolarMonitor {
	inverters: Vec<SocketAddr>,
	interval: Duration,
}

impl SolarMonitor {
	pub fn new(inverters: Vec<&str>) -> Self {
		SolarMonitor {
			inverters: inverters
				.iter()
				.map(|inverter| inverter.parse::<SocketAddr>().expect("Invalid inverter address"))
				.collect(),
			interval: Duration::from_secs(10),
		}
	}

	pub async fn start(&self) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
		env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

		let addr1 = self.inverters[0];
		let addr2 = self.inverters[1];

		// -- HomeKit accessories --
		let bridge = BridgeAccessory::new(
			1,
			AccessoryInformation {
				name: "Solar Monitor".into(),
				..Default::default()
			},
		)?;

		let power_total = LightSensorAccessory::new(
			2,
			AccessoryInformation {
				name: "Solar Power Total".into(),
				..Default::default()
			},
		)?;

		let energy_total = LightSensorAccessory::new(
			3,
			AccessoryInformation {
				name: "Solar Energy Today".into(),
				..Default::default()
			},
		)?;

		let power_inv1 = LightSensorAccessory::new(
			4,
			AccessoryInformation {
				name: "Solar Power Inv 1".into(),
				..Default::default()
			},
		)?;

		let power_inv2 = LightSensorAccessory::new(
			5,
			AccessoryInformation {
				name: "Solar Power Inv 2".into(),
				..Default::default()
			},
		)?;

		// -- HAP server --
		let mut storage = FileStorage::current_dir().await?;

		let config = match storage.load_config().await {
			Ok(mut config) => {
				config.host = "0.0.0.0".parse().unwrap();
				storage.save_config(&config).await?;
				config
			},
			Err(_) => {
				let config = Config {
					host: "0.0.0.0".parse().unwrap(),
					pin: Pin::new([1, 1, 1, 2, 2, 3, 3, 3])?,
					name: String::from("Solar Monitor"),
					device_id: MacAddress::from([10, 20, 30, 40, 50, 60]),
					category: AccessoryCategory::Bridge,
					..Default::default()
				};
				storage.save_config(&config).await?;
				config
			},
		};

		let server = IpServer::new(config, storage).await?;
		server.add_accessory(bridge).await?;
		let power_total_ptr = server.add_accessory(power_total).await?;
		let energy_total_ptr = server.add_accessory(energy_total).await?;
		let power_inv1_ptr = server.add_accessory(power_inv1).await?;
		let power_inv2_ptr = server.add_accessory(power_inv2).await?;

		let handle = server.run_handle();

		info!("HomeKit bridge started - pair with PIN 111-22-333");
		info!("Polling inverters at {addr1} and {addr2} every {}s", self.interval.as_secs());

		// -- Polling task --
		let poller = async move {
			let mut interval = tokio::time::interval(self.interval);
			loop {
				interval.tick().await;

				let (r1, r2) = tokio::join!(Self::read_inverter(addr1), Self::read_inverter(addr2));

				let reading1 = match r1 {
					Ok(r) => {
						info!("inv1: {}W, {:.1}kWh today", r.power_watts, r.daily_kwh);
						Some(r)
					},
					Err(e) => {
						warn!("inv1 read failed: {e}");
						None
					},
				};

				let reading2 = match r2 {
					Ok(r) => {
						info!("inv2: {}W, {:.1}kWh today", r.power_watts, r.daily_kwh);
						Some(r)
					},
					Err(e) => {
						warn!("inv2 read failed: {e}");
						None
					},
				};

				let watts1 = reading1.as_ref().map_or(0, |r| r.power_watts.max(0));
				let watts2 = reading2.as_ref().map_or(0, |r| r.power_watts.max(0));
				let kwh1 = reading1.as_ref().map_or(0.0, |r| r.daily_kwh);
				let kwh2 = reading2.as_ref().map_or(0.0, |r| r.daily_kwh);

				let total_watts = watts1 + watts2;
				let total_kwh = kwh1 + kwh2;

				info!("total: {total_watts}W, {total_kwh:.1}kWh today");

				// Update HomeKit characteristics.
				// Light sensor lux range: 0.0001-100000. Clamp minimum (HomeKit requires > 0).
				let lux_total_power = (total_watts as f64).max(0.0001);
				let lux_total_energy = (total_kwh as f64 * 100.0).max(0.0001);
				let lux_inv1 = (watts1 as f64).max(0.0001);
				let lux_inv2 = (watts2 as f64).max(0.0001);

				if let Err(e) = Self::set_lux(&power_total_ptr, lux_total_power).await {
					error!("failed to update power total: {e}");
				}
				if let Err(e) = Self::set_lux(&energy_total_ptr, lux_total_energy).await {
					error!("failed to update energy total: {e}");
				}
				if let Err(e) = Self::set_lux(&power_inv1_ptr, lux_inv1).await {
					error!("failed to update power inv1: {e}");
				}
				if let Err(e) = Self::set_lux(&power_inv2_ptr, lux_inv2).await {
					error!("failed to update power inv2: {e}");
				}
			}

			#[allow(unreachable_code)]
			Ok(())
		};

		futures::try_join!(handle, poller)?;
		Ok(())
	}

	/// Decode a word-swapped int32 from two consecutive Modbus registers.
	/// Sungrow sends low-word first: value = (r1 << 16) | r0
	fn decode_i32_word_swap(low_word: u16, high_word: u16) -> i32 {
		let combined = ((high_word as u32) << 16) | (low_word as u32);
		combined as i32
	}

	async fn read_inverter(
		ip: SocketAddr,
	) -> std::result::Result<InverterReading, Box<dyn std::error::Error + Send + Sync>> {
		let mut modbus = tcp::connect_slave(ip, Slave(1)).await?;

		// Register 5008-5009: Total active power (int32, word-swapped, watts)
		// Works on SG string inverters. Falls back to 5600 (meter power) for SH hybrid.
		let power_watts = match modbus.read_input_registers(5008, 2).await? {
			Ok(registers) => {
				let watts = Self::decode_i32_word_swap(registers[0], registers[1]);
				if watts != 0 {
					watts
				} else {
					// 5008 returned 0, try 5600 (meter active power) as fallback
					match modbus.read_input_registers(5600, 2).await? {
						Ok(registers) => Self::decode_i32_word_swap(registers[0], registers[1]),
						Err(_) => 0,
					}
				}
			},
			Err(_) => match modbus.read_input_registers(5600, 2).await? {
				// 5008 not supported, try 5600
				Ok(registers) => Self::decode_i32_word_swap(registers[0], registers[1]),
				Err(_) => 0,
			},
		};

		// Register 5000: Daily output energy (uint16, factor x0.1 kWh)
		// Works on both SG and SH inverters.
		let daily_kwh = match modbus.read_input_registers(5000, 1).await? {
			Ok(registers) => registers[0] as f32 * 0.1,
			Err(_) => 0.0,
		};

		modbus.disconnect().await?;

		Ok(InverterReading { power_watts, daily_kwh })
	}

	async fn set_lux(sensor_ptr: &Arc<Mutex<Box<dyn HapAccessory>>>, value: f64) -> Result<()> {
		let mut accessory = sensor_ptr.lock().await;
		let service = accessory.get_mut_service(HapType::LightSensor).expect("missing LightSensor service");
		let characteristic =
			service.get_mut_characteristic(HapType::CurrentLightLevel).expect("missing lux characteristic");
		characteristic.set_value(serde_json::Value::from(value)).await
	}
}
