//! Crystal Oscillator (XOSC)
// See [Chapter 2 Section 16](https://datasheets.raspberrypi.org/rp2040/rp2040_datasheet.pdf) for more details

use core::{convert::Infallible, ops::RangeInclusive};

use fugit::HertzU32;
use nb::Error::WouldBlock;

use crate::{pac::XOSC, typelevel::Sealed};

/// State of the Crystal Oscillator (typestate trait)
pub trait State: Sealed {}

/// XOSC is disabled (typestate)
pub struct Disabled;

/// XOSC is initialized, ie we've given parameters (typestate)
pub struct Initialized {
    freq_hz: HertzU32,
}

/// Stable state (typestate)
pub struct Stable {
    freq_hz: HertzU32,
}

/// XOSC is in dormant mode (see Chapter 2, Section 16, §5)
pub struct Dormant;

impl State for Disabled {}
impl Sealed for Disabled {}
impl State for Initialized {}
impl Sealed for Initialized {}
impl State for Stable {}
impl Sealed for Stable {}
impl State for Dormant {}
impl Sealed for Dormant {}

/// Possible errors when initializing the CrystalOscillator
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    /// Frequency is out of the 1-15MHz range (see datasheet)
    FrequencyOutOfRange,

    /// Argument is bad : overflows, ...
    BadArgument,
}

/// Blocking helper method to setup the XOSC without going through all the steps.
///
/// - `frequency` must be between 1MHz and 15MHz
/// - `stable_delay_millis` must be in the range `1..=1000` milliseconds and defines
/// the time to wait before the crystal reaches a stable and high enough amplitude to be usable.
///
/// See datasheet Chapter 2 Section 16
pub fn setup_xosc_blocking(
    xosc_dev: XOSC,
    frequency: HertzU32,
    stable_delay_millis: u32,
) -> Result<CrystalOscillator<Stable>, Error> {
    let initialized_xosc = CrystalOscillator::new(xosc_dev).initialize(frequency, stable_delay_millis)?;

    let stable_xosc_token = nb::block!(initialized_xosc.await_stabilization()).unwrap();

    Ok(initialized_xosc.get_stable(stable_xosc_token))
}

/// A Crystal Oscillator.
pub struct CrystalOscillator<S: State> {
    device: XOSC,
    state: S,
}

impl<S: State> CrystalOscillator<S> {
    /// Transitions the oscillator to another state.
    fn transition<To: State>(self, state: To) -> CrystalOscillator<To> {
        CrystalOscillator {
            device: self.device,
            state,
        }
    }

    /// Releases the underlying device.
    pub fn free(self) -> XOSC {
        self.device
    }
}

impl CrystalOscillator<Disabled> {
    /// Creates a new CrystalOscillator from the underlying device.
    pub fn new(dev: XOSC) -> Self {
        CrystalOscillator {
            device: dev,
            state: Disabled,
        }
    }

    /// Initializes the XOSC : frequency range is set, startup delay is calculated and set.
    ///
    /// - `frequency` must be between 1MHz and 15MHz
    /// - `stable_delay_millis` must be in the range `1..=1000` milliseconds and defines
    /// the time to wait before the crystal reaches a stable and high enough amplitude to be usable.
    ///
    /// See datasheet Chapter 2 Section 16
    pub fn initialize(self, frequency: HertzU32, stable_delay_millis: u32) -> Result<CrystalOscillator<Initialized>, Error> {
        const ALLOWED_FREQUENCY_RANGE: RangeInclusive<HertzU32> =
            HertzU32::MHz(1)..=HertzU32::MHz(15);

        if !ALLOWED_FREQUENCY_RANGE.contains(&frequency) {
            return Err(Error::FrequencyOutOfRange);
        }

        self.device.ctrl.write(|w| {
            w.freq_range()._1_15mhz();
            w
        });

        // See Chapter 2, Section 16, §3)
        // startup_delay = (freq_hz * STABLE_DELAY) / 256
        //               = (freq_hz * (delay_in_millis / 1000)) / 256
        //               = (freq_hz * delay_in_millis) / (1000 * 256)
        //               = (freq_khz * delay_in_millis) / 256
        // We do the calculation first.
        match stable_delay_millis {
            0 => return Err(Error::BadArgument),
            1..=1000 => (),
            _ => return Err(Error::BadArgument),
        }
        // Convert to kHZ first so that 15_000 * 1_000 is the max numerator, thus we can't overflow u32
        let startup_delay = (frequency.to_kHz() * stable_delay_millis) / 256;

        // We already checked freq is 1Mhz..=15Mhz and millis is between 1 and 1000.
        // The maximum value possible for the above calculation is then,
        //
        // (15_000 * 1000) / 256 = 58593
        //
        // which is within the bounds of a u16, so no check is necessary.
        let startup_delay = startup_delay as u16;

        self.device.startup.write(|w| unsafe {
            w.delay().bits(startup_delay);
            w
        });

        self.device.ctrl.write(|w| {
            w.enable().enable();
            w
        });

        Ok(self.transition(Initialized { freq_hz: frequency }))
    }
}

/// A token that's given when the oscillator is stablilzed, and can be exchanged to proceed to the next stage.
pub struct StableOscillatorToken {
    _private: (),
}

impl CrystalOscillator<Initialized> {
    /// One has to wait for the startup delay before using the oscillator, ie awaiting stablilzation of the XOSC
    pub fn await_stabilization(&self) -> nb::Result<StableOscillatorToken, Infallible> {
        if self.device.status.read().stable().bit_is_clear() {
            return Err(WouldBlock);
        }

        Ok(StableOscillatorToken { _private: () })
    }

    /// Returns the stablilzed oscillator
    pub fn get_stable(self, _token: StableOscillatorToken) -> CrystalOscillator<Stable> {
        let freq_hz = self.state.freq_hz;
        self.transition(Stable { freq_hz })
    }
}

impl CrystalOscillator<Stable> {
    /// Operating frequency of the XOSC in hertz
    pub fn operating_frequency(&self) -> HertzU32 {
        self.state.freq_hz
    }

    /// Disables the XOSC
    pub fn disable(self) -> CrystalOscillator<Disabled> {
        self.device.ctrl.modify(|_r, w| {
            w.enable().disable();
            w
        });

        self.transition(Disabled)
    }

    /// Put the XOSC in DORMANT state.
    ///
    /// # Safety
    /// This method is marked unsafe because prior to switch the XOSC into DORMANT state,
    /// PLLs must be stopped and IRQs have to be properly configured.
    /// This method does not do any of that, it merely switches the XOSC to DORMANT state.
    /// See Chapter 2, Section 16, §5) for details.
    pub unsafe fn dormant(self) -> CrystalOscillator<Dormant> {
        //taken from the C SDK
        const XOSC_DORMANT_VALUE: u32 = 0x636f6d61;

        self.device.dormant.write(|w| {
            w.bits(XOSC_DORMANT_VALUE);
            w
        });

        self.transition(Dormant)
    }
}
