use jfn_platform_abi::geometry::{Bounds, ClampError, MpvScreen, ScreenName, TargetScreen};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::core::BOOL;

pub(crate) struct Monitor {
    info: MONITORINFOEXW,
}

impl Monitor {
    pub(crate) fn of(handle: HMONITOR) -> windows::core::Result<Monitor> {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        unsafe { GetMonitorInfoW(handle, std::ptr::from_mut(&mut info).cast::<MONITORINFO>()) }
            .ok()?;
        Ok(Monitor { info })
    }

    pub(crate) fn name(&self) -> Option<ScreenName> {
        let device = &self.info.szDevice;
        let end = device.iter().position(|&c| c == 0).unwrap_or(device.len());
        ScreenName::new(String::from_utf16_lossy(&device[..end]))
    }

    pub(crate) fn work_area(&self) -> RECT {
        self.info.monitorInfo.rcWork
    }

    fn work_bounds(&self) -> Bounds {
        let work = self.work_area();
        Bounds {
            w: work.right - work.left,
            h: work.bottom - work.top,
        }
    }

    pub(crate) fn find(name: &ScreenName) -> Result<Option<TargetScreen>, ClampError> {
        for (index, handle) in enumerate()?.into_iter().enumerate() {
            let monitor = Monitor::of(handle).map_err(|_| ClampError::MonitorInfo)?;
            if monitor.name().as_ref() == Some(name) {
                return Ok(Some(TargetScreen {
                    mpv: MpvScreen::Index(index),
                    bounds: monitor.work_bounds(),
                }));
            }
        }
        Ok(None)
    }
}

unsafe extern "system" fn collect(
    monitor: HMONITOR,
    _dc: HDC,
    _clip: *mut RECT,
    found: LPARAM,
) -> BOOL {
    let found = unsafe { &mut *(found.0 as *mut Vec<HMONITOR>) };
    found.push(monitor);
    BOOL::from(true)
}

fn enumerate() -> Result<Vec<HMONITOR>, ClampError> {
    let mut found: Vec<HMONITOR> = Vec::new();
    let data = LPARAM(std::ptr::from_mut(&mut found) as isize);
    unsafe { EnumDisplayMonitors(None, None, Some(collect), data) }
        .ok()
        .map_err(|_| ClampError::Monitors)?;
    Ok(found)
}
