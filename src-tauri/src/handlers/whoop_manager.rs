use std::{fs, io::ErrorKind};

use openwhoop::ble::tauri_blec::TauriBlecDevice;
use openwhoop_codec::{
    constants::{WhoopGeneration, ALL_WHOOP_SERVICES},
    WhoopPacket,
};
use tauri::AppHandle;
use tauri_plugin_blec::{Handler, OnDisconnectHandler};

use crate::{
    config::{normalize_whoop_address, whoop_store_path, SAVED_WHOOP_SCAN_DURATION_SECS},
    error::{AppError, AppResult},
    handlers::{log_error, log_info, log_warn, whoop_device_name},
    internals::send_device_command,
    scan_for_saved_whoop,
    sync::{start_background_sync, stop_background_sync},
    AppState,
};

const PLACEHOLDER_WHOOP_ADDRESS: &str = "00:00:00:00:00:00";

fn is_placeholder_whoop_address(address: &str) -> bool {
    normalize_whoop_address(address)
        .map(|normalized| normalized == PLACEHOLDER_WHOOP_ADDRESS)
        .unwrap_or(false)
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PersistedWhoopStore {
    pub selected_whoop_address: Option<String>,
    pub generation: Option<WhoopGeneration>,
    pub has_selected_whoop: bool,
    pub debug_packets: bool,
}

impl PersistedWhoopStore {
    pub fn whoop_and_generation<'a>(&'a self) -> Option<(&'a str, WhoopGeneration)> {
        let address = self.selected_whoop_address.as_ref()?;
        let generation = self.generation?;
        Some((address, generation))
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedWhoopConnectionResult {
    address: String,
    name: Option<String>,
    rssi: Option<i16>,
    generation: Option<WhoopGeneration>,
    connected: bool,
    error: Option<AppError>,
}

#[tauri::command]
pub async fn reboot_whoop_device(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let store = read_persisted_whoop_store(&app)?;
    let Some((address, generation)) = store.whoop_and_generation() else {
        return Err(format!("Whoop device not selected"));
    };

    log_info(
        &app,
        "device_command",
        format!(
            "Reboot WHOOP requested address={} generation={}",
            address, generation
        ),
    );

    send_device_command(
        state.inner(),
        &app,
        address,
        generation,
        WhoopPacket::restart(),
    )
    .await?;
    log_info(&app, "device_command", "WHOOP reboot command completed.");
    Ok(())
}

#[tauri::command]
pub async fn erase_whoop_device_data(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let store = read_persisted_whoop_store(&app)?;
    let Some((address, generation)) = store.whoop_and_generation() else {
        return Err(format!("Whoop device not selected"));
    };

    log_info(
        &app,
        "device_command",
        format!(
            "Erase WHOOP requested address={} generation={}",
            address, generation
        ),
    );

    send_device_command(
        state.inner(),
        &app,
        address,
        generation,
        WhoopPacket::erase(),
    )
    .await?;
    log_info(&app, "device_command", "WHOOP erase command completed.");
    Ok(())
}

#[tauri::command]
pub async fn connect_to_whoop(
    address: String,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let _ble_guard = state.inner().lock_ble_operation().await;
    let normalized_address = normalize_whoop_address(&address)?;
    let existing_store = read_persisted_whoop_store(&app)?;

    if is_placeholder_whoop_address(&normalized_address) {
        log_info(
            &app,
            "connect_to_whoop",
            format!("Saving placeholder WHOOP address={normalized_address}"),
        );

        let _ = stop_background_sync(state.inner()).await;
        disconnect_connected_whoop().await;
        write_persisted_whoop_store(
            &app,
            &PersistedWhoopStore {
                selected_whoop_address: Some(normalized_address.clone()),
                generation: Some(WhoopGeneration::Placeholder),
                has_selected_whoop: true,
                debug_packets: existing_store.debug_packets,
            },
        )?;
        state
            .inner()
            .set_whoop_address(Some(normalized_address.clone()))?;
        return Ok(normalized_address);
    }

    log_info(
        &app,
        "connect_to_whoop",
        format!("Connect WHOOP requested address={}", normalized_address,),
    );

    let generation = connect_to_whoop_address(&normalized_address, state.inner()).await?;

    write_persisted_whoop_store(
        &app,
        &PersistedWhoopStore {
            selected_whoop_address: Some(normalized_address.clone()),
            generation: Some(generation),
            has_selected_whoop: true,
            debug_packets: existing_store.debug_packets,
        },
    )?;
    state
        .inner()
        .set_whoop_address(Some(normalized_address.clone()))?;

    {
        let packet = match generation {
            WhoopGeneration::Gen4 => WhoopPacket::run_alarm_now(),
            WhoopGeneration::Gen5 => WhoopPacket::run_haptic_pattern_gen5(),
            WhoopGeneration::Placeholder => {
                return Err("WhoopGeneration::Placeholder cannot ring a device".to_owned());
            }
        };

        send_device_command(state.inner(), &app, &normalized_address, generation, packet).await?;
    }

    if let Ok(handler) = tauri_plugin_blec::get_handler() {
        let _ = handler.stop_scan().await;
    }

    start_background_sync(&app, state.inner(), normalized_address.clone(), generation).await?;

    log_info(
        &app,
        "connect_to_whoop",
        format!("WHOOP connected and saved address={normalized_address}"),
    );
    Ok(normalized_address)
}

pub fn read_persisted_whoop_store(app: &AppHandle) -> AppResult<PersistedWhoopStore> {
    let store_path = whoop_store_path(app)?;

    match fs::read_to_string(store_path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(AppError::from),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(PersistedWhoopStore::default()),
        Err(err) => Err(AppError::from(err)),
    }
}

pub fn write_persisted_whoop_store(app: &AppHandle, store: &PersistedWhoopStore) -> AppResult<()> {
    let store_path = whoop_store_path(app)?;
    let contents = serde_json::to_string(store)?;
    Ok(fs::write(store_path, contents)?)
}

#[tauri::command]
pub fn get_debug_packets(app: AppHandle) -> AppResult<bool> {
    Ok(read_persisted_whoop_store(&app)?.debug_packets)
}

#[tauri::command]
pub fn set_debug_packets(app: AppHandle, enabled: bool) -> AppResult<bool> {
    let mut store = read_persisted_whoop_store(&app)?;
    store.debug_packets = enabled;
    write_persisted_whoop_store(&app, &store)?;
    Ok(store.debug_packets)
}

pub async fn connect_to_whoop_address(
    address: &str,
    state: &AppState,
) -> AppResult<WhoopGeneration> {
    stop_background_sync(state).await?;
    connect_handler_to_whoop_address(address).await
}

pub async fn connect_handler_to_whoop_address(address: &str) -> AppResult<WhoopGeneration> {
    let handler = tauri_plugin_blec::get_handler().map_err(|err| err.to_string())?;
    let _ = handler.stop_scan().await;

    if handler.is_connected() {
        let connected_device = handler.connected_device().await?;

        if connected_device.address.eq_ignore_ascii_case(address) {
            return determine_generation_from(handler).await;
        }

        disconnect_connected_whoop().await;
    }

    // On macOS the BLE peripheral id is an opaque CoreBluetooth UUID, and blec's
    // `connect(id)` only resolves peripherals discovered by a scan in the current
    // process run. The UUID itself stays stable across restarts, but after a
    // restart it is not yet in blec's in-process map, so connecting cold fails
    // with "There is no peripheral with id ...". Re-scan to rediscover the device
    // and connect using the address the scanner actually reports.
    //
    // On other platforms (e.g. Linux) the address is a stable MAC that blec can
    // connect to directly, so we keep the original cold-connect path to avoid an
    // extra scan and the risk of blocking when the device is out of range.
    #[cfg(target_os = "macos")]
    let connect_address = {
        let Some(device) = scan_for_saved_whoop(address).await? else {
            return Err(format!(
                "WHOOP {address} not found after scanning; move the device closer and retry."
            )
            .into());
        };
        device.address
    };

    #[cfg(not(target_os = "macos"))]
    let connect_address = address.to_string();

    handler
        .connect(&connect_address, OnDisconnectHandler::None, false)
        .await?;

    determine_generation_from(handler).await
}

async fn determine_generation_from(handler: &Handler) -> AppResult<WhoopGeneration> {
    let generation = handler
        .connected_device()
        .await?
        .services
        .iter()
        .find_map(|service| {
            if ALL_WHOOP_SERVICES.contains(service) {
                WhoopGeneration::from_service(*service)
            } else {
                None
            }
        })
        .unwrap_or(WhoopGeneration::Placeholder);

    Ok(generation)
}

pub async fn disconnect_connected_whoop() {
    if let Ok(handler) = tauri_plugin_blec::get_handler() {
        if handler.is_connected() {
            let _ = handler.disconnect().await;
        }
    }
}

#[tauri::command]
pub async fn connect_to_saved_whoop(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<Option<SavedWhoopConnectionResult>> {
    let _ble_guard = state.inner().lock_ble_operation().await;
    let store = read_persisted_whoop_store(&app)?;
    let Some(whoop_address) = store.selected_whoop_address else {
        return Err(AppError::from("No selected whoop"));
    };

    state
        .inner()
        .set_whoop_address(Some(whoop_address.clone()))?;

    if is_placeholder_whoop_address(&whoop_address) {
        log_info(
            &app,
            "connect_to_saved_whoop",
            "Restoring placeholder WHOOP without BLE reconnect.",
        );
        return Ok(Some(SavedWhoopConnectionResult {
            address: whoop_address,
            name: Some("Placeholder WHOOP".to_owned()),
            rssi: None,
            generation: Some(store.generation.unwrap_or(WhoopGeneration::Placeholder)),
            connected: false,
            error: None,
        }));
    }

    log_info(
        &app,
        "connect_to_saved_whoop",
        format!("Attempting reconnect for saved WHOOP address={whoop_address:?}"),
    );

    let Some(saved_whoop) = scan_for_saved_whoop(&whoop_address).await? else {
        log_warn(
            &app,
            "connect_to_saved_whoop",
            format!("Saved WHOOP was not found during scan address={whoop_address}"),
        );
        return Ok(Some(saved_whoop_connection_result(
            whoop_address,
            None,
            false,
            Some(format!(
                "Saved WHOOP not found after scanning for {} seconds.",
                SAVED_WHOOP_SCAN_DURATION_SECS
            )),
        )));
    };

    match connect_to_whoop_address(&whoop_address, state.inner()).await {
        Ok(_) => {
            let sync_error = start_background_sync(
                &app,
                state.inner(),
                whoop_address.clone(),
                saved_whoop.generation,
            )
            .await
            .err();

            Ok(Some(saved_whoop_connection_result(
                whoop_address,
                Some(&saved_whoop),
                true,
                sync_error,
            )))
        }
        Err(reason) => {
            log_error(
                &app,
                "connect_to_saved_whoop",
                format!(
                    "Saved WHOOP reconnect failed address={}: {:?}",
                    whoop_address, reason
                ),
            );
            Ok(Some(saved_whoop_connection_result(
                whoop_address,
                Some(&saved_whoop),
                false,
                Some(reason),
            )))
        }
    }
}

#[tauri::command]
pub async fn clear_selected_whoop_address(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<()> {
    log_info(
        &app,
        "selection_state",
        "Clearing the saved WHOOP selection.",
    );
    let _ = stop_background_sync(state.inner()).await;
    disconnect_connected_whoop().await;
    let existing_store = read_persisted_whoop_store(&app)?;
    write_persisted_whoop_store(
        &app,
        &PersistedWhoopStore {
            selected_whoop_address: None,
            generation: None,
            has_selected_whoop: true,
            debug_packets: existing_store.debug_packets,
        },
    )?;
    state.set_whoop_address(None)?;
    log_info(&app, "selection_state", "Saved WHOOP selection cleared.");
    Ok(())
}

fn saved_whoop_connection_result<E>(
    address: String,
    device: Option<&TauriBlecDevice>,
    connected: bool,
    error: Option<E>,
) -> SavedWhoopConnectionResult
where
    AppError: From<E>,
{
    SavedWhoopConnectionResult {
        address,
        name: device.map(|device| whoop_device_name(&device.name)),
        rssi: device.and_then(|device| device.rssi),
        generation: device.map(|device| device.generation),
        connected,
        error: error.map(AppError::from),
    }
}
