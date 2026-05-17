use crate::{DeviceInfo, ListOptions, PlatformBackend};

pub struct WindowsBackend;

impl PlatformBackend for WindowsBackend {
    fn list_devices(&self, opts: &ListOptions) -> Result<Vec<DeviceInfo>, String> {
        #[cfg(target_os = "windows")]
        {
            list_devices_windows(opts)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = opts;
            Ok(Vec::new())
        }
    }

    fn write_image(&self, _image_path: &str, _device_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn get_boot_device(&self) -> Option<String> {
        #[cfg(target_os = "windows")]
        {
            get_boot_device_windows()
        }
        #[cfg(not(target_os = "windows"))]
        None
    }
}

// ── Windows implementation ────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn list_devices_windows(opts: &ListOptions) -> Result<Vec<DeviceInfo>, String> {
    use windows::core::GUID;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
        SetupDiGetDeviceRegistryPropertyW, DIGCF_PRESENT, SPDRP_FRIENDLYNAME, SPDRP_HARDWAREID,
        SP_DEVINFO_DATA,
    };
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;

    // GUID_DEVINTERFACE_DISK
    const DISK_GUID: GUID = GUID {
        data1: 0x53F56307,
        data2: 0xB6BF,
        data3: 0x11D0,
        data4: [0x94, 0xF2, 0x00, 0xA0, 0xC9, 0x1E, 0xFB, 0x8B],
    };

    let boot_dev = get_boot_device_windows();
    let mut out = Vec::new();

    // Use a simple approach: enumerate \\.\PhysicalDrive0 through 15
    for n in 0..16u32 {
        let path = format!(r"\\.\PhysicalDrive{}", n);
        if let Some(info) = probe_physical_drive(&path, n, &boot_dev, opts) {
            out.push(info);
        }
    }

    Ok(out)
}

#[cfg(target_os = "windows")]
fn probe_physical_drive(
    path: &str,
    n: u32,
    boot_dev: &Option<String>,
    opts: &ListOptions,
) -> Option<DeviceInfo> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    // IOCTL_STORAGE_QUERY_PROPERTY = 0x002D1400
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;

    // Try to open the device
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            HANDLE(0),
        )
        .ok()?
    };

    // Query storage device property to get bus type
    #[repr(C)]
    struct StoragePropertyQuery {
        property_id: u32,
        query_type: u32,
        additional: [u8; 1],
    }
    #[repr(C)]
    struct StorageDeviceDescriptor {
        version: u32,
        size: u32,
        device_type: u8,
        device_type_modifier: u8,
        removable_media: u8, // BOOLEAN
        command_queueing: u8,
        vendor_id_offset: u32,
        product_id_offset: u32,
        product_revision_offset: u32,
        serial_number_offset: u32,
        bus_type: u32,
        raw_properties_length: u32,
        raw_device_properties: [u8; 1],
    }

    let query = StoragePropertyQuery {
        property_id: 0,
        query_type: 0,
        additional: [0],
    };
    let mut desc = [0u8; 1024];
    let mut bytes_returned = 0u32;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const _),
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            Some(desc.as_mut_ptr() as *mut _),
            desc.len() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };

    unsafe { windows::Win32::Foundation::CloseHandle(handle).ok() };

    if ok.is_err() || bytes_returned < std::mem::size_of::<StorageDeviceDescriptor>() as u32 {
        return None;
    }

    let descriptor = unsafe { &*(desc.as_ptr() as *const StorageDeviceDescriptor) };

    // BusTypeUsb = 7
    let is_usb = descriptor.bus_type == 7;
    let removable = descriptor.removable_media != 0;

    if !opts.show_all && (!is_usb || !removable) {
        return None;
    }

    let is_boot = boot_dev.as_deref().map(|b| b == path).unwrap_or(false);
    let is_system = !removable || !is_usb || is_boot;

    if !opts.show_all && is_system {
        return None;
    }

    // Get disk size via IOCTL_DISK_GET_LENGTH_INFO = 0x0007405C
    let size_bytes = get_disk_size(path).unwrap_or(0);

    let name = extract_string(&desc, descriptor.product_id_offset as usize)
        .unwrap_or_else(|| format!("PhysicalDrive{}", n));

    Some(DeviceInfo {
        id: path.to_string(),
        name: name.trim().to_string(),
        size_bytes,
        removable,
        bus: if is_usb {
            "USB".to_string()
        } else {
            "Unknown".to_string()
        },
        raw_device: Some(path.to_string()),
        is_system_drive: is_system,
    })
}

#[cfg(target_os = "windows")]
fn get_disk_size(path: &str) -> Option<u64> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            HANDLE(0),
        )
        .ok()?
    };

    let mut length_info = [0u8; 8];
    let mut bytes_returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some(length_info.as_mut_ptr() as *mut _),
            8,
            Some(&mut bytes_returned),
            None,
        )
    };
    unsafe { windows::Win32::Foundation::CloseHandle(handle).ok() };

    if ok.is_err() {
        return None;
    }
    Some(u64::from_le_bytes(length_info))
}

#[cfg(target_os = "windows")]
fn get_boot_device_windows() -> Option<String> {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buf = [0u16; 260];
    let len = unsafe { GetSystemDirectoryW(Some(&mut buf)) };
    if len == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..len as usize]);
    // s = "C:\Windows\system32" → drive letter = "C"
    let drive = s.chars().next()?;
    // Map drive letter to physical drive number via a WMI-less approach:
    // just try \\.\C: volume → disk extent
    let volume_path = format!(r"\\.\{}:", drive);
    get_physical_drive_for_volume(&volume_path)
}

#[cfg(target_os = "windows")]
fn get_physical_drive_for_volume(volume_path: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0005_60000;

    let wide: Vec<u16> = volume_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            HANDLE(0),
        )
        .ok()?
    };

    let mut buf = [0u8; 64];
    let mut bytes = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut bytes),
            None,
        )
    };
    unsafe { windows::Win32::Foundation::CloseHandle(handle).ok() };

    if ok.is_err() || bytes < 16 {
        return None;
    }

    // DISK_EXTENT[0].DiskNumber is at offset 8 (after NumberOfDiskExtents u32 + padding u32)
    let disk_number = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    Some(format!(r"\\.\PhysicalDrive{}", disk_number))
}

#[cfg(target_os = "windows")]
fn extract_string(buf: &[u8], offset: usize) -> Option<String> {
    if offset == 0 || offset >= buf.len() {
        return None;
    }
    let end = buf[offset..]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(buf.len() - offset);
    std::str::from_utf8(&buf[offset..offset + end])
        .ok()
        .map(|s| s.to_string())
}
