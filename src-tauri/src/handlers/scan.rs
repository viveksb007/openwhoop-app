use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use openwhoop::ble::tauri_blec::{scan_tauri_blec_devices, TauriBlecDevice};
use openwhoop_codec::constants::WhoopGeneration;
use tauri::AppHandle;
use tokio::time::sleep;

use crate::{
    config::{ACTIVE_WHOOP_SCAN_DURATION_SECS, BACKGROUND_SYNC_POLL_INTERVAL_MS},
    error::AppResult,
    handlers::{log_error, log_info, sync::stop_background_sync},
    AppState,
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WhoopScanResult {
    address: String,
    name: String,
    rssi: Option<i16>,
    generation: WhoopGeneration,
}

impl From<TauriBlecDevice> for WhoopScanResult {
    fn from(device: TauriBlecDevice) -> Self {
        Self {
            address: device.address,
            name: whoop_device_name(&device.name),
            rssi: device.rssi,
            generation: device.generation,
        }
    }
}

pub fn whoop_device_name(name: &str) -> String {
    let trimmed_name = name.trim();

    if trimmed_name.is_empty() {
        "Unnamed WHOOP".to_owned()
    } else {
        trimmed_name.to_owned()
    }
}

#[tauri::command]
pub async fn scan_whoops(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<Vec<WhoopScanResult>> {
    let _ble_guard = state.inner().lock_ble_operation().await;
    log_info(&app, "scan_whoops", "Starting WHOOP BLE scan.");
    stop_background_sync(state.inner()).await?;

    let handler = tauri_plugin_blec::get_handler().map_err(|err| err.to_string())?;
    let _ = handler.stop_scan().await;
    let mut devices = scan_tauri_blec_devices(
        handler,
        Duration::from_secs(ACTIVE_WHOOP_SCAN_DURATION_SECS),
        false,
    )
    .await
    .map_err(|err| {
        let message = err.to_string();
        log_error(
            &app,
            "scan_whoops",
            format!("WHOOP BLE scan failed: {message}"),
        );
        message
    })?;

    devices.sort_by(|left, right| {
        right
            .rssi
            .unwrap_or(i16::MIN)
            .cmp(&left.rssi.unwrap_or(i16::MIN))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.address.cmp(&right.address))
    });

    let results = devices.into_iter().map(Into::into).collect::<Vec<_>>();
    log_info(
        &app,
        "scan_whoops",
        format!("WHOOP BLE scan finished with {} device(s).", results.len()),
    );
    Ok(results)
}

pub async fn scan_for_saved_whoop(address: &str) -> AppResult<Option<TauriBlecDevice>> {
    let handler = tauri_plugin_blec::get_handler().map_err(|err| err.to_string())?;
    let _ = handler.stop_scan().await;

    loop {
        let scanned_devices =
            scan_tauri_blec_devices(handler, Duration::from_millis(500), false).await?;

        let device = scanned_devices
            .into_iter()
            .find(|device| device.address.eq_ignore_ascii_case(address));

        if device.is_some() {
            return Ok(device);
        }
    }
}

pub async fn wait_for_stop_signal(duration: Duration, should_exit: &AtomicBool) {
    let poll_duration = Duration::from_millis(BACKGROUND_SYNC_POLL_INTERVAL_MS);
    let mut remaining = duration;

    while !should_exit.load(Ordering::SeqCst) && remaining > Duration::ZERO {
        let current_sleep = remaining.min(poll_duration);
        sleep(current_sleep).await;
        remaining = remaining.saturating_sub(current_sleep);
    }
}
