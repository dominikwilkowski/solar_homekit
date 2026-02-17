use std::{
	net::{IpAddr, Ipv4Addr, SocketAddr},
	sync::Arc,
	time::Duration,
};

use futures::lock::Mutex;
use hap::{
	Config as HapConfig, HapType, MacAddress, Pin, Result,
	accessory::{
		AccessoryCategory, AccessoryInformation, HapAccessory, bridge::BridgeAccessory, light_sensor::LightSensorAccessory,
	},
	server::{IpServer, Server},
	storage::{FileStorage, Storage},
};
use log::{error, info, warn};
use tokio_modbus::prelude::*;

#[derive(Debug, Clone)]
pub struct InverterReading {
	pub power_watts: i32,
	pub daily_kwh: f32,
}

#[derive(Debug)]
pub struct Config {
	pub inverters: Vec<SocketAddr>,
	pub pin: [u8; 8],
	pub name: String,
	pub host: IpAddr,
	pub device_id: [u8; 6],
	pub interval: Duration,
}

impl Default for Config {
	fn default() -> Self {
		Config {
			inverters: Vec::new(),
			pin: [1, 1, 1, 2, 2, 3, 3, 3],
			name: String::from("Solar Monitor"),
			host: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
			device_id: [10, 20, 30, 40, 50, 60],
			interval: Duration::from_secs(10),
		}
	}
}

pub struct SolarMonitor {
	config: Config,
}

impl SolarMonitor {
	pub fn new(config: Config) -> Self {
		SolarMonitor { config }
	}

	pub async fn start(&self) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
		env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

		let bridge = BridgeAccessory::new(
			1,
			AccessoryInformation {
				name: self.config.name.clone(),
				..Default::default()
			},
		)?;

		let power_total = LightSensorAccessory::new(
			2,
			AccessoryInformation {
				name: format!("{} Solar Power Total", self.config.name),
				..Default::default()
			},
		)?;

		let energy_total = LightSensorAccessory::new(
			3,
			AccessoryInformation {
				name: format!("{} Solar Energy Today", self.config.name),
				..Default::default()
			},
		)?;

		let inverter_power_accessories = self
			.config
			.inverters
			.iter()
			.enumerate()
			.map(|(index, _)| {
				LightSensorAccessory::new(
					(4 + index) as u64,
					AccessoryInformation {
						name: format!("{} Solar Power Inverter {}", self.config.name, index + 1),
						..Default::default()
					},
				)
			})
			.collect::<Result<Vec<LightSensorAccessory>>>()?;

		let mut storage = FileStorage::current_dir().await?;

		let config = match storage.load_config().await {
			Ok(mut config) => {
				config.host = self.config.host;
				storage.save_config(&config).await?;
				config
			},
			Err(_) => {
				let config = HapConfig {
					host: self.config.host,
					pin: Pin::new(self.config.pin)?,
					name: self.config.name.clone(),
					device_id: MacAddress::from(self.config.device_id),
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
		let mut inverter_power_ptrs = Vec::with_capacity(inverter_power_accessories.len());
		for accessory in inverter_power_accessories {
			let ptr = server.add_accessory(accessory).await?;
			inverter_power_ptrs.push(ptr);
		}

		let handle = server.run_handle();
		info!("HomeKit bridge started - pair with PIN {:?}", self.config.pin);

		let inverter_addrs = self.config.inverters.clone();
		info!("Starting polling inverters every {}s", self.config.interval.as_secs());
		let poller = async move {
			let mut interval = tokio::time::interval(self.config.interval);
			loop {
				interval.tick().await;

				let mut tasks = tokio::task::JoinSet::new();
				for (index, addr) in inverter_addrs.iter().enumerate() {
					let addr = *addr;
					tasks.spawn(async move { (index, Self::read_inverter(addr).await) });
				}

				let mut readings: Vec<Option<InverterReading>> = vec![None; inverter_addrs.len()];
				while let Some(result) = tasks.join_next().await {
					let (index, read_result) = result?;
					match read_result {
						Ok(reading) => {
							info!("Inverter {index}: {}W, {:.1}kWh today", reading.power_watts, reading.daily_kwh);
							readings[index] = Some(reading);
						},
						Err(error) => warn!("Inverter {index} read failed: {error}"),
					}
				}

				let total_watts = readings.iter().flatten().map(|reading| reading.power_watts.max(0)).sum::<i32>();
				let total_kwh = readings.iter().flatten().map(|reading| reading.daily_kwh).sum::<f32>();

				info!("total: {total_watts}W, {total_kwh:.1}kWh today");

				if let Err(error) = Self::set_lux(&power_total_ptr, Self::clamp_lux(total_watts as f64)).await {
					error!("failed to update power total: {error}");
				}
				if let Err(error) = Self::set_lux(&energy_total_ptr, Self::clamp_lux(total_kwh as f64 * 100.0)).await {
					error!("failed to update energy total: {error}");
				}

				for (index, inverter_power_ptr) in inverter_power_ptrs.iter().enumerate() {
					let watts = readings[index].as_ref().map_or(0, |reading| reading.power_watts.max(0));
					if let Err(error) = Self::set_lux(inverter_power_ptr, Self::clamp_lux(watts as f64)).await {
						error!("failed to update power inv{index}: {error}");
					}
				}
			}

			// The loop never exits, but the async block needs a return type for try_join!
			#[allow(unreachable_code)]
			Ok(())
		};

		futures::try_join!(handle, poller)?;
		Ok(())
	}

	// HomeKit light sensors: lux range 0.0001–100000
	fn clamp_lux(value: f64) -> f64 {
		value.max(0.0001)
	}

	/// Decode a word-swapped int32 from two consecutive Modbus registers.
	/// Sungrow sends low-word first: value = (r1 << 16) | r0
	fn decode_i32_word_swap(low_word: u16, high_word: u16) -> i32 {
		let combined = ((high_word as u32) << 16) | (low_word as u32);
		combined as i32
	}

	/// Access the inverter communication module via Modbus protocol
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

	/// Set the KWH as lux for the sensor
	async fn set_lux(sensor_ptr: &Arc<Mutex<Box<dyn HapAccessory>>>, value: f64) -> Result<()> {
		let mut accessory = sensor_ptr.lock().await;
		let service = accessory.get_mut_service(HapType::LightSensor).expect("missing LightSensor service");
		let characteristic =
			service.get_mut_characteristic(HapType::CurrentLightLevel).expect("missing lux characteristic");
		characteristic.set_value(serde_json::Value::from(value)).await
	}
}
