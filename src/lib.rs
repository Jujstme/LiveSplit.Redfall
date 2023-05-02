#![no_std]

use asr::{
    signature::Signature, time::Duration, timer, timer::TimerState,
    Address, Process, sync
};

#[cfg(all(not(test), target_arch = "wasm32"))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

static AUTOSPLITTER: sync::Mutex<State> = sync::Mutex::new(State {
    game: None,
    watchers: Watchers {
        is_loading: false,
    },
    // settings: None,
});

struct State {
    game: Option<ProcessInfo>,
    watchers: Watchers,
    // settings: Option<Settings>,
}

struct ProcessInfo {
    game: Process,
    addresses: Option<MemoryPtr>,
}

struct Watchers {
    is_loading: bool,
}

struct MemoryPtr {
    g_world: Address,
}

/*
#[derive(asr::Settings)]
struct Settings {
    #[default = true]
    /// AUTO START
    start: bool,
}
*/

impl ProcessInfo {
    fn attach_process() -> Option<Self> {
        let game = PROCESS_NAMES.iter()
            .find_map(|m| Process::attach(m))?;

        Some(Self {
            game,
            addresses: None,
        })
    }

    fn look_for_addresses(&mut self) -> Option<MemoryPtr> {
        const SIG_GWORLD: Signature<15> = Signature::new("80 7C 24 ?? 00 ?? ?? 48 8B 3D ???????? 48");
        let game = &self.game;

        let (Ok(main_module_base), Ok(main_module_size)) = PROCESS_NAMES.iter()
            .map(|m| (game.get_module_address(m), game.get_module_size(m)))
            .find(|m| m.0.is_ok() && m.1.is_ok())?
            else {
                return None
            };

        let ptr = SIG_GWORLD.scan_process_range(game, main_module_base, main_module_size)?.0 as i64 + 10;
        let g_world = Address((ptr + 0x4 + game.read::<i32>(Address(ptr as u64)).ok()? as i64) as u64);

        Some(MemoryPtr {
            g_world,
        })
    }
}

impl State {
    fn init(&mut self) -> bool {
        if self.game.is_none() {
            self.game = ProcessInfo::attach_process()
        }

        let Some(game) = &mut self.game else {
            return false
        };

        if !game.game.is_open() {
            self.game = None;
            return false;
        }

        if game.addresses.is_none() {
            game.addresses = game.look_for_addresses()
        }

        game.addresses.is_some()
    }

    fn update(&mut self) {
        let Some(game) = &self.game else { return };
        let Some(addresses) = &game.addresses else { return };
        let proc = &game.game;


        let mut is_loading = true;

        if let Ok(g_world) = proc.read::<u64>(addresses.g_world) {
            if let Ok(owninggameinstance) = proc.read::<u64>(Address(g_world + 0x180)) {
                if let Ok(load_addr) = proc.read::<u32>(Address(owninggameinstance + 0x560)) {
                    is_loading = load_addr > 0;
                }
            }
        }

        self.watchers.is_loading = is_loading;
    }

    fn start(&mut self) -> bool {
        false
    }

    fn split(&mut self) -> bool {
        false
    }

    fn reset(&mut self) -> bool {
        false
    }

    fn is_loading(&mut self) -> Option<bool> {
        Some(self.watchers.is_loading)
    }

    fn game_time(&mut self) -> Option<Duration> {
        None
    }
}

#[no_mangle]
pub extern "C" fn update() {
    // Get access to the spinlock
    let autosplitter = &mut AUTOSPLITTER.lock();

    // Sets up the settings
    // autosplitter.settings.get_or_insert_with(Settings::register);

    // Main autosplitter logic, essentially refactored from the OG LivaSplit autosplitting component.
    // First of all, the autosplitter needs to check if we managed to attach to the target process,
    // otherwise there's no need to proceed further.
    if !autosplitter.init() {
        return;
    }

    // The main update logic is launched with this
    autosplitter.update();

    // Splitting logic. Adapted from OG LiveSplit:
    // Order of execution
    // 1. update() [this is launched above] will always be run first. There are no conditions on the execution of this action.
    // 2. If the timer is currently either running or paused, then the isLoading, gameTime, and reset actions will be run.
    // 3. If reset does not return true, then the split action will be run.
    // 4. If the timer is currently not running (and not paused), then the start action will be run.
    let timer_state = timer::state();
    if timer_state == TimerState::Running || timer_state == TimerState::Paused {
        if let Some(is_loading) = autosplitter.is_loading() {
            if is_loading {
                timer::pause_game_time()
            } else {
                timer::resume_game_time()
            }
        }

        if let Some(game_time) = autosplitter.game_time() {
            timer::set_game_time(game_time)
        }

        if autosplitter.reset() {
            timer::reset()
        } else if autosplitter.split() {
            timer::split()
        }
    }

    if timer::state() == TimerState::NotRunning && autosplitter.start() {
        timer::start();

        if let Some(is_loading) = autosplitter.is_loading() {
            if is_loading {
                timer::pause_game_time()
            } else {
                timer::resume_game_time()
            }
        }
    }
}

const PROCESS_NAMES: [&str; 1] = ["Redfall.exe"];
