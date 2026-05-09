pub mod linux;
pub mod macos;
pub mod windows;

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub size_bytes: u64,
    pub removable: bool,
    pub bus: String,
    pub raw_device: Option<String>,
    /// True if this drive is the system/boot drive or an internal storage device.
    pub is_system_drive: bool,
}

/// Options for listing devices.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListOptions {
    /// If false (default), hide system/internal drives.
    pub show_all: bool,
}

pub trait PlatformBackend {
    fn list_devices(&self, opts: &ListOptions) -> Result<Vec<DeviceInfo>, String>;
    fn write_image(&self, image_path: &str, device_id: &str) -> Result<(), String>;
    /// Returns the block device path of the current boot drive, if detectable.
    fn get_boot_device(&self) -> Option<String>;
}
