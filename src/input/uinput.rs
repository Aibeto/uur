use anyhow::{Context, Result};
use input_linux_sys as linux;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;

use super::InputBackend;

// /dev/uinput interface constants (linux/uinput.h, linux/input-event-codes.h)
const UI_SET_EVBIT: libc::Ioctl = 0x4004_5564_u64 as libc::Ioctl;
const UI_SET_KEYBIT: libc::Ioctl = 0x4004_5565_u64 as libc::Ioctl;
const UI_SET_RELBIT: libc::Ioctl = 0x4004_5566_u64 as libc::Ioctl;
const UI_SET_ABSBIT: libc::Ioctl = 0x4004_5567_u64 as libc::Ioctl;
// struct uinput_setup = input_id(8) + name[80] + ff_effects_max(4) = 92 bytes
const UI_DEV_SETUP: libc::Ioctl = 0x405C_5503_u64 as libc::Ioctl;
// struct uinput_abs_setup = u16 code + input_absinfo(24) = 28 bytes
const UI_ABS_SETUP: libc::Ioctl = 0x401C_5504_u64 as libc::Ioctl;
const UI_DEV_CREATE: libc::Ioctl = 0x5501_u64 as libc::Ioctl;
const UI_DEV_DESTROY: libc::Ioctl = 0x5502_u64 as libc::Ioctl;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const REL_HWHEEL: u16 = 0x09;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x116;
const BTN_EXTRA: u16 = 0x117;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOUCH: u16 = 0x14a;
const BTN_STYLUS: u16 = 0x14b;

const BUS_VIRTUAL: u16 = 0x06;

/// struct input_event, 64-bit time_t layout.
#[repr(C)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

/// Linux ABI: input_id(4×u16), then name[80], then ff_effects_max(u32).
#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

/// struct uinput_abs_setup: u16 code (+padding) + input_absinfo (6×i32).
#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    pad: u16,
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

fn open_device() -> Result<File> {
    OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .context("opening /dev/uinput")
}

fn ioctl_u32(file: &File, request: libc::Ioctl, value: u32) -> Result<()> {
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request, value) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("uinput ioctl");
    }
    Ok(())
}

/// One kernel virtual input device.
struct UinputDevice {
    file: File,
}

impl UinputDevice {
    fn create(name: &'static str, product: u16) -> Result<Self> {
        let file = open_device()?;
        unsafe {
            let mut setup_name = [0u8; 80];
            let label = name.as_bytes();
            let len = label.len().min(79);
            setup_name[..len].copy_from_slice(&label[..len]);
            let rc = libc::ioctl(
                file.as_raw_fd(),
                UI_DEV_SETUP,
                &UinputSetup {
                    name: setup_name,
                    bustype: BUS_VIRTUAL,
                    vendor: 0x1,
                    product,
                    version: 1,
                    ff_effects_max: 0,
                },
            );
            if rc < 0 {
                return Err(std::io::Error::last_os_error()).context("uinput UI_DEV_SETUP");
            }
        }
        Ok(Self { file })
    }

    fn keybit(&self, code: u16) -> Result<()> {
        ioctl_u32(&self.file, UI_SET_KEYBIT, code as u32)
    }

    fn relbit(&self, code: u16) -> Result<()> {
        ioctl_u32(&self.file, UI_SET_RELBIT, code as u32)
    }

    fn absbit(&self, code: u16, maximum: i32) -> Result<()> {
        ioctl_u32(&self.file, UI_SET_ABSBIT, code as u32)?;
        let setup = UinputAbsSetup {
            code,
            pad: 0,
            value: 0,
            minimum: 0,
            maximum,
            fuzz: 0,
            flat: 0,
            // libinput requires physical axis resolution for tablets. A
            // 65.535 mm virtual surface maps the full normalized range.
            resolution: 1000,
        };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), UI_ABS_SETUP, &setup) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("uinput UI_ABS_SETUP");
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_CREATE) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("uinput UI_DEV_CREATE");
        }
        Ok(())
    }

    fn emit(&self, type_: u16, code: u16, value: i32) -> Result<()> {
        let event = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &event as *const InputEvent as *const u8,
                std::mem::size_of::<InputEvent>(),
            )
        };
        // File implements Write through a shared reference.
        let mut file = &self.file;
        file.write_all(bytes)?;
        Ok(())
    }

    fn syn(&self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

/// Kernel-level input injection through /dev/uinput.  Works identically on
/// every Wayland compositor and every X11 session, because the events enter
/// the input stack like real hardware.  Requires write access to
/// /dev/uinput (udev `uaccess` tag or the `input` group).
///
/// Devices are deliberately split so libinput classifies each one cleanly:
/// mixing REL and ABS axes on one device makes compositors treat it as a
/// mouse and silently drop its absolute events (verified on KWin).
struct Devices {
    keyboard: UinputDevice,
    pointer: UinputDevice,
    tablet: UinputDevice,
}

impl Devices {
    fn create() -> Result<Self> {
        let keyboard = UinputDevice::create("uur virtual keyboard", 1)?;
        ioctl_u32(&keyboard.file, UI_SET_EVBIT, EV_KEY as u32)?;
        ioctl_u32(&keyboard.file, UI_SET_EVBIT, EV_SYN as u32)?;
        for code in 1..=0xffu16 {
            keyboard.keybit(code)?;
        }
        keyboard.finish()?;

        let pointer = UinputDevice::create("uur virtual pointer", 2)?;
        ioctl_u32(&pointer.file, UI_SET_EVBIT, EV_KEY as u32)?;
        ioctl_u32(&pointer.file, UI_SET_EVBIT, EV_REL as u32)?;
        ioctl_u32(&pointer.file, UI_SET_EVBIT, EV_SYN as u32)?;
        for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
            pointer.keybit(button)?;
        }
        for rel in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
            pointer.relbit(rel)?;
        }
        pointer.finish()?;

        // An absolute pointer must look like a pen tablet, otherwise
        // libinput treats it as a mouse and ignores absolute motion.
        let tablet = UinputDevice::create("uur virtual tablet", 3)?;
        ioctl_u32(&tablet.file, UI_SET_EVBIT, EV_KEY as u32)?;
        ioctl_u32(&tablet.file, UI_SET_EVBIT, EV_ABS as u32)?;
        ioctl_u32(&tablet.file, UI_SET_EVBIT, EV_SYN as u32)?;
        tablet.keybit(BTN_TOOL_PEN)?;
        tablet.keybit(BTN_TOUCH)?;
        tablet.keybit(BTN_STYLUS)?;
        tablet.absbit(ABS_X, 65535)?;
        tablet.absbit(ABS_Y, 65535)?;
        tablet.finish()?;
        // Stay in proximity so hover motion keeps updating the cursor.
        tablet.emit(EV_KEY, BTN_TOOL_PEN, 1)?;
        tablet.syn()?;

        Ok(Self {
            keyboard,
            pointer,
            tablet,
        })
    }
}

pub struct UinputBackend {
    devices: Devices,
    held_keys: HashSet<u16>,
    held_buttons: HashSet<u16>,
}

impl UinputBackend {
    pub fn connect() -> Result<Self> {
        let devices = Devices::create()?;
        Ok(Self {
            devices,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
        })
    }
}

impl InputBackend for UinputBackend {
    fn name(&self) -> &'static str {
        "uinput"
    }

    fn key(&mut self, vkey: u16, down: bool) -> Result<()> {
        let code = vk_to_linux_key(vkey);
        if code == 0 {
            return Ok(()); // not representable on this layout; drop quietly
        }
        let value = if down { 1 } else { 0 };
        self.devices.keyboard.emit(EV_KEY, code, value)?;
        self.devices.keyboard.syn()?;
        if down {
            self.held_keys.insert(code);
        } else {
            self.held_keys.remove(&code);
        }
        Ok(())
    }

    fn button(&mut self, button: u16, down: bool) -> Result<()> {
        let code = match button {
            1 => BTN_LEFT,
            2 => BTN_RIGHT,
            3 => BTN_MIDDLE,
            8 => BTN_SIDE,
            9 => BTN_EXTRA,
            _ => return Ok(()), // unknown buttons are dropped, not fatal
        };
        let value = if down { 1 } else { 0 };
        self.devices.pointer.emit(EV_KEY, code, value)?;
        self.devices.pointer.syn()?;
        if down {
            self.held_buttons.insert(code);
        } else {
            self.held_buttons.remove(&code);
        }
        Ok(())
    }

    fn motion(&mut self, absolute: bool, x: i32, y: i32) -> Result<()> {
        if absolute {
            self.devices.tablet.emit(EV_ABS, ABS_X, x.clamp(0, 65535))?;
            self.devices.tablet.emit(EV_ABS, ABS_Y, y.clamp(0, 65535))?;
            self.devices.tablet.syn()?;
        } else {
            self.devices.pointer.emit(EV_REL, REL_X, x)?;
            self.devices.pointer.emit(EV_REL, REL_Y, y)?;
            self.devices.pointer.syn()?;
        }
        Ok(())
    }

    fn wheel(&mut self, horizontal: bool, delta: i32) -> Result<()> {
        self.devices.pointer.emit(
            EV_REL,
            if horizontal { REL_HWHEEL } else { REL_WHEEL },
            super::wheel_steps(delta),
        )?;
        self.devices.pointer.syn()
    }

    fn release_all(&mut self) -> Result<()> {
        for code in self.held_keys.clone() {
            self.devices.keyboard.emit(EV_KEY, code, 0)?;
        }
        self.held_keys.clear();
        for code in self.held_buttons.clone() {
            self.devices.pointer.emit(EV_KEY, code, 0)?;
        }
        self.held_buttons.clear();
        self.devices.keyboard.syn()?;
        self.devices.pointer.syn()
    }
}

/// Windows virtual-key code -> Linux input keycode, invariant subset.
/// The layout-aware remainder arrives with the Phase 2 semantic text path.
pub(crate) fn vk_to_linux_key(vkey: u16) -> u16 {
    let base = vkey & 0xff;
    let extended = vkey & 0x100 != 0;
    let code = match base {
        0x08 => linux::KEY_BACKSPACE,
        0x09 => linux::KEY_TAB,
        0x0d if extended => linux::KEY_KPENTER,
        0x0d => linux::KEY_ENTER,
        0x10 | 0xa0 => linux::KEY_LEFTSHIFT,
        0xa1 => linux::KEY_RIGHTSHIFT,
        0x11 | 0xa2 if extended => linux::KEY_RIGHTCTRL,
        0x11 | 0xa2 => linux::KEY_LEFTCTRL,
        0xa3 => linux::KEY_RIGHTCTRL,
        0x12 | 0xa4 if extended => linux::KEY_RIGHTALT,
        0x12 | 0xa4 => linux::KEY_LEFTALT,
        0xa5 => linux::KEY_RIGHTALT,
        0x13 => linux::KEY_PAUSE,
        0x14 => linux::KEY_CAPSLOCK,
        0x1b => linux::KEY_ESC,
        0x20 => linux::KEY_SPACE,
        0x21 => linux::KEY_PAGEUP,
        0x22 => linux::KEY_PAGEDOWN,
        0x23 => linux::KEY_END,
        0x24 => linux::KEY_HOME,
        0x25 => linux::KEY_LEFT,
        0x26 => linux::KEY_UP,
        0x27 => linux::KEY_RIGHT,
        0x28 => linux::KEY_DOWN,
        0x2d => linux::KEY_INSERT,
        0x2e => linux::KEY_DELETE,
        0x30 => linux::KEY_0,
        0x31 => linux::KEY_1,
        0x32 => linux::KEY_2,
        0x33 => linux::KEY_3,
        0x34 => linux::KEY_4,
        0x35 => linux::KEY_5,
        0x36 => linux::KEY_6,
        0x37 => linux::KEY_7,
        0x38 => linux::KEY_8,
        0x39 => linux::KEY_9,
        0x41 => linux::KEY_A,
        0x42 => linux::KEY_B,
        0x43 => linux::KEY_C,
        0x44 => linux::KEY_D,
        0x45 => linux::KEY_E,
        0x46 => linux::KEY_F,
        0x47 => linux::KEY_G,
        0x48 => linux::KEY_H,
        0x49 => linux::KEY_I,
        0x4a => linux::KEY_J,
        0x4b => linux::KEY_K,
        0x4c => linux::KEY_L,
        0x4d => linux::KEY_M,
        0x4e => linux::KEY_N,
        0x4f => linux::KEY_O,
        0x50 => linux::KEY_P,
        0x51 => linux::KEY_Q,
        0x52 => linux::KEY_R,
        0x53 => linux::KEY_S,
        0x54 => linux::KEY_T,
        0x55 => linux::KEY_U,
        0x56 => linux::KEY_V,
        0x57 => linux::KEY_W,
        0x58 => linux::KEY_X,
        0x59 => linux::KEY_Y,
        0x5a => linux::KEY_Z,
        0x5b => linux::KEY_LEFTMETA,
        0x5c => linux::KEY_RIGHTMETA,
        0x60 => linux::KEY_KP0,
        0x61 => linux::KEY_KP1,
        0x62 => linux::KEY_KP2,
        0x63 => linux::KEY_KP3,
        0x64 => linux::KEY_KP4,
        0x65 => linux::KEY_KP5,
        0x66 => linux::KEY_KP6,
        0x67 => linux::KEY_KP7,
        0x68 => linux::KEY_KP8,
        0x69 => linux::KEY_KP9,
        0x6a => linux::KEY_KPASTERISK,
        0x6b => linux::KEY_KPPLUS,
        0x6d => linux::KEY_KPMINUS,
        0x6e => linux::KEY_KPDOT,
        0x6f => linux::KEY_KPSLASH,
        0x70 => linux::KEY_F1,
        0x71 => linux::KEY_F2,
        0x72 => linux::KEY_F3,
        0x73 => linux::KEY_F4,
        0x74 => linux::KEY_F5,
        0x75 => linux::KEY_F6,
        0x76 => linux::KEY_F7,
        0x77 => linux::KEY_F8,
        0x78 => linux::KEY_F9,
        0x79 => linux::KEY_F10,
        0x7a => linux::KEY_F11,
        0x7b => linux::KEY_F12,
        0xba => linux::KEY_SEMICOLON,
        0xbb => linux::KEY_EQUAL,
        0xbc => linux::KEY_COMMA,
        0xbd => linux::KEY_MINUS,
        0xbe => linux::KEY_DOT,
        0xbf => linux::KEY_SLASH,
        0xc0 => linux::KEY_GRAVE,
        0xdb => linux::KEY_LEFTBRACE,
        0xdc => linux::KEY_BACKSLASH,
        0xdd => linux::KEY_RIGHTBRACE,
        0xde => linux::KEY_APOSTROPHE,
        _ => return 0,
    };
    code as u16
}

#[cfg(test)]
mod tests {
    use super::vk_to_linux_key;
    use input_linux_sys as linux;

    #[test]
    fn qwerty_row_uses_evdev_physical_positions() {
        let mapped: Vec<u16> = "QWERTYUIOP"
            .bytes()
            .map(|key| vk_to_linux_key(key.into()))
            .collect();
        assert_eq!(
            mapped,
            [
                linux::KEY_Q,
                linux::KEY_W,
                linux::KEY_E,
                linux::KEY_R,
                linux::KEY_T,
                linux::KEY_Y,
                linux::KEY_U,
                linux::KEY_I,
                linux::KEY_O,
                linux::KEY_P,
            ]
            .map(|code| code as u16)
        );
    }

    #[test]
    fn alphabet_is_not_assumed_contiguous() {
        let mapped: Vec<u16> = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
            .bytes()
            .map(|key| vk_to_linux_key(key.into()))
            .collect();
        assert_eq!(
            mapped,
            [
                linux::KEY_A,
                linux::KEY_B,
                linux::KEY_C,
                linux::KEY_D,
                linux::KEY_E,
                linux::KEY_F,
                linux::KEY_G,
                linux::KEY_H,
                linux::KEY_I,
                linux::KEY_J,
                linux::KEY_K,
                linux::KEY_L,
                linux::KEY_M,
                linux::KEY_N,
                linux::KEY_O,
                linux::KEY_P,
                linux::KEY_Q,
                linux::KEY_R,
                linux::KEY_S,
                linux::KEY_T,
                linux::KEY_U,
                linux::KEY_V,
                linux::KEY_W,
                linux::KEY_X,
                linux::KEY_Y,
                linux::KEY_Z,
            ]
            .map(|code| code as u16)
        );
    }

    #[test]
    fn non_contiguous_function_keys_and_extended_modifiers_are_preserved() {
        assert_eq!(vk_to_linux_key(0x7a), linux::KEY_F11 as u16);
        assert_eq!(vk_to_linux_key(0x7b), linux::KEY_F12 as u16);
        assert_eq!(vk_to_linux_key(0x111), linux::KEY_RIGHTCTRL as u16);
        assert_eq!(vk_to_linux_key(0x112), linux::KEY_RIGHTALT as u16);
        assert_eq!(vk_to_linux_key(0x10d), linux::KEY_KPENTER as u16);
    }
}
