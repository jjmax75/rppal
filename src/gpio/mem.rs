// Copyright (c) 2017-2018 Rene van der Meer
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use libc;

use crate::gpio::{Error, Level, Mode, PullUpDown, Result};
use crate::system::DeviceInfo;

// The BCM2835 has 41 32-bit registers related to the GPIO (datasheet @ 6.1).
const GPIO_MEM_REGISTERS: usize = 41;
const GPIO_MEM_SIZE: usize = GPIO_MEM_REGISTERS * std::mem::size_of::<u32>();

const GPFSEL0: usize = 0x00;
const GPSET0: usize = 0x1c / std::mem::size_of::<u32>();
const GPCLR0: usize = 0x28 / std::mem::size_of::<u32>();
const GPLEV0: usize = 0x34 / std::mem::size_of::<u32>();
const GPPUD: usize = 0x94 / std::mem::size_of::<u32>();
const GPPUDCLK0: usize = 0x98 / std::mem::size_of::<u32>();

pub struct GpioMem {
    mem_ptr: *mut u32,
    locks: [AtomicBool; GPIO_MEM_REGISTERS],
}

impl fmt::Debug for GpioMem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpioMem")
            .field("mem_ptr", &self.mem_ptr)
            .field("locks", &format_args!("{{ .. }}"))
            .finish()
    }
}

impl GpioMem {
    pub fn open() -> Result<GpioMem> {
        // Try /dev/gpiomem first. If that fails, try /dev/mem instead. If neither works,
        // report back the error that's the most relevant.
        let mem_ptr = match Self::map_devgpiomem() {
            Ok(ptr) => ptr,
            Err(gpiomem_err) => match Self::map_devmem() {
                Ok(ptr) => ptr,
                Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(Error::PermissionDenied)
                }
                Err(Error::UnknownModel) => return Err(Error::UnknownModel),
                _ => return Err(gpiomem_err),
            },
        };

        let locks = init_array!(AtomicBool::new(false), GPIO_MEM_REGISTERS);

        Ok(GpioMem { mem_ptr, locks })
    }

    fn map_devgpiomem() -> Result<*mut u32> {
        // Open /dev/gpiomem with read/write/sync flags. This might fail if
        // /dev/gpiomem doesn't exist (< Raspbian Jessie), or /dev/gpiomem
        // doesn't have the appropriate permissions, or the current user is
        // not a member of the gpio group.
        let gpiomem_file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open("/dev/gpiomem")?;

        // Memory-map /dev/gpiomem at offset 0
        let gpiomem_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                GPIO_MEM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                gpiomem_file.as_raw_fd(),
                0,
            )
        };

        if gpiomem_ptr == libc::MAP_FAILED {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        Ok(gpiomem_ptr as *mut u32)
    }

    fn map_devmem() -> Result<*mut u32> {
        // Identify which SoC we're using, so we know what offset to start at
        let device_info = DeviceInfo::new().map_err(|_| Error::UnknownModel)?;

        let mem_file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open("/dev/mem")?;

        // Memory-map /dev/mem at the appropriate offset for our SoC
        let mem_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                GPIO_MEM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                mem_file.as_raw_fd(),
                (device_info.peripheral_base() + device_info.gpio_offset()) as libc::off_t,
            )
        };

        if mem_ptr == libc::MAP_FAILED {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        Ok(mem_ptr as *mut u32)
    }

    #[inline]
    fn read(&self, offset: usize) -> u32 {
        unsafe { ptr::read_volatile(self.mem_ptr.add(offset)) }
    }

    #[inline]
    fn write(&self, offset: usize, value: u32) {
        unsafe {
            ptr::write_volatile(self.mem_ptr.add(offset), value);
        }
    }

    pub(crate) fn set_high(&self, pin: u8) {
        let offset = GPSET0 + pin as usize / 32;
        let shift = pin % 32;
        self.write(offset, 1 << shift);
    }

    pub(crate) fn set_low(&self, pin: u8) {
        let offset = GPCLR0 + pin as usize / 32;
        let shift = pin % 32;
        self.write(offset, 1 << shift);
    }

    pub(crate) fn level(&self, pin: u8) -> Level {
        let offset = GPLEV0 + pin as usize / 32;
        let shift = pin % 32;

        let reg_value = self.read(offset);

        unsafe { std::mem::transmute((reg_value >> shift) as u8 & 0b1) }
    }

    pub(crate) fn mode(&self, pin: u8) -> Mode {
        let offset = GPFSEL0 + pin as usize / 10;
        let shift = (pin % 10) * 3;

        let reg_value = self.read(offset);

        unsafe { std::mem::transmute((reg_value >> shift) as u8 & 0b111) }
    }

    pub(crate) fn set_mode(&self, pin: u8, mode: Mode) {
        let offset = GPFSEL0 + pin as usize / 10;
        let shift = (pin % 10) * 3;

        loop {
            if !self.locks[offset].compare_and_swap(false, true, Ordering::SeqCst) {
                break;
            }
        }

        let reg_value = self.read(offset);
        self.write(
            offset,
            (reg_value & !(0b111 << shift)) | ((mode as u32) << shift),
        );

        self.locks[offset].store(false, Ordering::SeqCst);
    }

    /// Configures the built-in GPIO pull-up/pull-down resistors.
    pub(crate) fn set_pullupdown(&self, pin: u8, pud: PullUpDown) {
        let offset = GPPUDCLK0 + pin as usize / 32;
        let shift = pin % 32;

        loop {
            if !self.locks[GPPUD].compare_and_swap(false, true, Ordering::SeqCst) {
                if !self.locks[offset].compare_and_swap(false, true, Ordering::SeqCst) {
                    break;
                } else {
                    self.locks[GPPUD].store(false, Ordering::SeqCst);
                }
            }
        }

        // Set the control signal in GPPUD.
        let reg_value = self.read(GPPUD);
        self.write(GPPUD, (reg_value & !0b11) | ((pud as u32) & 0b11));

        // The datasheet mentions waiting at least 150 cycles for set-up and hold, but
        // doesn't state which clock is used. This is likely the VPU clock (see
        // https://www.raspberrypi.org/forums/viewtopic.php?f=72&t=163352). At either
        // 250MHz or 400MHz, a 5µs delay + overhead is more than adequate.

        // Set-up time for the control signal.
        sleep(Duration::new(0, 5000)); // >= 5µs

        // Clock the control signal into the selected pin.
        self.write(offset, 1 << shift);

        // Hold time for the control signal.
        sleep(Duration::new(0, 5000)); // >= 5µs

        // Remove the control signal and clock.
        self.write(GPPUD, reg_value & !0b11);
        self.write(offset, 0);

        self.locks[offset].store(false, Ordering::SeqCst);
        self.locks[GPPUD].store(false, Ordering::SeqCst);
    }
}

impl Drop for GpioMem {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(
                self.mem_ptr as *mut libc::c_void,
                GPIO_MEM_SIZE as libc::size_t,
            );
        }
    }
}

// Required because of the raw pointer to our memory-mapped file
unsafe impl Send for GpioMem {}
unsafe impl Sync for GpioMem {}
