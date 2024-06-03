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

mod unreal;

use asr::{
    future::{next_tick, retry},
    settings::Gui,
    time::Duration,
    timer,
    timer::TimerState,
    watcher::Watcher,
    Process,
};

use crate::unreal::{Module, UnrealPointer};

asr::panic_handler!();
asr::async_main!(nightly);

async fn main() {
    let mut settings = Settings::register();

    loop {
        // Hook to the target process
        let process = retry(|| PROCESS_NAMES.into_iter().find_map(Process::attach)).await;

        process
            .until_closes(async {
                // Once the target has been found and attached to, set up some default watchers
                let mut watchers = Watchers::default();

                // Perform memory scanning to look for the addresses we need
                let addresses = Addresses::init(&process).await;

                loop {
                    // Splitting logic. Adapted from OG LiveSplit:
                    // Order of execution
                    // 1. update() will always be run first. There are no conditions on the execution of this action.
                    // 2. If the timer is currently either running or paused, then the isLoading, gameTime, and reset actions will be run.
                    // 3. If reset does not return true, then the split action will be run.
                    // 4. If the timer is currently not running (and not paused), then the start action will be run.ù
                    settings.update();
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
            })
            .await;
    }
}

#[derive(Default)]
struct Watchers {
    is_loading: Watcher<bool>,
    player_exp: Watcher<u64>,
    level: Watcher<Map>,
}

#[derive(Gui)]
struct Settings {
    #[default = true]
    /// AUTO START
    start: bool,
}

struct Addresses {
    unreal_module: Module,
    current_level: UnrealPointer<4>,
    player_exp: UnrealPointer<8>,
    no_of_online_players: UnrealPointer<4>,
    is_loading_single: UnrealPointer<3>,
}

impl Addresses {
    async fn init(game: &Process) -> Self {
        let main_module = retry(|| {
            PROCESS_NAMES
                .iter()
                .find_map(|m| game.get_module_address(m).ok())
        })
        .await;

        let unreal = retry(|| Module::attach(game, main_module)).await;

        let current_level =
            UnrealPointer::<4>::new(unreal.g_engine(), &["GameViewport", "World", "0x4B8", "0"]);
        let player_exp = UnrealPointer::<8>::new(
            unreal.g_engine(),
            &[
                "GameViewport",
                "GameInstance",
                "LocalPlayers",
                "0",
                "PlayerController",
                "Pawn",
                "Experience",
                "CurrentExperienceAndLevel.Level",
            ],
        );
        let no_of_online_players = UnrealPointer::<4>::new(
            unreal.g_engine(),
            &[
                "GameViewport",
                "GameInstance",
                "ArkNetClientMatchmaking",
                "0x60",
            ],
        );
        let is_loading_single = UnrealPointer::<3>::new(
            unreal.g_engine(),
            &["GameViewport", "GameInstance", "0x570"],
        );

        Self {
            unreal_module: unreal,
            current_level,
            player_exp,
            no_of_online_players,
            is_loading_single,
        }
    }
}

fn update_loop(game: &Process, addresses: &Addresses, watchers: &mut Watchers) {
    let no_of_online_players = addresses
        .no_of_online_players
        .deref::<u32>(&game, &addresses.unreal_module)
        .unwrap_or_default();

    let level = addresses
        .current_level
        .deref::<[u16; 100]>(&game, &addresses.unreal_module)
        .map(|n| n.map(|val| val as u8));

    let level = level.map(|val| {
        let map_name = &val[..val.iter().position(|&b| b == 0).unwrap_or(val.len())];

        match map_name {
            b"/Game/Maps/Campaign/FrontEnd/FrontEnd" => Map::MainMenu,
            b"/Game/Maps/Campaign/District_01/District_01" => Map::RedfallCommons,
            b"/Game/Maps/Campaign/District_02/District_02" => Map::BurialPoint,
            _ => match watchers.level.pair {
                Some(x) => x.current,
                _ => Map::MainMenu,
            },
        }
    });

    watchers
        .is_loading
        .update_infallible(match no_of_online_players {
            0 => {
                addresses
                    .is_loading_single
                    .deref::<u32>(&game, &addresses.unreal_module)
                    .unwrap_or_default()
                    != 0
            }
            _ => level.is_some_and(|val| val == Map::MainMenu),
        });

    watchers
        .level
        .update_infallible(level.unwrap_or_else(|| Map::MainMenu));
    watchers.player_exp.update_infallible(
        addresses
            .player_exp
            .deref::<u64>(&game, &addresses.unreal_module)
            .unwrap_or_default(),
    );
}

fn start(watchers: &Watchers, settings: &Settings) -> bool {
    if !settings.start {
        return false;
    }
    let Some(is_loading) = &watchers.is_loading.pair else {
        return false;
    };
    let Some(level) = &watchers.level.pair else {
        return false;
    };
    let Some(player_exp) = &watchers.player_exp.pair else {
        return false;
    };

    !is_loading.current
        && is_loading.old
        && level.current == Map::RedfallCommons
        && player_exp.current == 0
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
