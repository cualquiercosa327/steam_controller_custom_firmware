#![no_std]
#![no_main]
#![feature(asm, const_loop, const_if_match, const_panic, const_fn)]
#![feature(panic_info_message)]

mod usb_descriptors;
mod system;
mod lpc11uxx_misc;
mod rt;
mod led;
mod programming_mode;
mod usb_debug_uart;
mod nrf_comms;

use core::slice;
use core::mem::size_of;

use cortex_m_rt::{entry, exception};
use lpc11uxx_rom::iap;
use lpc11uxx::*;

// TODO: Once_cell for the cortex-m.
// BODY: Conquer-cell maybe? But that appears to spinlock... I need to check but
// BODY: I'm *fairly* sure that ARM guarantees interrupts only happen on insn
// BODY: boundary. This means that in theory, I should be able to use volatile
// BODY: reads to this variable to maintain the necessary invariants.
static mut MAIN_CLOCK_FREQ: u32 = 0;

fn initialize_main_clock_freq(syscon: &SYSCON) {
    unsafe { MAIN_CLOCK_FREQ = lpc11uxx_misc::get_main_clock_rate(syscon); }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct EepromData {
    magic: u16,
    unknown: u16,
    version: u32
}

static mut EEPROM_CACHE: EepromData = EepromData {
    magic: 0,
    unknown: 0,
    version: 0,
};

fn check_eeprom_magic() {
    unsafe {
        let eeprom_magic_ptr = slice::from_raw_parts_mut(&mut EEPROM_CACHE as *mut _ as *mut u8, size_of::<EepromData>());
        iap::eeprom_read(0, eeprom_magic_ptr, MAIN_CLOCK_FREQ / 1024);
        if EEPROM_CACHE.magic != 0xa55a {
            EEPROM_CACHE.magic = 0xa55a;
            EEPROM_CACHE.unknown = 0;
            // Steam controller writes 0, but all the steam controllers out there
            // have 10 here it seems. And since this eventually affects pinmux,
            // we really want to have the right value here.
            EEPROM_CACHE.version = 10;
            write_eeprom_cache();
        }
    }
}

fn write_eeprom_cache() {
    let eeprom_magic_ptr = unsafe { slice::from_raw_parts(&EEPROM_CACHE as *const _ as *const u8, size_of::<EepromData>()) };
    iap::eeprom_write(0, eeprom_magic_ptr, unsafe { MAIN_CLOCK_FREQ } / 1024);
}

fn is_usb_disconnected(gpio_port: &mut GPIO_PORT) -> bool {
    gpio_port.b0[3].read().pbyte().bit()
}

fn set_battery_power(gpio_port: &mut GPIO_PORT, state: bool) {
    // The original has an absolutely hilarious bug: they first write to the
    // GPIO port, and then set the direction bit. Over-eager optimizations?
    //
    // Seems to work anyways - I suppose those pins default to being output
    // pins. But let's do it in the correct order here.

    unsafe {
        if EEPROM_CACHE.version < 5 {
            gpio_port.dir[1].modify(|_, w| w.dirp8().set_bit());
            gpio_port.b140.write(|v| v.pbyte().bit(state));
        } else if EEPROM_CACHE.version < 8 {
            gpio_port.dir[1].modify(|_, w| w.dirp0().set_bit());
            gpio_port.b132.write(|v| v.pbyte().bit(state));
        } else {
            gpio_port.dir[1].modify(|_, w| w.dirp10().set_bit());
            gpio_port.b142.write(|v| v.pbyte().bit(!state));
        };
    }
}

/// Sets a special register to re-enter programming mode when when the device
/// resets.
///
/// This register persists through wakes and resets.
fn enter_programming_mode_on_reboot(pmu: &mut PMU, enable: bool) {
    pmu.gpreg[1].write(|v| unsafe { v.gpdata().bits(!enable as u32) });
}

/// Someone is going to need to explain this function to me. I can't figure
/// out if it actually does anything useful...
// Inline never because of the huge stack usage of this function.
#[inline(never)]
fn weird_flash_function(mut ram_page: u32) {
    let mut page_content = [0; 0x1000];

    let mut start_sector_number = 2;
    let mut u_var = 1;

    // Protect the first two pages
    if ram_page <= 2  {
        return;
    }

    while ram_page < 0x1c {
        if u_var < start_sector_number {
            iap::prepare_sector_for_write(u_var + 1, ram_page - 1);
            iap::erase_sectors(u_var + 1, ram_page - 1, unsafe { MAIN_CLOCK_FREQ } / 1024);
            u_var = ram_page - 1;
        }
        page_content.copy_from_slice(unsafe { slice::from_raw_parts((ram_page << 12) as *const u8, 0x1000) });
        iap::prepare_sector_for_write(start_sector_number, start_sector_number);
        iap::copy_ram_to_flash(start_sector_number << 12, &page_content as *const _ as usize, 0x1000, unsafe { MAIN_CLOCK_FREQ } / 1024);
        ram_page += 1;
        start_sector_number += 1;
    }
}

fn setup_pinmux(iocon: &mut IOCON) {
    iocon.pio0_3.write(|v| v.func().pio0_3().mode().pull_down());
    iocon.pio0_6.write(|v| v.func().usb_connect().mode().floating());
    iocon.pio1_17.write(|v| v.func().rxd().mode().floating());
    iocon.pio1_18.write(|v| v.func().txd().mode().floating());
}

fn watchdog_init(syscon: &lpc11uxx::SYSCON, watchdog: &lpc11uxx::WWDT) {
    // Enable watchdog clock.
    syscon.sysahbclkctrl.modify(|_, writer| writer.wwdt().enabled());

    // Initialize watchdog with default values
    watchdog.mod_.reset();
    watchdog.tc.reset();
    watchdog.warnint.write(|v| unsafe {
        // Normally, we're only supposed to write to bits 0:9, and the TRM tells
        // us that the rest of the bits should not contain ones. But then, you
        // look at the lpc_chip_11uxx_lib and lo and behold, they write 16 bits
        // of ones to WARNINT!
        //
        // For now, we'll follow in their footsteps. We might want to switch to
        // only setting the 9 defined bits to ones down the road. Or maybe just
        // keep the reset value in it?
        v.bits(0xffff)
    });
    watchdog.window.reset();
}

fn watchdog_feed(watchdog: &lpc11uxx::WWDT) {
    watchdog.feed.write(|v| unsafe { v.feed().bits(0xaa) });
    watchdog.feed.write(|v| unsafe { v.feed().bits(0x55) });
}

fn setup_watchdog(syscon: &SYSCON, watchdog: &WWDT, timeout: u32) {
    // Re-initialize the watchdog to the default values.
    watchdog_init(syscon, watchdog);

    // Set the timeout
    watchdog.tc.write(|v| unsafe { v.count().bits(timeout) });

    // Enable watchdog and make it reset on timeout
    watchdog.mod_.modify(|_, v| v.wden().running().wdreset().reset());

    // Do the first feed to start the watchdog.
    watchdog_feed(watchdog);
}

fn start_program2() -> ! {
    // ASM is slightly different for efficiency's sake.
    unsafe {
        asm!("
            // Get program2 vector table
            ldr r0, =0x2000

            // Load MSP with program2 master stack pointer
            ldr r1, [r0]
            msr msp, r1

            // Jump to program2 reset vector
            ldr r1, [r0, 4]
            bx r1

            test:
            wfi
            b test
        ");
    }
    // We shouldn't end up here
    loop {
        cortex_m::asm::wfi();
    }
}

#[entry]
fn main() -> ! {
    let mut peripherals = Peripherals::take().unwrap();
    let core_peripherals = CorePeripherals::take().unwrap();

    // Clock setup, and flashctrl init.
    system::initialize(&mut peripherals.SYSCON, &mut peripherals.FLASHCTRL);

    // Initialize the MAIN_CLOCK_FREQ
    initialize_main_clock_freq(&peripherals.SYSCON);

    // Check that the EEPROM Magic is correct, set it to the right value otherwise.
    check_eeprom_magic();

    // Enable GPIO clock
    peripherals
        .SYSCON
        .sysahbclkctrl
        .modify(|_, writer| writer.gpio().enabled());

    let usb_disconnected = is_usb_disconnected(&mut peripherals.GPIO_PORT);

    // If a brown-out is detected, we should kill the battery and die.
    if !usb_disconnected && peripherals.SYSCON.sysrststat.read().bod().bit_is_set() {
        peripherals.SYSCON.sysrststat.write_with_zero(|f| f.bod().reset_clear());
        set_battery_power(&mut peripherals.GPIO_PORT, false);
        loop {
            cortex_m::asm::wfi();
        }
    }

    set_battery_power(&mut peripherals.GPIO_PORT, true);
    enter_programming_mode_on_reboot(&mut peripherals.PMU, true);

    // The real firmware uses a table like the following and calls
    // Chip_IOCON_PinMuxSet in a loop to setup the pinmuxing. Unfortunately, the
    // functions to setup pinmux in lpc11uxx aren't flexible enough to allow
    // this in a convenient way, so we'll just have a single function setting up
    // the pinmux.
    // static PINMUX_INFO: [PinMuxInfo; 4] = [
    //     PinmuxInfo { port: 0, pin:  3, mode: PIO0_3 | PULL_DOWN },
    //     PinmuxInfo { port: 0, pin:  6, mode: USB_CONNECT | INACTIVE },
    //     PinmuxInfo { port: 1, pin: 17, mode: RXD | INACTIVE },
    //     PinmuxInfo { port: 1, pin: 18, mode: TXD | INACTIVE }
    // ];
    // for pinmux_info in &PINMUX_INFO {
    //    pinmux_set(pinmux_info.port, pinmux_info.pin, pinmux_info.mode);
    // }

    setup_pinmux(&mut peripherals.IOCON);

    let usb_disconnected = is_usb_disconnected(&mut peripherals.GPIO_PORT);
    set_battery_power(&mut peripherals.GPIO_PORT, !usb_disconnected);

    let mut should_copy_ram_to_flash = [0; 4];
    iap::eeprom_read(0x500, &mut should_copy_ram_to_flash, unsafe { MAIN_CLOCK_FREQ } / 1024);
    let should_copy_ram_to_flash = u32::from_le_bytes(should_copy_ram_to_flash);
    if should_copy_ram_to_flash != 0 {
        iap::eeprom_write(0x500, &0u32.to_le_bytes(), unsafe { MAIN_CLOCK_FREQ } / 1024);
        weird_flash_function(should_copy_ram_to_flash);
        setup_watchdog(&peripherals.SYSCON, &peripherals.WWDT, 100);
    }

    if peripherals.PMU.gpreg[0].read().bits() == 0xecaabac0 {
        peripherals.PMU.gpreg[0].write(|v| unsafe { v.gpdata().bits(0) });
    } else if unsafe { *(0x2024 as *const u32) == 0xecaabac0 && EEPROM_CACHE.version != 0 } {
        enter_programming_mode_on_reboot(&mut peripherals.PMU, false);

        // Enable RAM1 clock before jumping to program2.
        peripherals
            .SYSCON
            .sysahbclkctrl
            .modify(|_, writer| writer.ram1().enabled());

        start_program2();
    }

    programming_mode::enter_programming_mode(core_peripherals, peripherals);
}

#[exception]
fn NonMaskableInt() {
    let program2_hdlr = unsafe { *(0x2008 as *const extern fn()) };
    program2_hdlr();
}

#[exception]
fn HardFault(_frame: &cortex_m_rt::ExceptionFrame) -> ! {
    let program2_hdlr = unsafe { *(0x200c as *const extern fn()) };
    program2_hdlr();
    loop {
        cortex_m::asm::wfi();
    }
}

#[exception]
fn SVCall() {
    let program2_hdlr = unsafe { *(0x202c as *const extern fn()) };
    program2_hdlr();
}

#[exception]
fn PendSV() {
    // TODO
    let peripherals = unsafe { Peripherals::steal() };

    if peripherals.PMU.gpreg[1].read().bits() == 0 {
        programming_mode::PendSV();
    } else {
        let program2_hdlr = unsafe { *(0x2038 as *const extern fn()) };
        program2_hdlr();
    }
}

#[exception]
fn SysTick() {
    let program2_hdlr = unsafe { *(0x203c as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn PIN_INT0() {
    let program2_hdlr = unsafe { *(0x2040 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn PIN_INT1() {
    let program2_hdlr = unsafe { *(0x2044 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn PIN_INT2() {
    let program2_hdlr = unsafe { *(0x2048 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn PIN_INT3() {
    let program2_hdlr = unsafe { *(0x204c as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn PIN_INT4() {
    let program2_hdlr = unsafe { *(0x2050 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn PIN_INT5() {
    let program2_hdlr = unsafe { *(0x2054 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn PIN_INT6() {
    let program2_hdlr = unsafe { *(0x2058 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn PIN_INT7() {
    let program2_hdlr = unsafe { *(0x205c as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn GINT0() {
    let program2_hdlr = unsafe { *(0x2060 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn GINT1() {
    let program2_hdlr = unsafe { *(0x2064 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn SSP1() {
    let program2_hdlr = unsafe { *(0x2078 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn I2C() {
    let program2_hdlr = unsafe { *(0x207c as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn CT16B0() {
    let program2_hdlr = unsafe { *(0x2080 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn CT16B1() {
    let program2_hdlr = unsafe { *(0x2084 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn CT32B0() {
    let program2_hdlr = unsafe { *(0x2088 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn CT32B1() {
    // TODO
    let peripherals = unsafe { Peripherals::steal() };

    if peripherals.PMU.gpreg[1].read().bits() == 0 {
        programming_mode::CT32B1();
    } else {
        let program2_hdlr = unsafe { *(0x208c as *const extern fn()) };
        program2_hdlr();
    }
}

#[interrupt]
fn SSP0() {
    let program2_hdlr = unsafe { *(0x2090 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn USART() {
    // TODO
    let peripherals = unsafe { Peripherals::steal() };

    if peripherals.PMU.gpreg[1].read().bits() == 0 {
        programming_mode::USART();
    } else {
        let program2_hdlr = unsafe { *(0x2094 as *const extern fn()) };
        program2_hdlr();
    }
}

#[interrupt]
fn USB_IRQ() {
    // TODO
    let peripherals = unsafe { Peripherals::steal() };

    if peripherals.PMU.gpreg[1].read().bits() == 0 {
        programming_mode::USB_IRQ();
    } else {
        let program2_hdlr = unsafe { *(0x2098 as *const extern fn()) };
        program2_hdlr();
    }
}

#[interrupt]
fn USB_FIQ() {
    let program2_hdlr = unsafe { *(0x209c as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn ADC() {
    let program2_hdlr = unsafe { *(0x20a0 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn WDT() {
    let program2_hdlr = unsafe { *(0x20a4 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn BOD_IRQ() {
    let program2_hdlr = unsafe { *(0x20a8 as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn FLASH_IRQ() {
    let program2_hdlr = unsafe { *(0x20ac as *const extern fn()) };
    program2_hdlr();
}

#[interrupt]
fn USBWAKEUP() {
    let program2_hdlr = unsafe { *(0x20b8 as *const extern fn()) };
    program2_hdlr();
}