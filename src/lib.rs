#![no_std]
#![feature(type_alias_impl_trait, const_async_blocks)]
#![warn(
    clippy::complexity,
    clippy::correctness,
    clippy::perf,
    clippy::style,
    clippy::undocumented_unsafe_blocks,
    rust_2018_idioms
)]

use asr::{future::{retry, next_tick}, timer, timer::TimerState, watcher::Watcher, Address, Process, time::Duration, signature::Signature, Address64};


asr::panic_handler!();
asr::async_main!(nightly);


async fn main() {
    let settings = Settings::register();

    loop {
        // Hook to the target process
        let process = retry(|| PROCESS_NAMES.into_iter().find_map(Process::attach)).await;

        process.until_closes(async {
            // Once the target has been found and attached to, set up some default watchers
            let mut watchers = Watchers::default();

            // Perform memory scanning to look for the addresses we need
            let addresses = retry(|| Addresses::init(&process)).await;

            loop {
                // Splitting logic. Adapted from OG LiveSplit:
                // Order of execution
                // 1. update() will always be run first. There are no conditions on the execution of this action.
                // 2. If the timer is currently either running or paused, then the isLoading, gameTime, and reset actions will be run.
                // 3. If reset does not return true, then the split action will be run.
                // 4. If the timer is currently not running (and not paused), then the start action will be run.
                update_loop(&process, &addresses, &mut watchers);

                let timer_state = timer::state();
                if timer_state == TimerState::Running || timer_state == TimerState::Paused {
                    if let Some(is_loading) = is_loading(&watchers, &settings) {
                        if is_loading {
                            timer::pause_game_time()
                        } else {
                            timer::resume_game_time()
                        }
                    }

                    if let Some(game_time) = game_time(&watchers, &settings) {
                        timer::set_game_time(game_time)
                    }

                    if reset(&watchers, &settings) {
                        timer::reset()
                    } else if split(&watchers, &settings) {
                        timer::split()
                    }
                }

                if timer::state() == TimerState::NotRunning && start(&watchers, &settings) {
                    timer::start();
                    timer::pause_game_time();

                    if let Some(is_loading) = is_loading(&watchers, &settings) {
                        if is_loading {
                            timer::pause_game_time()
                        } else {
                            timer::resume_game_time()
                        }
                    }
                }

                next_tick().await;
            }
        }).await;
    }
}

#[derive(Default)]
struct Watchers {
    is_loading: Watcher<bool>,
    player_exp: Watcher<u64>,
    level: Watcher<Map>,
}


#[derive(asr::Settings)]
struct Settings {
    #[default = true]
    /// AUTO START
    start: bool,
}

struct Addresses {
    g_engine: Address,
}

impl Addresses {
    fn init(game: &Process) -> Option<Addresses> {
        const SIG_GENGINE: Signature<7> = Signature::new("A8 01 75 ?? 48 C7 05");

        let main_module = PROCESS_NAMES.iter()
            .find_map(|m| game.get_module_range(m).ok())?;

        let ptr = SIG_GENGINE.scan_process_range(game, main_module)?.add(7);
        let g_engine = ptr.add(8).add_signed(game.read::<i32>(ptr).ok()? as i64);

        Some(Self {
            g_engine,
        })
    }
}

fn update_loop(game: &Process, addresses: &Addresses, watchers: &mut Watchers) {
    let mut is_loading = true;
    let mut is_coop = bool::default();
    let mut player_exp = u64::default();
    let mut current_level = match &watchers.level.pair { Some(x) => x.current, _ => Map::default() };

    if let Ok(g_engine) = game.read::<Address64>(addresses.g_engine) {
        if let Ok(game_view_port) = game.read::<Address64>(g_engine.add(0x7B8)) {

            // Current map
            if let Ok(world) = game.read::<Address64>(game_view_port.add(0x78)) {
                if let Ok(level) = game.read::<Address64>(world.add(0x4B8)) {
                    if let Ok(map_name) = game.read::<[u16; 100]>(level) {
                        let map = map_name.map(|n| n as u8);
                        let map_name = &map[..map.iter().position(|&b| b == 0).unwrap_or(map.len())];

                        current_level = match map_name {
                            b"/Game/Maps/Campaign/FrontEnd/FrontEnd" => Map::MainMenu,
                            b"/Game/Maps/Campaign/District_01/District_01" => Map::RedfallCommons,
                            b"/Game/Maps/Campaign/District_02/District_02" => Map::BurialPoint,
                            _ => current_level,
                        };
                    }
                }
            }

            if let Ok(game_instance) = game.read::<Address64>(game_view_port.add(0x80)) {
                if current_level != Map::MainMenu {
                    if let Ok(local_players) = game.read::<Address64>(game_instance.add(0x38)) {
                        if let Ok(player0) = game.read::<Address64>(local_players) {
                            if let Ok(player_controller) = game.read::<Address64>(player0.add(0x30)) {
                                if let Ok(pawn) = game.read::<Address64>(player_controller.add(0x268)) {
                                    if let Ok(experience) = game.read::<Address64>(pawn.add(0xDC0)) {
                                        if let Ok(exp) = game.read::<u64>(experience.add(0xE0)) {
                                                player_exp = exp;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if let Ok(ark_net_client_matchmaking) = game.read::<Address64>(game_instance.add(0x8A8)) {
                    if let Ok(no_of_players) = game.read::<u32>(ark_net_client_matchmaking.add(0x60)) {
                        is_coop = no_of_players > 0;
                    }
                }

                if let Ok(load_addr) = game.read::<u32>(game_instance.add(if is_coop { 0x520 } else { 0x560 })) {
                    is_loading = if is_coop { current_level == Map::MainMenu /* || load_addr != 0 */ } else { load_addr != 0 };
                }
            }
        }
    }

    watchers.is_loading.update_infallible(is_loading);
    watchers.level.update_infallible(current_level);
    watchers.player_exp.update_infallible(player_exp);
}

fn start(watchers: &Watchers, settings: &Settings) -> bool {
    if !settings.start { return false }
    let Some(is_loading) = &watchers.is_loading.pair else { return false };
    let Some(level) = &watchers.level.pair else { return false };
    let Some(player_exp) = &watchers.player_exp.pair else { return false };

    !is_loading.current && is_loading.old && level.current == Map::RedfallCommons && player_exp.current == 0
}

fn split(_watchers: &Watchers, _settings: &Settings) -> bool {
    false
}

fn reset(_watchers: &Watchers, _settings: &Settings) -> bool {
    false
}

fn is_loading(watchers: &Watchers, _settings: &Settings) -> Option<bool> {
    Some(watchers.is_loading.pair?.current)
}

fn game_time(_watchers: &Watchers, _settings: &Settings) -> Option<Duration> {
    None
}

#[derive(Copy, Clone, PartialEq, Default)]
enum Map {
    #[default]
    MainMenu,
    RedfallCommons,
    BurialPoint,
}

const PROCESS_NAMES: [&str; 1] = ["Redfall.exe"];