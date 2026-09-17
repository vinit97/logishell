//! Direct command execution, device selection, and exclusive hardware access.

use crate::{
    bluetooth, config, device,
    model::{Device, DeviceState, Inventory},
    remap,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    future::Future,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::watch,
};
pub struct ControllerLock {
    _file: File,
}

pub struct PairingLock {
    _command: ControllerLock,
    _active_mappings: ControllerLock,
}

pub fn runtime_dir() -> Result<PathBuf> {
    // SAFETY: geteuid takes no arguments and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let directory = root.join(format!("logishell-{uid}"));
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).context("create logishell runtime directory"),
    }
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "{} must be a real directory owned by the current user with mode 0700",
            directory.display()
        );
    }
    Ok(directory)
}

use std::os::unix::fs::DirBuilderExt;

/// Pairing can reuse a receiver slot, so exclude active mapping sessions until
/// the complete pairing/unpairing operation finishes. An idle daemon permits it.
pub fn pairing_lock() -> Result<PairingLock> {
    let directory = runtime_dir()?;
    let command = lock_file(&directory.join("controller.lock"))?;
    let active_mappings = try_lock_file(&directory.join("remap-active.lock"))?.context(
        "device remapping is active; stop `logishell daemon` before receiver pairing or unpairing",
    )?;
    Ok(PairingLock {
        _command: command,
        _active_mappings: active_mappings,
    })
}

/// Blocking mapping workers use this only for route identity checks and
/// diversion changes; their event loop uses its separate HID++ software ID.
pub(crate) fn mapping_command_lock() -> Result<ControllerLock> {
    let path = runtime_dir()?.join("controller.lock");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(lock) = try_lock_file(&path)? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            bail!(
                "another logishell command prevented remapping cleanup or initialization for 30 seconds"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn lock_file(path: &Path) -> Result<ControllerLock> {
    try_lock_file(path)?.context("another logishell command is active; wait for it to finish")
}

pub(crate) fn try_lock_file(path: &Path) -> Result<Option<ControllerLock>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open controller lock {}", path.display()))?;
    // SAFETY: the file descriptor remains open for the lock's lifetime.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error)
            .context("acquire controller lock; check runtime directory and sandbox access");
    }
    Ok(Some(ControllerLock { _file: file }))
}

pub(crate) async fn command_lock() -> Result<ControllerLock> {
    wait_for_command_lock(
        &runtime_dir()?.join("controller.lock"),
        Duration::from_secs(10),
    )
    .await
}

/// Read inventory while holding the same command lock as configuration changes.
pub async fn inventory() -> Result<Inventory> {
    let _lock = command_lock().await?;
    discover(device::Discovery::Status).await
}

async fn wait_for_command_lock(path: &Path, timeout: Duration) -> Result<ControllerLock> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(lock) = try_lock_file(path)? {
            return Ok(lock);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("another logishell command is still active; try again when it finishes");
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(50))).await;
    }
}

async fn discover(detail: device::Discovery) -> Result<Inventory> {
    let (hid, bluetooth) = tokio::join!(
        tokio::task::spawn_blocking(move || device::discover(detail)),
        bluetooth::inventory()
    );
    let mut result = match hid.context("device discovery worker failed")? {
        Ok(inventory) => inventory,
        Err(error) => Inventory {
            warnings: vec![format!("HID discovery: {error:#}")],
            ..Default::default()
        },
    };
    match bluetooth {
        Ok(bt) => {
            for device in bt.devices {
                if let Some(existing) = result.devices.iter_mut().find(|d| {
                    d.id == device.id
                        || d.bluetooth_address
                            .as_ref()
                            .zip(device.bluetooth_address.as_ref())
                            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
                }) {
                    if existing.battery.is_none() {
                        existing.battery = device.battery;
                    }
                    if existing.bluetooth_address.is_none() {
                        existing.bluetooth_address = device.bluetooth_address;
                    }
                    if existing.state == DeviceState::Unknown {
                        existing.state = device.state;
                    }
                    existing.warnings.extend(device.warnings);
                } else {
                    result.devices.push(device);
                }
            }
            result.warnings.extend(bt.warnings);
        }
        Err(error) => result
            .warnings
            .push(format!("Bluetooth discovery: {error:#}")),
    }
    result.devices.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}

pub fn select<'a>(
    inventory: &'a Inventory,
    config: &config::Config,
    selector: &str,
) -> Result<&'a Device> {
    if selector.trim().is_empty() {
        bail!("device selector cannot be empty; use a device ID, saved alias, or unambiguous name");
    }
    let alias_id = config.devices.iter().find_map(|(id, c)| {
        c.alias
            .as_ref()
            .filter(|a| a.eq_ignore_ascii_case(selector))
            .map(|_| id)
    });
    // A saved alias is an explicit identity, even while its device is absent.
    // Never reinterpret it as a fuzzy name and select a different peripheral.
    if let Some(alias_id) = alias_id {
        return inventory
            .devices
            .iter()
            .find(|device| &device.id == alias_id)
            .with_context(|| {
                format!("alias {selector:?} refers to {alias_id}, which is not currently detected")
            });
    }
    let exact: Vec<_> = inventory
        .devices
        .iter()
        .filter(|d| d.id == selector)
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }
    let selector_lower = selector.to_lowercase();
    let selector_model = crate::model::classify(selector);
    let matches: Vec<_> = inventory
        .devices
        .iter()
        .filter(|d| {
            d.id.starts_with(selector)
                || d.name.to_lowercase().contains(&selector_lower)
                || selector_model
                    .is_some_and(|model| crate::model::classify(&d.name) == Some(model))
        })
        .collect();
    match matches.as_slice() {
        [device] => Ok(device),
        [] => bail!("no detected device matches {selector:?}; run `logishell status`"),
        _ => bail!(
            "{selector:?} is ambiguous; use one of: {}",
            matches
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Keep the lock alive with the configuration and inventory used to select a
/// target. Every public command holds it through hardware work and file writes.
pub(crate) struct CommandContext {
    _lock: ControllerLock,
    config: config::Config,
    inventory: Inventory,
}

impl CommandContext {
    pub(crate) async fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            _lock: command_lock().await?,
            config: config::load(path)?,
            inventory: discover(device::Discovery::Control).await?,
        })
    }

    fn target(&self, selector: &str) -> Result<Device> {
        select(&self.inventory, &self.config, selector).cloned()
    }

    async fn setting(&self, selector: &str, key: &str) -> Result<Value> {
        let target = self.target(selector)?;
        if key == "thumb-wheel-interval" {
            return Ok(json!(
                self.config
                    .devices
                    .get(&target.id)
                    .map_or(0, |saved| saved.thumb_wheel_interval_ms)
            ));
        }
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || device::setting(&target, &key))
            .await
            .context("device setting worker failed")?
    }

    pub(crate) async fn set(
        &self,
        path: &Path,
        selector: &str,
        key: &str,
        value: &str,
        temporary: bool,
    ) -> Result<Value> {
        let target = self.target(selector)?;
        if key == "thumb-wheel-interval" {
            let interval = parse_thumb_wheel_interval(value, temporary)?;
            let mut config = config::load(path)?;
            config
                .devices
                .entry(target.id.clone())
                .or_default()
                .thumb_wheel_interval_ms = interval;
            config::save(path, &config)?;
            return Ok(json!({"device":target.id,"setting":key,"value":interval,"saved":true}));
        }
        crate::model::validate_setting(&target, key, value)?;
        if !temporary {
            let mut prospective = config::load(path)?;
            config::record_setting(
                prospective.devices.entry(target.id.clone()).or_default(),
                key,
                value,
            );
            config::validate(&prospective)
                .context("saved settings would be inconsistent; device was not changed")?;
        }
        let (cloned, k, v) = (target.clone(), key.to_owned(), value.to_owned());
        let actual =
            tokio::task::spawn_blocking(move || device::set_setting(&cloned, &k, &v)).await??;
        if !temporary {
            let mut config = config::load(path)
                .context("device setting changed, but configuration could not be reloaded")?;
            config::record_setting(
                config.devices.entry(target.id.clone()).or_default(),
                key,
                value,
            );
            config::save(path, &config)
                .context("device setting changed, but it could not be saved")?;
        }
        Ok(json!({"device":target.id,"setting":key,"value":actual,"saved":!temporary}))
    }

    pub(crate) async fn update_bindings(
        &self,
        path: &Path,
        selector: &str,
        edits: &BTreeMap<String, Option<String>>,
    ) -> Result<Value> {
        let target = self.target(selector)?;
        let mut config = config::load(path)?;
        let bindings = &mut config
            .devices
            .entry(target.id.clone())
            .or_default()
            .bindings;
        *bindings = remap::updated_bindings(bindings, edits)?;
        let proposed = bindings.clone();
        config::validate(&config)?;
        if edits.values().any(Option::is_some) {
            let cloned = target.clone();
            tokio::task::spawn_blocking(move || remap::check_bindings(&cloned, &proposed))
                .await??;
        }
        config::save(path, &config)?;
        Ok(json!({"device":target.id,"saved":true}))
    }

    pub(crate) async fn alias(&self, path: &Path, selector: &str, alias: &str) -> Result<Value> {
        let target = self.target(selector)?;
        self.aliases(
            path,
            &BTreeMap::from([(target.id.clone(), alias.to_owned())]),
        )?;
        Ok(json!({"device":target.id,"alias":alias}))
    }

    pub(crate) fn aliases(&self, path: &Path, aliases: &BTreeMap<String, String>) -> Result<()> {
        let mut config = config::load(path)?;
        for (selector, alias) in aliases {
            let target = self.target(selector)?;
            config.devices.entry(target.id).or_default().alias = Some(alias.clone());
        }
        config::save(path, &config)
    }
}

pub async fn get(path: &Path, selector: &str) -> Result<Value> {
    checked_settings(info(path, selector).await?)
}

pub async fn get_setting(path: &Path, selector: &str, key: &str) -> Result<Value> {
    CommandContext::open(path)
        .await?
        .setting(selector, key)
        .await
}

fn checked_settings(result: Value) -> Result<Value> {
    if let Some(error) = result.get("settings_error").and_then(Value::as_str) {
        bail!("{error}");
    }
    Ok(result)
}

pub async fn info(path: &Path, selector: &str) -> Result<Value> {
    let command = CommandContext::open(path).await?;
    let target = command.target(selector)?;
    read_device_info(target).await
}

pub async fn inspect_selected(path: &Path, device: &Device) -> Result<Value> {
    let _lock = command_lock().await?;
    config::load(path)?;
    let mut target = device.clone();
    if target.state == DeviceState::Offline {
        // The setup inventory can predate a wakeup. The read still verifies the
        // route identity and probes the live protocol before querying settings.
        target.state = DeviceState::Unknown;
    }
    checked_settings(read_device_info(target).await?)
}

async fn read_device_info(mut target: Device) -> Result<Value> {
    let mut cloned = target.clone();
    let details = match tokio::task::spawn_blocking(move || {
        let details =
            device::settings(&mut cloned).map(|settings| (settings, remap::controls(&cloned)));
        (cloned, details)
    })
    .await
    .context("device settings worker failed")
    {
        Ok((updated, details)) => {
            target = updated;
            details
        }
        Err(error) => Err(error),
    };
    if details.is_ok() {
        target.state = DeviceState::Online;
    }
    let mut result = json!({"device": target, "settings": null});
    match details {
        Ok((settings, controls)) => {
            result["settings"] = serde_json::to_value(settings)?;
            match controls {
                Ok(controls) => result["controls"] = serde_json::to_value(controls)?,
                Err(error) => result["controls_error"] = json!(format!("{error:#}")),
            }
        }
        Err(error) => result["settings_error"] = json!(format!("{error:#}")),
    }
    Ok(result)
}

fn parse_thumb_wheel_interval(value: &str, temporary: bool) -> Result<u16> {
    ensure!(
        !temporary,
        "thumb-wheel-interval controls saved bindings and does not support --temporary"
    );
    let interval: u16 = value
        .parse()
        .context("thumb-wheel-interval must be an integer in milliseconds")?;
    ensure!(
        interval <= config::MAX_THUMB_WHEEL_INTERVAL_MS,
        "thumb-wheel-interval must be between 0 and {} milliseconds",
        config::MAX_THUMB_WHEEL_INTERVAL_MS
    );
    Ok(interval)
}

pub async fn set(
    path: &Path,
    selector: &str,
    key: &str,
    value: &str,
    temporary: bool,
) -> Result<Value> {
    if key == "thumb-wheel-interval" {
        parse_thumb_wheel_interval(value, temporary)?;
    } else {
        config::validate_setting(key, value)?;
    }
    CommandContext::open(path)
        .await?
        .set(path, selector, key, value, temporary)
        .await
}

pub async fn apply(path: &Path, selector: Option<&str>) -> Result<Value> {
    let command = CommandContext::open(path).await?;
    let targets = match selector {
        Some(selector) => vec![command.target(selector)?],
        None => command
            .inventory
            .devices
            .iter()
            .filter(|device| command.config.devices.contains_key(&device.id))
            .cloned()
            .collect(),
    };
    let mut applied = Vec::new();
    let mut failures = Vec::new();
    for target in targets {
        let Some(saved) = command.config.devices.get(&target.id) else {
            continue;
        };
        match apply_device(target.clone(), saved.settings.clone()).await {
            Ok(true) => applied.push(target.id),
            Ok(false) => {}
            Err(error) => failures.push(format!("{}: {error:#}", target.id)),
        }
    }
    if !failures.is_empty() {
        bail!(
            "applied settings to [{}]; failures: {}",
            applied.join(", "),
            failures.join("; ")
        );
    }
    Ok(json!({ "applied": applied }))
}

pub async fn bind(path: &Path, selector: &str, button: &str, action: &str) -> Result<Value> {
    let result = update_bindings(
        path,
        selector,
        &BTreeMap::from([(button.to_owned(), Some(action.to_owned()))]),
    )
    .await?;
    Ok(json!({"device":result["device"],"button":button,"action":action,"saved":true}))
}

pub async fn unbind(path: &Path, selector: &str, button: &str) -> Result<Value> {
    let result =
        update_bindings(path, selector, &BTreeMap::from([(button.to_owned(), None)])).await?;
    Ok(json!({"device":result["device"],"button":button,"removed":true}))
}

pub async fn update_bindings(
    path: &Path,
    selector: &str,
    edits: &BTreeMap<String, Option<String>>,
) -> Result<Value> {
    CommandContext::open(path)
        .await?
        .update_bindings(path, selector, edits)
        .await
}

pub async fn alias(path: &Path, selector: &str, alias: &str) -> Result<Value> {
    CommandContext::open(path)
        .await?
        .alias(path, selector, alias)
        .await
}

async fn apply_device(target: Device, settings: BTreeMap<String, String>) -> Result<bool> {
    if settings.is_empty() {
        return Ok(false);
    }
    if target.state != DeviceState::Online {
        bail!("device is not online; saved settings were not applied");
    }
    tokio::task::spawn_blocking(move || {
        let mut errors = Vec::new();
        for (key, value) in config::ordered_settings(&settings) {
            if let Err(error) = device::set_setting(&target, &key, &value) {
                errors.push(format!("{key}: {error:#}"));
            }
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        Ok(true)
    })
    .await?
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeGeneration {
    filesystem: u64,
    inode: u64,
    device: u64,
    changed: i64,
    changed_ns: i64,
}

fn node_generation(path: &Path) -> Option<NodeGeneration> {
    fs::metadata(path).ok().map(|metadata| NodeGeneration {
        filesystem: metadata.dev(),
        inode: metadata.ino(),
        device: metadata.rdev(),
        changed: metadata.ctime(),
        changed_ns: metadata.ctime_nsec(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MappingRoute {
    hid_path: Option<String>,
    slot: Option<u8>,
    node: Option<NodeGeneration>,
    sysfs_path: Option<PathBuf>,
    sysfs_node: Option<NodeGeneration>,
}

impl MappingRoute {
    fn from_device(device: &Device) -> Self {
        Self::current(device.hid_path.clone(), device.slot)
    }

    fn current(hid_path: Option<String>, slot: Option<u8>) -> Self {
        let hid = hid_path.as_deref().map(Path::new);
        let sysfs_path = hid.and_then(Path::file_name).and_then(|name| {
            fs::canonicalize(Path::new("/sys/class/hidraw").join(name).join("device")).ok()
        });
        Self {
            node: hid.and_then(node_generation),
            sysfs_node: sysfs_path.as_deref().and_then(node_generation),
            sysfs_path,
            hid_path,
            slot,
        }
    }
}

struct MappingWorker {
    bindings: BTreeMap<String, String>,
    thumb_wheel_interval_ms: Arc<AtomicU16>,
    route: MappingRoute,
    stop: watch::Sender<bool>,
    handle: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl MappingWorker {
    fn finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(|handle| handle.is_finished())
    }

    fn reusable(&self, saved: &config::DeviceConfig, route: &MappingRoute) -> bool {
        !self.finished()
            && !*self.stop.borrow()
            && self.bindings == saved.bindings
            && self.route.node.is_some()
            && self.route == *route
    }

    async fn finish(mut self) -> Result<()> {
        self.stop.send_replace(true);
        if let Some(handle) = self.handle.take() {
            handle.await.context("remapping worker failed")??;
        }
        Ok(())
    }
}

impl Drop for MappingWorker {
    fn drop(&mut self) {
        // Also request cleanup if an unexpected error unwinds the coordinator.
        self.stop.send_replace(true);
    }
}

struct MappingRetry {
    bindings: BTreeMap<String, String>,
    thumb_wheel_interval_ms: u16,
    route: MappingRoute,
    after: Instant,
}

impl MappingRetry {
    fn blocks(
        &self,
        bindings: &BTreeMap<String, String>,
        thumb_wheel_interval_ms: u16,
        route: &MappingRoute,
        now: Instant,
    ) -> bool {
        self.bindings == *bindings
            && self.thumb_wheel_interval_ms == thumb_wheel_interval_ms
            && self.route == *route
            && now < self.after
    }
}

async fn wait_for_mapping_stop(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

/// Forward shutdown without cancelling blocking HID work or its cleanup.
async fn drain_mapping(
    worker: impl Future<Output = Result<()>>,
    stop: watch::Sender<bool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    if *shutdown.borrow() || shutdown.has_changed().is_err() {
        stop.send_replace(true);
    }
    tokio::pin!(worker);
    tokio::select! {
        result = &mut worker => result,
        _ = wait_for_mapping_stop(&mut shutdown) => {
            stop.send_replace(true);
            worker.await
        }
    }
}

struct MappingState {
    config: config::Config,
    selector: Option<String>,
    selected_id: Option<String>,
    workers: BTreeMap<String, MappingWorker>,
    retry: BTreeMap<String, MappingRetry>,
    warnings: Vec<String>,
    active_guard: Option<ControllerLock>,
}

fn mapping_selection(
    config: &config::Config,
    inventory: &Inventory,
    selector: &str,
) -> Result<String> {
    // A saved identity/alias remains meaningful while its device is unplugged.
    if config.devices.contains_key(selector) {
        return Ok(selector.to_owned());
    }
    if let Some((id, _)) = config.devices.iter().find(|(_, saved)| {
        saved
            .alias
            .as_ref()
            .is_some_and(|alias| alias.eq_ignore_ascii_case(selector))
    }) {
        return Ok(id.clone());
    }
    Ok(select(inventory, config, selector)?.id.clone())
}

impl MappingState {
    fn new(config: config::Config, selector: Option<String>) -> Self {
        Self {
            config,
            selector,
            selected_id: None,
            workers: BTreeMap::new(),
            retry: BTreeMap::new(),
            warnings: Vec::new(),
            active_guard: None,
        }
    }

    fn failure(
        &mut self,
        id: String,
        bindings: BTreeMap<String, String>,
        thumb_wheel_interval_ms: u16,
        route: MappingRoute,
        error: &str,
    ) {
        tracing::warn!("remapping {id}: {error}; retrying in 30 seconds");
        self.retry.insert(
            id,
            MappingRetry {
                bindings,
                thumb_wheel_interval_ms,
                route,
                after: Instant::now() + Duration::from_secs(30),
            },
        );
    }

    fn needs_inventory(&self) -> bool {
        self.selector.is_some() && self.selected_id.is_none()
            || self.config.devices.iter().any(|(id, saved)| {
                !saved.bindings.is_empty()
                    && self
                        .selected_id
                        .as_ref()
                        .is_none_or(|selected| selected == id)
            })
    }

    fn can_reuse_workers(&self, config: &config::Config) -> bool {
        if self.selector.is_some() && self.selected_id.is_none() {
            return false;
        }
        let mut count = 0;
        for (id, saved) in &config.devices {
            if saved.bindings.is_empty()
                || self
                    .selected_id
                    .as_ref()
                    .is_some_and(|selected| selected != id)
            {
                continue;
            }
            let Some(worker) = self.workers.get(id) else {
                return false;
            };
            let current = MappingRoute::current(worker.route.hid_path.clone(), worker.route.slot);
            if !worker.reusable(saved, &current) {
                return false;
            }
            count += 1;
        }
        count == self.workers.len() && (count == 0 || self.active_guard.is_some())
    }

    fn update_intervals(&self) {
        for (id, worker) in &self.workers {
            if let Some(saved) = self.config.devices.get(id) {
                // Software-only timing changes must not release and reclaim
                // hardware diversion or recreate the virtual input device.
                worker
                    .thumb_wheel_interval_ms
                    .store(saved.thumb_wheel_interval_ms, Ordering::Relaxed);
            }
        }
    }

    fn mapping_targets(&self, devices: Vec<Device>) -> BTreeMap<String, Device> {
        devices
            .into_iter()
            .filter(|device| {
                let Some(saved) = self.config.devices.get(&device.id) else {
                    return false;
                };
                !saved.bindings.is_empty()
                    && self.selected_id.as_ref().is_none_or(|id| id == &device.id)
                    && (device.state == DeviceState::Online
                        || device.state == DeviceState::Unknown
                            && self.workers.get(&device.id).is_some_and(|worker| {
                                worker.reusable(saved, &MappingRoute::from_device(device))
                            }))
            })
            .map(|device| (device.id.clone(), device))
            .collect()
    }

    async fn reconcile(
        &mut self,
        directory: &Path,
        shutdown: &watch::Receiver<bool>,
        command: ControllerLock,
    ) -> Result<()> {
        if *shutdown.borrow() {
            return Ok(());
        }
        // No mappings means no device or uinput access is necessary.
        let inventory = if self.needs_inventory() {
            discover(device::Discovery::Control).await?
        } else {
            Inventory::default()
        };
        if *shutdown.borrow() {
            return Ok(());
        }
        if self.selected_id.is_none()
            && let Some(selector) = &self.selector
        {
            self.selected_id = Some(mapping_selection(&self.config, &inventory, selector)?);
        }
        let mut warnings = inventory.warnings;
        for device in &inventory.devices {
            warnings.extend(
                device
                    .warnings
                    .iter()
                    .map(|warning| format!("{}: {warning}", device.id)),
            );
        }
        warnings.sort();
        warnings.dedup();
        for warning in &warnings {
            if self.warnings.binary_search(warning).is_err() {
                tracing::warn!("discovery: {warning}");
            }
        }
        self.warnings = warnings;

        let targets = self.mapping_targets(inventory.devices);
        let routes: BTreeMap<_, _> = targets
            .iter()
            .map(|(id, device)| (id.clone(), MappingRoute::from_device(device)))
            .collect();
        if self.active_guard.is_none() && !targets.is_empty() {
            self.active_guard = Some(
                try_lock_file(&directory.join("remap-active.lock"))?
                    .context("receiver pairing is active; remapping cannot start yet")?,
            );
        }
        // Active ownership was established while holding the command lock.
        // Workers now take that lock themselves around SID-less receiver reads;
        // never retain it while awaiting their initialization or cleanup.
        drop(command);
        self.retry.retain(|id, retry| {
            routes.get(id) == Some(&retry.route)
                && self.config.devices[id].bindings == retry.bindings
                && self.config.devices[id].thumb_wheel_interval_ms == retry.thumb_wheel_interval_ms
        });
        let removed: Vec<_> = self
            .workers
            .iter()
            .filter(|(id, worker)| {
                worker.finished()
                    || routes.get(*id) != Some(&worker.route)
                    || self.config.devices[*id].bindings != worker.bindings
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in removed {
            if let Some(worker) = self.workers.remove(&id) {
                let unexpectedly_finished = worker.finished();
                let bindings = worker.bindings.clone();
                let thumb_wheel_interval_ms =
                    worker.thumb_wheel_interval_ms.load(Ordering::Relaxed);
                let route = worker.route.clone();
                let failure = match worker.finish().await {
                    Ok(()) if unexpectedly_finished => Some("mapping worker ended".to_owned()),
                    Ok(()) => None,
                    Err(error) => Some(format!("{error:#}")),
                };
                if let Some(error) = failure {
                    if routes.get(&id) == Some(&route) {
                        self.failure(id, bindings, thumb_wheel_interval_ms, route, &error);
                    } else {
                        tracing::warn!("remapping {id}: {error}");
                    }
                }
            }
        }
        for (id, target) in targets {
            if *shutdown.borrow() {
                break;
            }
            if target.state != DeviceState::Online || self.workers.contains_key(&id) {
                continue;
            }
            let bindings = self.config.devices[&id].bindings.clone();
            let thumb_wheel_interval_ms = self.config.devices[&id].thumb_wheel_interval_ms;
            let route = routes[&id].clone();
            if self.retry.get(&id).is_some_and(|retry| {
                retry.blocks(&bindings, thumb_wheel_interval_ms, &route, Instant::now())
            }) {
                continue;
            }
            if let Err(error) = crate::model::require_remappable(&target) {
                self.failure(
                    id,
                    bindings,
                    thumb_wheel_interval_ms,
                    route,
                    &format!("{error:#}"),
                );
                continue;
            }
            let (stop, receiver) = watch::channel(false);
            let thumb_wheel_interval_ms = Arc::new(AtomicU16::new(thumb_wheel_interval_ms));
            let handle = tokio::spawn(drain_mapping(
                remap::run(
                    target,
                    bindings.clone(),
                    thumb_wheel_interval_ms.clone(),
                    receiver,
                ),
                stop.clone(),
                shutdown.clone(),
            ));
            self.workers.insert(
                id,
                MappingWorker {
                    bindings,
                    thumb_wheel_interval_ms,
                    route,
                    stop,
                    handle: Some(handle),
                },
            );
        }
        if self.workers.is_empty() {
            self.active_guard = None;
        }
        self.update_intervals();
        Ok(())
    }

    async fn stop(&mut self) {
        for worker in self.workers.values() {
            worker.stop.send_replace(true);
        }
        for (id, worker) in std::mem::take(&mut self.workers) {
            if let Err(error) = worker.finish().await {
                tracing::warn!("remapping cleanup {id}: {error:#}");
            }
        }
        self.active_guard = None;
    }
}

/// Its caller owns remap.lock, so stale cleanup cannot unlink a live daemon.
struct ReloadSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl ReloadSocket {
    fn bind(directory: &Path) -> Result<Self> {
        let path = directory.join("daemon.sock");
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket(),
                    "{} is not a socket",
                    path.display()
                );
                fs::remove_file(&path).context("remove stale daemon socket")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect daemon socket"),
        }
        let socket = Self {
            listener: UnixListener::bind(&path).context("open daemon reload socket")?,
            path,
        };
        fs::set_permissions(&socket.path, fs::Permissions::from_mode(0o600))?;
        Ok(socket)
    }
}

impl Drop for ReloadSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Request explicit adoption of saved bindings without holding the command lock:
/// the daemon needs that lock while changing and cleaning up its mapping workers.
pub async fn reload(path: &Path) -> Result<Value> {
    config::load(path)?;
    request_reload(&runtime_dir()?.join("daemon.sock"), Duration::from_secs(60)).await
}

/// Call after saving and releasing the command lock. An absent daemon will read
/// the saved config on startup; a running daemon must acknowledge the reload.
pub async fn reload_if_running(path: &Path) -> Result<()> {
    async { reload_if_running_in(path, &runtime_dir()?).await }
        .await
        .context("configuration saved, but the daemon could not reload it")
}

async fn reload_if_running_in(path: &Path, directory: &Path) -> Result<()> {
    if try_lock_file(&directory.join("remap.lock"))?.is_some() {
        return Ok(());
    }
    config::load(path)?;
    request_reload(&directory.join("daemon.sock"), Duration::from_secs(60)).await?;
    Ok(())
}

async fn request_reload(socket: &Path, timeout: Duration) -> Result<Value> {
    tokio::time::timeout(timeout, async {
        let mut stream = UnixStream::connect(socket).await.map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                anyhow::anyhow!("logishell daemon is not running")
            } else {
                anyhow::Error::new(error).context("connect to logishell daemon")
            }
        })?;
        // A single operation byte avoids an unbounded request parser.
        stream.write_all(b"R").await?;
        let mut response = Vec::new();
        stream.take(64 * 1024).read_to_end(&mut response).await?;
        ensure!(
            !response.is_empty(),
            "daemon closed without acknowledging the reload"
        );
        let response: Value =
            serde_json::from_slice(&response).context("invalid daemon reload response")?;
        if let Some(error) = response["error"].as_str() {
            bail!("{error}");
        }
        ensure!(
            response["reloaded"] == true,
            "daemon did not acknowledge the reload"
        );
        Ok(response)
    })
    .await
    .context("daemon reload acknowledgement timed out; the reload may still finish")?
}

async fn serve_reload(
    mut stream: UnixStream,
    state: &mut MappingState,
    path: &Path,
    directory: &Path,
    shutdown: &watch::Receiver<bool>,
) -> Result<bool> {
    let mut stopping = shutdown.clone();
    let mut request = [0];
    tokio::select! {
        _ = wait_for_mapping_stop(&mut stopping) => bail!("daemon is stopping"),
        result = tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut request)) => {
            result.context("daemon reload request timed out")??;
        }
    }
    let mut reconciled = false;
    let result = async {
        ensure!(request == *b"R", "unknown daemon request");
        let command_path = directory.join("controller.lock");
        let command = tokio::select! {
            _ = wait_for_mapping_stop(&mut stopping) => bail!("daemon is stopping"),
            command = wait_for_command_lock(&command_path, Duration::from_secs(10)) => command?,
        };
        // Read and validate before replacing the snapshot. Reconciliation's
        // fallible discovery/selection phase also precedes any worker changes.
        let loaded = config::load(path)?;
        if state.can_reuse_workers(&loaded) {
            state.config = loaded;
            state.update_intervals();
        } else {
            let previous = std::mem::replace(&mut state.config, loaded);
            let selected = state.selected_id.clone();
            reconciled = true;
            if let Err(error) = state.reconcile(directory, shutdown, command).await {
                state.config = previous;
                state.selected_id = selected;
                return Err(error);
            }
        }
        ensure!(!*shutdown.borrow(), "daemon stopped before completing the reload");
        Ok::<_, anyhow::Error>(json!({
            "reloaded": true,
            "path": path,
            "devices": state.config.devices.values().filter(|saved| !saved.bindings.is_empty()).count()
        }))
    }.await;
    let response = match result {
        Ok(response) => response,
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    // A disconnected client never cancels the reconciliation or its cleanup.
    tokio::time::timeout(
        Duration::from_secs(2),
        stream.write_all(&serde_json::to_vec(&response)?),
    )
    .await
    .context("daemon reload response timed out")??;
    Ok(reconciled)
}

struct MappingSignals(tokio::task::JoinHandle<()>);
impl Drop for MappingSignals {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Keep software mappings active until interrupted. This never applies saved
/// hardware settings and does not own the normal command lock while idle.
pub async fn run_remapping(path: PathBuf, selector: Option<String>) -> Result<()> {
    let directory = runtime_dir()?;
    let _runner = try_lock_file(&directory.join("remap.lock"))?
        .context("another logishell daemon is already running")?;
    let mut state = MappingState::new(config::load(&path)?, selector);
    let socket = ReloadSocket::bind(&directory)?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let (shutdown, mut receiver) = watch::channel(false);
    let stop = shutdown.clone();
    let _signals = MappingSignals(tokio::spawn(async move {
        tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
        stop.send_replace(true);
    }));
    eprintln!("logishell daemon: running");
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let outcome = async {
        loop {
            tokio::select! {
                _ = wait_for_mapping_stop(&mut receiver) => break,
                incoming = socket.listener.accept() => {
                    let (stream, _) = incoming.context("accept daemon reload request")?;
                    match serve_reload(stream, &mut state, &path, &directory, &receiver).await {
                        Ok(true) => ticker.reset(),
                        Ok(false) => {},
                        Err(error) => tracing::warn!("daemon reload: {error:#}"),
                    }
                }
                _ = ticker.tick() => {
                    // A foreground command gets priority. Workers use their own
                    // HID++ software ID, so mappings continue during commands.
                    if let Some(command) = try_lock_file(&directory.join("controller.lock"))? {
                        state.reconcile(&directory, &receiver, command).await?;
                    }
                    // Slow discovery must leave a quiet interval for commands,
                    // rather than immediately consuming another overdue tick.
                    ticker.reset();
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    shutdown.send_replace(true);
    state.stop().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Transport;

    fn device(id: &str) -> Device {
        Device {
            id: id.into(),
            name: "same mouse".into(),
            transport: Transport::Usb,
            state: DeviceState::Online,
            ..Default::default()
        }
    }

    #[test]
    fn nickname_batches_allow_transfers_and_swaps_without_partial_saves() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.toml");
        let mut original = config::Config::default();
        for (id, alias) in [("usb:first", "first"), ("usb:second", "second")] {
            let saved = original.devices.entry(id.into()).or_default();
            saved.alias = Some(alias.into());
            saved.settings.insert("dpi".into(), "1600".into());
            saved.bindings.insert("back".into(), "key:ctrl+c".into());
        }
        original
            .devices
            .insert("absent".into(), config::DeviceConfig::default());
        let command = CommandContext {
            _lock: lock_file(&directory.path().join("controller.lock"))?,
            config: original.clone(),
            inventory: Inventory {
                devices: vec![device("usb:first"), device("usb:second")],
                ..Default::default()
            },
        };
        for second_alias in ["third", "first"] {
            config::save(&path, &original)?;
            command.aliases(
                &path,
                &BTreeMap::from([
                    ("usb:first".into(), "second".into()),
                    ("usb:second".into(), second_alias.into()),
                ]),
            )?;
            let mut expected = original.clone();
            expected
                .devices
                .get_mut("usb:first")
                .context("first device")?
                .alias = Some("second".into());
            expected
                .devices
                .get_mut("usb:second")
                .context("second device")?
                .alias = Some(second_alias.into());
            assert_eq!(config::load(&path)?, expected);
        }
        let before = fs::read(&path)?;
        for invalid in [
            BTreeMap::from([
                ("usb:first".into(), "duplicate".into()),
                ("usb:second".into(), "DUPLICATE".into()),
            ]),
            BTreeMap::from([
                ("usb:first".into(), "changed".into()),
                ("usb:missing".into(), "missing".into()),
            ]),
        ] {
            assert!(command.aliases(&path, &invalid).is_err());
            assert_eq!(fs::read(&path)?, before);
        }
        Ok(())
    }

    #[tokio::test]
    async fn thumb_wheel_interval_commands_save_and_read_without_hardware_writes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.toml");
        let mut original = config::Config::default();
        let saved = original.devices.entry("usb:mouse".into()).or_default();
        saved.alias = Some("mouse".into());
        saved.settings.insert("dpi".into(), "1600".into());
        saved
            .bindings
            .insert("thumb-left".into(), "key:ctrl+pageup".into());
        original
            .devices
            .insert("other".into(), config::DeviceConfig::default());
        config::save(&path, &original)?;
        let context = || -> Result<CommandContext> {
            Ok(CommandContext {
                _lock: lock_file(&directory.path().join("controller.lock"))?,
                config: config::load(&path)?,
                inventory: Inventory {
                    devices: vec![Device {
                        state: DeviceState::Offline,
                        hid_path: Some("/not-a-real-hidraw-node".into()),
                        ..device("usb:mouse")
                    }],
                    ..Default::default()
                },
            })
        };
        let command = context()?;
        assert_eq!(
            command.setting("mouse", "thumb-wheel-interval").await?,
            json!(0)
        );
        let result = command
            .set(&path, "mouse", "thumb-wheel-interval", "200", false)
            .await?;
        assert_eq!(result["value"], 200);
        assert_eq!(result["saved"], true);
        drop(command);
        let command = context()?;
        assert_eq!(
            command.setting("usb:mouse", "thumb-wheel-interval").await?,
            json!(200)
        );
        let mut expected = original;
        expected
            .devices
            .get_mut("usb:mouse")
            .context("fixture")?
            .thumb_wheel_interval_ms = 200;
        assert_eq!(config::load(&path)?, expected);
        let before = fs::read(&path)?;
        for (value, temporary) in [
            ("5001", false),
            ("-1", false),
            ("fast", false),
            ("250", true),
        ] {
            assert!(
                command
                    .set(&path, "mouse", "thumb-wheel-interval", value, temporary)
                    .await
                    .is_err()
            );
            assert_eq!(fs::read(&path)?, before);
        }
        assert!(config::validate_setting("thumb-wheel-interval", "200").is_err());
        Ok(())
    }

    #[tokio::test]
    async fn applying_empty_settings_does_not_require_an_online_device() -> Result<()> {
        let mut target = device("alias-only-device");
        target.state = DeviceState::Offline;
        assert!(!apply_device(target.clone(), BTreeMap::new()).await?);
        let error = apply_device(target, BTreeMap::from([("dpi".into(), "1000".into())]))
            .await
            .expect_err("actual hardware settings still require an online device");
        assert!(error.to_string().contains("not online"));
        Ok(())
    }

    #[test]
    fn ambiguous_names_never_select_arbitrarily() -> Result<()> {
        let inventory = Inventory {
            devices: vec![device("one"), device("two")],
            ..Default::default()
        };
        let mut config = config::Config::default();
        assert!(select(&inventory, &config, "mouse").is_err());
        config.devices.insert(
            "two".into(),
            config::DeviceConfig {
                alias: Some("mouse".into()),
                ..Default::default()
            },
        );
        assert_eq!(select(&inventory, &config, "mouse")?.id, "two");
        Ok(())
    }

    #[test]
    fn recognized_model_selectors_survive_pairing_name_changes() -> Result<()> {
        let mut keyboard = device("keyboard-id");
        for reported_name in ["MX MCHNCL M", "MX Mechanical Mini"] {
            keyboard.name = reported_name.into();
            let inventory = Inventory {
                devices: vec![keyboard.clone()],
                ..Default::default()
            };
            for selector in ["MX MCHNCL M", "mx mechanical mini", "mx-mechanical-mini"] {
                assert_eq!(
                    select(&inventory, &config::Config::default(), selector)?.id,
                    "keyboard-id"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn model_selectors_remain_ambiguous_and_preserve_explicit_identity() -> Result<()> {
        let mut first = device("first");
        first.name = "MX MCHNCL M".into();
        let mut second = device("second");
        second.name = "MX Mechanical Mini".into();
        let mut inventory = Inventory {
            devices: vec![first, second],
            ..Default::default()
        };
        let mut config = config::Config::default();
        for selector in ["MX MCHNCL M", "MX Mechanical Mini"] {
            let error = select(&inventory, &config, selector).expect_err("two physical keyboards");
            assert!(error.to_string().contains("ambiguous"));
        }
        assert_eq!(select(&inventory, &config, "second")?.id, "second");
        config.devices.insert(
            "first".into(),
            config::DeviceConfig {
                alias: Some("mx-mechanical-mini".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            select(&inventory, &config, "mx-mechanical-mini")?.id,
            "first"
        );
        inventory.devices.push(device("mx-mchncl-m"));
        assert_eq!(
            select(&inventory, &config, "mx-mchncl-m")?.id,
            "mx-mchncl-m"
        );
        Ok(())
    }

    #[test]
    fn settings_restore_threshold_before_enable_and_wheel_mode_last() {
        let settings = ["wheel-mode", "smartshift", "dpi", "smartshift-sensitivity"]
            .into_iter()
            .map(|name| (name.to_owned(), "value".to_owned()))
            .collect();
        let keys: Vec<_> = config::ordered_settings(&settings)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(
            keys,
            ["dpi", "smartshift-sensitivity", "smartshift", "wheel-mode"]
        );
    }

    #[test]
    fn missing_alias_target_never_falls_back_to_another_matching_device() {
        let inventory = Inventory {
            devices: vec![device("other")],
            ..Default::default()
        };
        let mut config = config::Config::default();
        config.devices.insert(
            "absent".into(),
            config::DeviceConfig {
                alias: Some("mouse".into()),
                ..Default::default()
            },
        );
        for selector in ["mouse", "MOUSE"] {
            let error = select(&inventory, &config, selector).expect_err("alias target absent");
            assert!(error.to_string().contains("absent"));
            assert!(error.to_string().contains("not currently detected"));
        }
        assert_eq!(
            select(&inventory, &config, "other")
                .expect("explicit ID")
                .id,
            "other"
        );
        for selector in ["", " ", "\t\n"] {
            assert!(
                select(&inventory, &config, selector)
                    .expect_err("empty selector")
                    .to_string()
                    .contains("cannot be empty")
            );
        }
    }

    #[test]
    fn command_lock_is_exclusive_and_released_on_drop() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("controller.lock");
        let first = lock_file(&path)?;
        let second = lock_file(&path);
        assert!(second.is_err());
        assert!(
            second
                .err()
                .expect("lock contention")
                .to_string()
                .contains("another logishell command")
        );
        drop(first);
        let _next = lock_file(&path)?;
        Ok(())
    }

    #[tokio::test]
    async fn command_waits_for_brief_discovery_and_times_out_on_long_contention() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("controller.lock");
        let held = lock_file(&path)?;
        let (acquired, ()) = tokio::join!(
            wait_for_command_lock(&path, Duration::from_secs(1)),
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(held);
            }
        );
        let _acquired = acquired?;
        let failure = wait_for_command_lock(&path, Duration::from_millis(10))
            .await
            .err()
            .expect("lock remains held");
        assert!(failure.to_string().contains("still active"));
        Ok(())
    }

    #[tokio::test]
    async fn idle_daemon_keeps_commands_and_pairing_available_without_devices() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let _runner = lock_file(&directory.path().join("remap.lock"))?;
        let mut config = config::Config::default();
        config.devices.insert(
            "absent-mouse".into(),
            config::DeviceConfig {
                settings: BTreeMap::from([("dpi".into(), "1600".into())]),
                ..Default::default()
            },
        );
        let path = directory.path().join("config.toml");
        config::save(&path, &config)?;
        let mut state = MappingState::new(config.clone(), None);
        assert!(
            !state.needs_inventory(),
            "hardware settings alone do not trigger discovery or application"
        );
        fs::write(&path, "schema_version = 700\n")?;
        let (_stop, receiver) = watch::channel(false);
        let command = lock_file(&directory.path().join("controller.lock"))?;
        state
            .reconcile(directory.path(), &receiver, command)
            .await?;
        assert_eq!(
            state.config, config,
            "reconnect ticks use the cached config"
        );
        assert!(state.workers.is_empty());
        assert!(state.active_guard.is_none());
        let _command = lock_file(&directory.path().join("controller.lock"))?;
        let _pairing = lock_file(&directory.path().join("remap-active.lock"))?;
        Ok(())
    }

    async fn exchange_reload(
        socket: &ReloadSocket,
        state: &mut MappingState,
        path: &Path,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<Value> {
        let (response, served) = tokio::join!(
            request_reload(&socket.path, Duration::from_secs(1)),
            async {
                let (stream, _) = socket.listener.accept().await?;
                serve_reload(
                    stream,
                    state,
                    path,
                    socket.path.parent().expect("socket directory"),
                    shutdown,
                )
                .await
            }
        );
        served?;
        response
    }

    #[tokio::test]
    async fn saved_config_reload_skips_absent_daemon_even_with_stale_socket() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.toml");
        config::save(&path, &config::Config::default())?;
        reload_if_running_in(&path, directory.path()).await?;
        let stale = UnixListener::bind(directory.path().join("daemon.sock"))?;
        drop(stale);
        reload_if_running_in(&path, directory.path()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn saved_config_reload_requires_running_daemon_acknowledgement() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.toml");
        config::save(&path, &config::Config::default())?;
        let _runner = lock_file(&directory.path().join("remap.lock"))?;
        let socket = ReloadSocket::bind(directory.path())?;
        for (reply, error) in [
            (r#"{"reloaded":true}"#, None),
            (
                r#"{"error":"bindings rejected"}"#,
                Some("bindings rejected"),
            ),
            ("invalid response", Some("invalid daemon reload response")),
            (r#"{"reloaded":false}"#, Some("did not acknowledge")),
            ("", Some("closed without acknowledging")),
        ] {
            let (reloaded, served) =
                tokio::join!(reload_if_running_in(&path, directory.path()), async {
                    let (mut stream, _) = socket.listener.accept().await?;
                    let mut request = [0];
                    stream.read_exact(&mut request).await?;
                    assert_eq!(request, *b"R");
                    stream.write_all(reply.as_bytes()).await?;
                    Ok::<_, anyhow::Error>(())
                });
            served?;
            if let Some(expected) = error {
                assert!(
                    reloaded
                        .expect_err("reload must fail")
                        .to_string()
                        .contains(expected)
                );
            } else {
                reloaded?;
            }
            assert_eq!(config::load(&path)?, config::Config::default());
        }
        Ok(())
    }

    #[tokio::test]
    async fn invalid_mapping_reload_retains_last_valid_bindings_and_recovers() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let _runner = lock_file(&directory.path().join("remap.lock"))?;
        let socket = ReloadSocket::bind(directory.path())?;
        let path = directory.path().join("config.toml");
        let mut initial = config::Config::default();
        initial.devices.insert(
            "mouse".into(),
            config::DeviceConfig {
                bindings: BTreeMap::from([("back".into(), "key:ctrl+c".into())]),
                ..Default::default()
            },
        );
        let mut state = MappingState::new(initial.clone(), None);
        let (_stop, shutdown) = watch::channel(false);
        fs::write(&path, "schema_version = 700\n")?;
        for _ in 0..2 {
            let error = exchange_reload(&socket, &mut state, &path, &shutdown)
                .await
                .expect_err("invalid reload");
            assert!(error.to_string().contains("700"));
            assert_eq!(state.config, initial);
        }
        config::save(&path, &config::Config::default())?;
        let response = exchange_reload(&socket, &mut state, &path, &shutdown).await?;
        assert_eq!(response["reloaded"], true);
        assert_eq!(response["devices"], 0);
        assert_eq!(state.config, config::Config::default());
        Ok(())
    }

    #[tokio::test]
    async fn reload_without_mapping_changes_preserves_the_discovery_deadline() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.toml");
        let mut config = config::Config::default();
        config.devices.entry("mouse".into()).or_default().alias = Some("renamed".into());
        config::save(&path, &config)?;
        let mut state = MappingState::new(config::Config::default(), None);
        let (_stop, shutdown) = watch::channel(false);
        let (server, mut client) = UnixStream::pair()?;
        client.write_all(b"R").await?;
        assert!(
            !serve_reload(server, &mut state, &path, directory.path(), &shutdown).await?,
            "a fast reload must not reset the periodic discovery deadline"
        );
        let mut response = Vec::new();
        client.read_to_end(&mut response).await?;
        assert_eq!(
            serde_json::from_slice::<Value>(&response)?["reloaded"],
            true
        );
        assert_eq!(state.config, config);
        assert!(state.workers.is_empty() && state.active_guard.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn reload_acknowledgement_waits_for_command_lock_and_worker_cleanup() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let _runner = lock_file(&directory.path().join("remap.lock"))?;
        let socket = ReloadSocket::bind(directory.path())?;
        let path = directory.path().join("config.toml");
        config::save(&path, &config::Config::default())?;
        let mut state = MappingState::new(config::Config::default(), None);
        let (stop, mut stopped) = watch::channel(false);
        let (observed, observation) = tokio::sync::oneshot::channel();
        let (finish, finished) = tokio::sync::oneshot::channel();
        state.workers.insert(
            "mouse".into(),
            MappingWorker {
                bindings: BTreeMap::new(),
                thumb_wheel_interval_ms: Arc::new(AtomicU16::new(0)),
                route: MappingRoute::from_device(&device("mouse")),
                stop,
                handle: Some(tokio::spawn(async move {
                    wait_for_mapping_stop(&mut stopped).await;
                    let _ = observed.send(());
                    finished.await?;
                    Ok(())
                })),
            },
        );
        let held = lock_file(&directory.path().join("controller.lock"))?;
        let (_stop, shutdown) = watch::channel(false);
        let response = {
            let reload = exchange_reload(&socket, &mut state, &path, &shutdown);
            tokio::pin!(reload);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut reload)
                    .await
                    .is_err()
            );
            drop(held);
            tokio::select! {
                result = &mut reload => panic!("acknowledged before cleanup: {result:?}"),
                result = observation => result?,
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut reload)
                    .await
                    .is_err()
            );
            // Cleanup can acquire the command lock; the reloader must release it.
            let _cleanup_lock = lock_file(&directory.path().join("controller.lock"))?;
            finish.send(()).expect("cleanup waiting");
            reload.await?
        };
        assert_eq!(response["reloaded"], true);
        assert!(state.workers.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn reload_socket_cleans_up_stale_paths_and_client_waits_are_bounded() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let _runner = lock_file(&directory.path().join("remap.lock"))?;
        let path = directory.path().join("daemon.sock");
        let error = request_reload(&path, Duration::from_millis(20))
            .await
            .expect_err("daemon absent");
        assert!(error.to_string().contains("not running"));
        let stale = UnixListener::bind(&path)?;
        drop(stale);
        let socket = ReloadSocket::bind(directory.path())?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        let error = request_reload(&path, Duration::from_millis(20))
            .await
            .expect_err("listener never responds");
        assert!(error.to_string().contains("timed out"));
        drop(socket);
        assert!(!path.exists());
        fs::write(&path, "not a socket")?;
        assert!(ReloadSocket::bind(directory.path()).is_err());
        assert_eq!(fs::read_to_string(&path)?, "not a socket");
        Ok(())
    }

    #[tokio::test]
    async fn idle_reload_client_cannot_hold_up_shutdown() -> Result<()> {
        let (server, _idle_client) = UnixStream::pair()?;
        let directory = tempfile::tempdir()?;
        let mut state = MappingState::new(config::Config::default(), None);
        let (stop, shutdown) = watch::channel(false);
        stop.send_replace(true);
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            serve_reload(
                server,
                &mut state,
                &directory.path().join("config.toml"),
                directory.path(),
                &shutdown,
            ),
        )
        .await?;
        assert!(
            outcome
                .expect_err("daemon stopping")
                .to_string()
                .contains("stopping")
        );
        Ok(())
    }

    #[tokio::test]
    async fn malformed_clients_are_rejected_and_disconnect_does_not_cancel_reload() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.toml");
        config::save(&path, &config::Config::default())?;
        let mut initial = config::Config::default();
        initial
            .devices
            .insert("mouse".into(), config::DeviceConfig::default());
        let mut state = MappingState::new(initial.clone(), None);
        let (_stop, shutdown) = watch::channel(false);
        let (server, mut client) = UnixStream::pair()?;
        client.write_all(b"?").await?;
        serve_reload(server, &mut state, &path, directory.path(), &shutdown).await?;
        let mut response = Vec::new();
        client.read_to_end(&mut response).await?;
        let response: Value = serde_json::from_slice(&response)?;
        assert_eq!(response["error"], "unknown daemon request");
        assert_eq!(state.config, initial);

        let (server, mut client) = UnixStream::pair()?;
        client.write_all(b"R").await?;
        drop(client);
        assert!(
            serve_reload(server, &mut state, &path, directory.path(), &shutdown)
                .await
                .is_err()
        );
        assert_eq!(state.config, config::Config::default());
        Ok(())
    }

    #[test]
    fn mapping_scope_keeps_saved_absent_identity_instead_of_matching_another_device() -> Result<()>
    {
        let mut config = config::Config::default();
        config.devices.insert(
            "absent".into(),
            config::DeviceConfig {
                alias: Some("mouse".into()),
                ..Default::default()
            },
        );
        let inventory = Inventory {
            devices: vec![device("different")],
            ..Default::default()
        };
        assert_eq!(mapping_selection(&config, &inventory, "MOUSE")?, "absent");
        assert_eq!(mapping_selection(&config, &inventory, "absent")?, "absent");
        Ok(())
    }

    #[tokio::test]
    async fn cached_workers_require_live_bindings_selection_and_unchanged_routes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("test-device");
        fs::write(&path, "first")?;
        let mut target = device("mouse");
        target.hid_path = Some(path.to_string_lossy().into_owned());
        target.slot = Some(1);
        let bindings = BTreeMap::from([("back".into(), "key:ctrl+c".into())]);
        let mut config = config::Config::default();
        config
            .devices
            .entry(target.id.clone())
            .or_default()
            .bindings = bindings.clone();
        let mut state = MappingState::new(config.clone(), None);
        state.active_guard = Some(lock_file(&directory.path().join("remap-active.lock"))?);
        let (stop, mut shutdown) = watch::channel(false);
        state.workers.insert(
            target.id.clone(),
            MappingWorker {
                bindings,
                thumb_wheel_interval_ms: Arc::new(AtomicU16::new(0)),
                route: MappingRoute::from_device(&target),
                stop,
                handle: Some(tokio::spawn(async move {
                    wait_for_mapping_stop(&mut shutdown).await;
                    Ok(())
                })),
            },
        );
        assert!(state.can_reuse_workers(&config));
        let saved = config.devices.get_mut("mouse").expect("fixture");
        saved.alias = Some("renamed".into());
        saved.settings.insert("dpi".into(), "1250".into());
        assert!(
            state.can_reuse_workers(&config),
            "settings and aliases do not change bindings"
        );
        let interval = state.workers["mouse"].thumb_wheel_interval_ms.clone();
        let config_path = directory.path().join("config.toml");
        let (_shutdown, receiver) = watch::channel(false);
        for millis in [250, 150, 0] {
            config
                .devices
                .get_mut("mouse")
                .expect("fixture")
                .thumb_wheel_interval_ms = millis;
            config::save(&config_path, &config)?;
            let (server, mut client) = UnixStream::pair()?;
            client.write_all(b"R").await?;
            assert!(
                !serve_reload(
                    server,
                    &mut state,
                    &config_path,
                    directory.path(),
                    &receiver
                )
                .await?,
                "interval-only reload must preserve the worker and discovery deadline"
            );
            let mut response = Vec::new();
            client.read_to_end(&mut response).await?;
            assert_eq!(
                serde_json::from_slice::<Value>(&response)?["reloaded"],
                true
            );
            assert_eq!(interval.load(Ordering::Relaxed), millis);
            assert!(!*state.workers["mouse"].stop.borrow());
            assert!(state.can_reuse_workers(&config));
        }
        for (status, keep) in [(DeviceState::Unknown, true), (DeviceState::Offline, false)] {
            target.state = status;
            assert_eq!(
                state
                    .mapping_targets(vec![target.clone()])
                    .contains_key("mouse"),
                keep
            );
        }
        target.state = DeviceState::Unknown;
        state.selector = Some("mouse".into());
        assert!(
            !state.can_reuse_workers(&config),
            "unresolved selection must discover"
        );
        state.selected_id = Some("mouse".into());
        assert!(state.can_reuse_workers(&config));
        state
            .config
            .devices
            .get_mut("mouse")
            .expect("fixture")
            .bindings
            .clear();
        assert!(
            !state.can_reuse_workers(&state.config),
            "removed bindings must stop the worker"
        );
        assert!(state.mapping_targets(vec![target.clone()]).is_empty());
        state.config = config.clone();
        config
            .devices
            .get_mut("mouse")
            .expect("fixture")
            .bindings
            .insert("back".into(), "key:ctrl+v".into());
        assert!(
            !state.can_reuse_workers(&config),
            "changed bindings must reconcile"
        );
        state.config = config.clone();
        assert!(
            state.mapping_targets(vec![target.clone()]).is_empty(),
            "unknown devices cannot start changed mappings"
        );
        config
            .devices
            .get_mut("mouse")
            .expect("fixture")
            .bindings
            .insert("back".into(), "key:ctrl+c".into());
        state.config = config.clone();
        let mut replaced = target.clone();
        replaced.id = "replacement".into();
        state
            .config
            .devices
            .insert(replaced.id.clone(), config.devices["mouse"].clone());
        assert!(
            state.mapping_targets(vec![replaced]).is_empty(),
            "a different device cannot inherit a worker"
        );
        state.config = config.clone();
        target.slot = Some(2);
        assert!(state.mapping_targets(vec![target.clone()]).is_empty());
        target.slot = Some(1);
        let replacement = directory.path().join("replacement");
        fs::write(&replacement, "second")?;
        fs::rename(replacement, &path)?;
        assert!(!state.can_reuse_workers(&config));
        assert!(state.mapping_targets(vec![target.clone()]).is_empty());
        state.workers.get_mut("mouse").expect("worker").route = MappingRoute::from_device(&target);
        assert!(state.can_reuse_workers(&config));
        state.workers["mouse"].stop.send_replace(true);
        assert!(
            !state.can_reuse_workers(&config),
            "stopping workers are not healthy"
        );
        assert!(state.mapping_targets(vec![target.clone()]).is_empty());
        state.stop().await;
        assert!(
            !state.can_reuse_workers(&config),
            "absent workers must be discovered again"
        );
        assert!(state.mapping_targets(vec![target]).is_empty());
        Ok(())
    }

    #[test]
    fn failed_mapping_backs_off_but_changed_bindings_can_retry() {
        let bindings = BTreeMap::from([("back".into(), "key:ctrl+c".into())]);
        let route = MappingRoute::from_device(&device("mouse"));
        let now = Instant::now();
        let retry = MappingRetry {
            bindings: bindings.clone(),
            thumb_wheel_interval_ms: 0,
            route: route.clone(),
            after: now + Duration::from_secs(30),
        };
        assert!(retry.blocks(&bindings, 0, &route, now + Duration::from_secs(29)));
        assert!(!retry.blocks(&bindings, 0, &route, now + Duration::from_secs(30)));
        assert!(!retry.blocks(&bindings, 250, &route, now));
        assert!(!retry.blocks(
            &BTreeMap::from([("back".into(), "key:ctrl+v".into())]),
            0,
            &route,
            now
        ));
        let mut changed = route;
        changed.slot = Some(2);
        assert!(!retry.blocks(&bindings, 0, &changed, now));
    }

    #[tokio::test]
    async fn already_stopped_mapping_signals_before_polling_the_worker() -> Result<()> {
        for closed in [false, true] {
            let (shutdown, receiver) = watch::channel(false);
            if closed {
                drop(shutdown);
            } else {
                shutdown.send_replace(true);
            }
            let (stop, stopped) = watch::channel(false);
            drain_mapping(
                async move {
                    assert!(
                        *stopped.borrow(),
                        "initialization must see shutdown before accessing hardware"
                    );
                    Ok(())
                },
                stop,
                receiver,
            )
            .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn mapping_shutdown_signals_immediately_and_drains_cleanup() -> Result<()> {
        let (shutdown, receiver) = watch::channel(false);
        let (stop, mut stopped) = watch::channel(false);
        let (observed, observation) = tokio::sync::oneshot::channel();
        let (finish_cleanup, cleanup_finished) = tokio::sync::oneshot::channel();
        let worker = async move {
            wait_for_mapping_stop(&mut stopped).await;
            let _ = observed.send(());
            cleanup_finished.await?;
            Ok(())
        };
        let managed = tokio::spawn(drain_mapping(worker, stop, receiver));
        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), observation).await??;
        assert!(!managed.is_finished(), "cleanup must not be cancelled");
        finish_cleanup.send(()).expect("cleanup still waiting");
        managed.await??;
        Ok(())
    }

    #[tokio::test]
    async fn active_pairing_guard_is_retained_until_worker_cleanup_finishes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("remap-active.lock");
        let mut state = MappingState::new(config::Config::default(), None);
        state.active_guard = Some(lock_file(&path)?);
        let (stop, mut receiver) = watch::channel(false);
        let (observed, observation) = tokio::sync::oneshot::channel();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            wait_for_mapping_stop(&mut receiver).await;
            let _ = observed.send(());
            finished.await?;
            Ok(())
        });
        state.workers.insert(
            "mouse".into(),
            MappingWorker {
                bindings: BTreeMap::new(),
                thumb_wheel_interval_ms: Arc::new(AtomicU16::new(0)),
                route: MappingRoute::from_device(&device("mouse")),
                stop,
                handle: Some(handle),
            },
        );
        let cleanup = tokio::spawn(async move {
            state.stop().await;
        });
        tokio::time::timeout(Duration::from_secs(1), observation).await??;
        assert!(try_lock_file(&path)?.is_none());
        finish.send(()).expect("cleanup still waiting");
        cleanup.await?;
        let _available = lock_file(&path)?;
        Ok(())
    }
}
