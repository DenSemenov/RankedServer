use std::net::SocketAddr;

use nalgebra::{Matrix3, Point3, Rotation3, Vector2, Vector3};
use std::cmp::min;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::hqm_game::{
    HQMGame, HQMGameObject, HQMGameState, HQMGameWorld, HQMIcingStatus, HQMMessage,
    HQMOffsideStatus, HQMPlayerInput, HQMPuck, HQMRink, HQMRulesState, HQMSkaterHand, HQMTeam,
    RHQMGamePlayer, RHQMPlayer,
};
use crate::hqm_parse::{HQMMessageReader, HQMMessageWriter, HQMObjectPacket};
use crate::hqm_simulate::HQMSimulationEvent;
use bytes::{Bytes, BytesMut};
use rand::Rng;
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::f32::consts::{FRAC_PI_2, PI};
use std::rc::Rc;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::info;

use std::error::Error;
use std::net::IpAddr;
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

const GAME_HEADER: &[u8] = b"Hock";

pub struct HQMSavedTick {
    packets: Vec<HQMObjectPacket>,
    time: Instant,
}

enum HQMServerReceivedData {
    GameClientPacket { addr: SocketAddr, data: Bytes },
}

pub(crate) struct HQMServer {
    pub(crate) players: Vec<Option<HQMConnectedPlayer>>,
    pub(crate) ban_list: HashSet<std::net::IpAddr>,
    pub(crate) allow_join: bool,
    pub(crate) allow_ranked_join: bool,
    pub(crate) config: HQMServerConfiguration,
    pub(crate) game: HQMGame,
    game_alloc: u32,
    pub(crate) is_muted: bool,
    pub(crate) last_sec: u64,
}

impl HQMServer {
    async fn handle_message(&mut self, addr: SocketAddr, socket: &Arc<UdpSocket>, msg: &[u8]) {
        let mut parser = HQMMessageReader::new(&msg);
        let header = parser.read_bytes_aligned(4);
        if header != GAME_HEADER {
            return;
        }

        let command = parser.read_byte_aligned();
        match command {
            0 => {
                self.request_info(socket, addr, &mut parser);
            }
            2 => {
                self.player_join(addr, &mut parser);
            }
            // if 8 or 0x10, client is modded, probly want to send it to the player_update function to store it in the client/player struct, to use when responding to clients
            4 | 8 | 0x10 => {
                self.player_update(addr, &mut parser, command);
            }
            7 => {
                self.player_exit(addr);
            }
            _ => {}
        }
    }

    fn request_info<'a>(
        &self,
        socket: &Arc<UdpSocket>,
        addr: SocketAddr,
        parser: &mut HQMMessageReader<'a>,
    ) {
        let mut write_buf = vec![0u8; 512];
        let _player_version = parser.read_bits(8);
        let ping = parser.read_u32_aligned();

        let mut writer = HQMMessageWriter::new(&mut write_buf);
        writer.write_bytes_aligned(GAME_HEADER);
        writer.write_byte_aligned(1);
        writer.write_bits(8, 55);
        writer.write_u32_aligned(ping);

        let player_count = self.player_count();
        writer.write_bits(8, player_count as u32);
        writer.write_bits(4, 4);
        writer.write_bits(4, self.config.team_max as u32);

        writer.write_bytes_aligned_padded(32, self.config.server_name.as_ref());

        let written = writer.get_bytes_written();
        let socket = socket.clone();
        let addr = addr.clone();
        tokio::spawn(async move {
            let slice = &write_buf[0..written];
            let _ = socket.send_to(slice, addr).await;
        });
    }

    fn player_count(&self) -> usize {
        let mut player_count = 0;
        for player in &self.players {
            if player.is_some() {
                player_count += 1;
            }
        }
        player_count
    }

    fn player_update(&mut self, addr: SocketAddr, parser: &mut HQMMessageReader, command: u8) {
        let current_slot = self.find_player_slot(addr);
        let (player_index, player) = match current_slot {
            Some(x) => (x, self.players[x].as_mut().unwrap()),
            None => {
                return;
            }
        };

        // Set client version based on the command used to trigger player_update
        // Huge thank you to Baba for his help with this!
        match command {
            4 => {
                player.client_version = 0; // Cryptic
            }
            8 => {
                player.client_version = 1; // Baba - Ping
            }
            0x10 => {
                player.client_version = 2; // Baba - Ping + Rules
            }
            _ => {}
        }

        let current_game_id = parser.read_u32_aligned();

        let input_stick_angle = parser.read_f32_aligned();
        let input_turn = parser.read_f32_aligned();
        let input_unknown = parser.read_f32_aligned();
        let input_fwbw = parser.read_f32_aligned();
        let input_stick_rot_1 = parser.read_f32_aligned();
        let input_stick_rot_2 = parser.read_f32_aligned();
        let input_head_rot = parser.read_f32_aligned();
        let input_body_rot = parser.read_f32_aligned();
        let input_keys = parser.read_u32_aligned();
        let input = HQMPlayerInput {
            stick_angle: input_stick_angle,
            turn: input_turn,
            unknown: input_unknown,
            fwbw: input_fwbw,
            stick: Vector2::new(input_stick_rot_1, input_stick_rot_2),
            head_rot: input_head_rot,
            body_rot: input_body_rot,
            keys: input_keys,
        };

        // if modded client get deltatime
        if player.client_version > 0 {
            let delta = parser.read_u32_aligned();
            player.deltatime = delta;
        }

        let packet = parser.read_u32_aligned();

        if player.game_id == current_game_id && player.known_packet < packet {
            if let Some(diff) = self.game.packet.checked_sub(packet) {
                let diff = diff as usize;
                let t1 = Instant::now();
                if let Some(t2) = self.game.saved_ticks.get(diff).map(|x| x.time) {
                    if let Some(duration) = t1.checked_duration_since(t2) {
                        player.last_ping.truncate(100 - 1);
                        player.last_ping.push_front(duration.as_secs_f32());
                    }
                }
            }
        }

        player.inactivity = 0;
        player.known_packet = packet;
        player.input = input;
        player.game_id = current_game_id;
        player.known_msgpos = parser.read_u16_aligned() as usize;

        let has_chat_msg = parser.read_bits(1) == 1;
        if has_chat_msg {
            let rep = parser.read_bits(3) as u8;
            if player.chat_rep != Some(rep) {
                player.chat_rep = Some(rep);
                let byte_num = parser.read_bits(8) as usize;
                let message = parser.read_bytes_aligned(byte_num);
                self.process_message(message, player_index);
            }
        }
    }

    fn player_join(&mut self, addr: SocketAddr, parser: &mut HQMMessageReader) {
        let player_count = self.player_count();
        let max_player_count = self.config.player_max;
        if player_count >= max_player_count {
            return; // Ignore join request
        }
        let player_version = parser.read_bits(8);
        if player_version != 55 {
            return; // Not the right version
        }
        let current_slot = self.find_player_slot(addr);
        if current_slot.is_some() {
            return; // Player has already joined
        }

        // Check ban list
        if self.ban_list.contains(&addr.ip()) {
            return;
        }

        let player_name_bytes = parser.read_bytes_aligned(32);
        let player_name = get_player_name(player_name_bytes);
        match player_name {
            Some(name) => {
                if let Some(player_index) = self.add_player(name.clone(), addr) {
                    info!(
                        "{} ({}) joined server from address {:?}",
                        name, player_index, addr
                    );
                    let msg = format!("{} joined", name);
                    self.add_server_chat_message(msg);
                }
            }
            _ => {}
        };
    }

    fn set_hand(&mut self, hand: HQMSkaterHand, player_index: usize) {
        if let Some(player) = &mut self.players[player_index] {
            player.hand = hand;
            if let Some(skater_obj_index) = player.skater {
                if let HQMGameObject::Player(skater) =
                    &mut self.game.world.objects[skater_obj_index]
                {
                    if self.game.state == HQMGameState::Game {
                        let msg = format!("Stick hand will change after next intermission");
                        self.add_directed_server_chat_message(msg, player_index);

                        return;
                    }

                    skater.hand = hand;
                }
            }
        }
    }

    fn process_command(&mut self, command: &str, arg: &str, player_index: usize) {
        match command {
            "login" => {
                self.login(player_index, arg);
            }
            "l" => {
                self.login(player_index, arg);
            }
            "vote" => {
                if let Ok(game) = arg.parse::<usize>() {
                    self.vote(player_index, game);
                }
            }
            "v" => {
                if let Ok(game) = arg.parse::<usize>() {
                    self.vote(player_index, game);
                }
            }
            "afk" => {
                self.afk(player_index);
            }
            "here" => {
                self.here(player_index);
            }
            "enablejoin" => {
                self.set_allow_join(player_index, true);
            }
            "disablejoin" => {
                self.set_allow_join(player_index, false);
            }
            "mute" => {
                if let Ok(mute_player_index) = arg.parse::<usize>() {
                    if mute_player_index < self.players.len() {
                        self.mute_player(player_index, mute_player_index);
                    }
                }
            }
            "unmute" => {
                if let Ok(mute_player_index) = arg.parse::<usize>() {
                    if mute_player_index < self.players.len() {
                        self.unmute_player(player_index, mute_player_index);
                    }
                }
            }
            /*"shadowmute" => {
                if let Ok(mute_player_index) = arg.parse::<usize>() {
                    if mute_player_index < self.players.len() {
                        self.shadowmute_player(player_index, mute_player_index);
                    }
                }
            },*/
            "mutechat" => {
                self.mute_chat(player_index);
            }
            "unmutechat" => {
                self.unmute_chat(player_index);
            }
            "kick" => {
                if let Ok(kick_player_index) = arg.parse::<usize>() {
                    if kick_player_index < self.players.len() {
                        self.kick_player(player_index, kick_player_index, false);
                    }
                }
            }
            "kickall" => {
                self.kick_all_matching(player_index, arg, false);
            }
            "ban" => {
                if let Ok(kick_player_index) = arg.parse::<usize>() {
                    if kick_player_index < self.players.len() {
                        self.kick_player(player_index, kick_player_index, true);
                    }
                }
            }
            "banall" => {
                self.kick_all_matching(player_index, arg, true);
            }
            "clearbans" => {
                self.clear_bans(player_index);
            }
            "set" => {
                let args = arg.split(" ").collect::<Vec<&str>>();
                if args.len() > 1 {
                    match args[0] {
                        "redscore" => {
                            let input_score = match args[1].parse::<i32>() {
                                Ok(input_score) => input_score,
                                Err(_) => -1,
                            };

                            if input_score >= 0 {
                                self.set_score(HQMTeam::Red, input_score as u32, player_index)
                            }
                        }
                        "bluescore" => {
                            let input_score = match args[1].parse::<i32>() {
                                Ok(input_score) => input_score,
                                Err(_) => -1,
                            };

                            if input_score >= 0 {
                                self.set_score(HQMTeam::Blue, input_score as u32, player_index)
                            }
                        }
                        "period" => {
                            let input_period = match args[1].parse::<i32>() {
                                Ok(input_period) => input_period,
                                Err(_) => -1,
                            };

                            if input_period >= 0 {
                                self.set_period(input_period as u32, player_index)
                            }
                        }
                        "mercy" => {
                            let mercy = match args[1].parse::<i32>() {
                                Ok(mercy) => mercy,
                                Err(_) => -1,
                            };

                            if mercy >= 0 {
                                self.set_mercy(mercy as u32, player_index)
                            }
                        }
                        "clock" => {
                            let time_part_string = match args[1].parse::<String>() {
                                Ok(time_part_string) => time_part_string,
                                Err(_) => {
                                    return;
                                }
                            };

                            let time_parts: Vec<&str> = time_part_string.split(':').collect();

                            if time_parts.len() >= 2 {
                                let time_minutes = match time_parts[0].parse::<i32>() {
                                    Ok(time_minutes) => time_minutes,
                                    Err(_) => -1,
                                };

                                let time_seconds = match time_parts[1].parse::<i32>() {
                                    Ok(time_seconds) => time_seconds,
                                    Err(_) => -1,
                                };

                                if time_minutes < 0 || time_seconds < 0 {
                                    return;
                                }

                                self.set_clock(
                                    time_minutes as u32,
                                    time_seconds as u32,
                                    player_index,
                                );
                            }
                        }
                        "hand" => match args[1] {
                            "left" => {
                                self.set_hand(HQMSkaterHand::Left, player_index);
                            }
                            "right" => {
                                self.set_hand(HQMSkaterHand::Right, player_index);
                            }
                            _ => {}
                        },
                        "icing" => {
                            if let Some(arg) = args.get(1) {
                                self.set_icing_rule(player_index, arg);
                            }
                        }
                        "offside" => {
                            if let Some(arg) = args.get(1) {
                                self.set_offside_rule(player_index, arg);
                            }
                        }
                        "teamsize" => {
                            if let Some(arg) = args.get(1) {
                                self.set_team_size(player_index, arg);
                            }
                        }
                        "teamparity" => {
                            if let Some(arg) = args.get(1) {
                                self.set_team_parity(player_index, arg);
                            }
                        }
                        "replay" => {
                            if let Some(arg) = args.get(1) {
                                self.set_replay(player_index, arg);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "sp" | "setposition" => {
                self.set_preferred_faceoff_position(player_index, arg);
            }
            "admin" => {
                self.admin_login(player_index, arg);
            }
            "faceoff" => {
                self.faceoff(player_index);
            }
            "start" | "startgame" => {
                self.start_game(player_index);
            }
            "reset" | "resetgame" => {
                self.reset_game(player_index);
            }
            "pause" | "pausegame" => {
                self.pause(player_index);
            }
            "unpause" | "unpausegame" => {
                self.unpause(player_index);
            }
            "lefty" => {
                self.set_hand(HQMSkaterHand::Left, player_index);
            }
            "righty" => {
                self.set_hand(HQMSkaterHand::Right, player_index);
            }
            "list" => {
                if arg.is_empty() {
                    self.list_players(player_index, 0);
                } else if let Ok(first_index) = arg.parse::<usize>() {
                    self.list_players(player_index, first_index);
                }
            }
            "search" => {
                self.search_players(player_index, arg);
            }
            "view" => {
                if let Ok(view_player_index) = arg.parse::<usize>() {
                    self.view(view_player_index, player_index);
                }
            }
            "restoreview" => {
                if let Some(player) = &mut self.players[player_index] {
                    if player.view_player_index != player_index {
                        player.view_player_index = player_index;
                        self.add_directed_server_chat_message(
                            "View has been restored".to_string(),
                            player_index,
                        );
                    }
                }
            }
            "ping" => {
                if let Ok(ping_player_index) = arg.parse::<usize>() {
                    self.ping(ping_player_index, player_index);
                }
            }
            "pings" => {
                if let Some((ping_player_index, _name)) = self.player_exact_unique_match(arg) {
                    self.ping(ping_player_index, player_index);
                } else {
                    let matches = self.player_search(arg);
                    if matches.is_empty() {
                        self.add_directed_server_chat_message(
                            "No matches found".to_string(),
                            player_index,
                        );
                    } else if matches.len() > 1 {
                        self.add_directed_server_chat_message(
                            "Multiple matches found, use /ping X".to_string(),
                            player_index,
                        );
                        for (found_player_index, found_player_name) in matches.into_iter().take(5) {
                            self.add_directed_server_chat_message(
                                format!("{}: {}", found_player_index, found_player_name),
                                player_index,
                            );
                        }
                    } else {
                        self.ping(matches[0].0, player_index);
                    }
                }
            }
            "views" => {
                if let Some((view_player_index, _name)) = self.player_exact_unique_match(arg) {
                    self.view(view_player_index, player_index);
                } else {
                    let matches = self.player_search(arg);
                    if matches.is_empty() {
                        self.add_directed_server_chat_message(
                            "No matches found".to_string(),
                            player_index,
                        );
                    } else if matches.len() > 1 {
                        self.add_directed_server_chat_message(
                            "Multiple matches found, use /view X".to_string(),
                            player_index,
                        );
                        for (found_player_index, found_player_name) in matches.into_iter().take(5) {
                            self.add_directed_server_chat_message(
                                format!("{}: {}", found_player_index, found_player_name),
                                player_index,
                            );
                        }
                    } else {
                        self.view(matches[0].0, player_index);
                    }
                }
            }
            "icing" => {
                self.set_icing_rule(player_index, arg);
            }
            "offside" => {
                self.set_offside_rule(player_index, arg);
            }
            "rules" => {
                let offside_str = match self.config.offside {
                    HQMOffsideConfiguration::Off => "Offside disabled",
                    HQMOffsideConfiguration::Delayed => "Offside enabled",
                    HQMOffsideConfiguration::Immediate => "Immediate offside enabled",
                };
                let icing_str = match self.config.icing {
                    HQMIcingConfiguration::Off => "Icing disabled",
                    HQMIcingConfiguration::Touch => "Icing enabled",
                    HQMIcingConfiguration::NoTouch => "No-touch icing enabled",
                };
                let msg = format!("{}, {}", offside_str, icing_str);
                self.add_directed_server_chat_message(msg, player_index);
            }
            "cheat" => {
                if self.config.cheats_enabled {
                    self.cheat(player_index, arg);
                }
            }
            /*
            "test" => {
                let rink = &self.game.world.rink;
                let faceoff_spot = match arg {
                    "c" => Some(rink.center_faceoff_spot.clone()),
                    "r1" => Some(rink.red_zone_faceoff_spots[0].clone()),
                    "r2" => Some(rink.red_zone_faceoff_spots[1].clone()),
                    "b1" => Some(rink.blue_zone_faceoff_spots[0].clone()),
                    "b2" => Some(rink.blue_zone_faceoff_spots[1].clone()),
                    "rn1" => Some(rink.red_neutral_faceoff_spots[0].clone()),
                    "rn2" => Some(rink.red_neutral_faceoff_spots[1].clone()),
                    "bn1" => Some(rink.blue_neutral_faceoff_spots[0].clone()),
                    "bn2" => Some(rink.blue_neutral_faceoff_spots[1].clone()),
                    _ => None
                };
                if let Some(faceoff_spot) = faceoff_spot {
                    self.game.next_faceoff_spot = faceoff_spot;
                    self.do_faceoff();
                }
            }
            */
            _ => {} // matches have to be exhaustive
        }
    }

    fn list_players(&mut self, player_index: usize, first_index: usize) {
        let mut found = vec![];
        for player_index in first_index..self.players.len() {
            if let Some(player) = &self.players[player_index] {
                found.push((player_index, player.player_name.clone()));
                if found.len() >= 5 {
                    break;
                }
            }
        }
        for (found_player_index, found_player_name) in found {
            self.add_directed_server_chat_message(
                format!("{}: {}", found_player_index, found_player_name),
                player_index,
            );
        }
    }

    fn search_players(&mut self, player_index: usize, name: &str) {
        let matches = self.player_search(name);
        if matches.is_empty() {
            self.add_directed_server_chat_message("No matches found".to_string(), player_index);
            return;
        }
        for (found_player_index, found_player_name) in matches.into_iter().take(5) {
            self.add_directed_server_chat_message(
                format!("{}: {}", found_player_index, found_player_name),
                player_index,
            );
        }
    }

    fn view(&mut self, view_player_index: usize, player_index: usize) {
        if view_player_index < self.players.len() {
            if let Some(view_player) = &self.players[view_player_index] {
                let view_player_name = view_player.player_name.clone();
                if let Some(player) = &mut self.players[player_index] {
                    if view_player_index != player.view_player_index {
                        player.view_player_index = view_player_index;
                        if player_index != view_player_index {
                            if set_team_internal(
                                player_index,
                                player,
                                &mut self.game.world,
                                &self.config,
                                None,
                            )
                            .is_some()
                            {
                                let msg = HQMMessage::PlayerUpdate {
                                    player_name: player.player_name.clone(),
                                    object: None,
                                    player_index,
                                    in_server: true,
                                };
                                self.add_global_message(msg, true);
                            };
                            self.add_directed_server_chat_message(
                                format!("You are now viewing {}", view_player_name),
                                player_index,
                            );
                        } else {
                            self.add_directed_server_chat_message(
                                "View has been restored".to_string(),
                                player_index,
                            );
                        }
                    }
                }
            } else {
                self.add_directed_server_chat_message(
                    "No player with this ID exists".to_string(),
                    player_index,
                );
            }
        }
    }

    fn ping(&mut self, ping_player_index: usize, player_index: usize) {
        if ping_player_index < self.players.len() {
            if let Some(ping_player) = &self.players[ping_player_index] {
                if ping_player.last_ping.is_empty() {
                    let msg = format!("No ping values found for {}", ping_player.player_name);
                    self.add_directed_server_chat_message(msg, player_index);
                } else {
                    let n = ping_player.last_ping.len() as f32;
                    let mut min = f32::INFINITY;
                    let mut max = f32::NEG_INFINITY;
                    let mut sum = 0f32;
                    for i in ping_player.last_ping.iter() {
                        min = min.min(*i);
                        max = max.max(*i);
                        sum += *i;
                    }
                    let avg = sum / n;
                    let dev = {
                        let mut s = 0f32;
                        for i in ping_player.last_ping.iter() {
                            s += (*i - avg).powi(2);
                        }
                        (s / n).sqrt()
                    };

                    let msg1 = format!(
                        "{} ping: avg {:.0} ms",
                        ping_player.player_name,
                        (avg * 1000f32)
                    );
                    let msg2 = format!(
                        "min {:.0} ms, max {:.0} ms, std.dev {:.1}",
                        (min * 1000f32),
                        (max * 1000f32),
                        (dev * 1000f32)
                    );
                    self.add_directed_server_chat_message(msg1, player_index);
                    self.add_directed_server_chat_message(msg2, player_index);
                }
            } else {
                self.add_directed_server_chat_message(
                    "No player with this ID exists".to_string(),
                    player_index,
                );
            }
        }
    }

    pub(crate) fn player_exact_unique_match(&self, name: &str) -> Option<(usize, String)> {
        let mut found = None;
        for (player_index, player) in self.players.iter().enumerate() {
            if let Some(player) = player {
                if player.player_name == name {
                    if found.is_none() {
                        found = Some((player_index, player.player_name.clone()));
                    } else {
                        return None;
                    }
                }
            }
        }
        found
    }

    pub(crate) fn player_search(&self, name: &str) -> Vec<(usize, String)> {
        let name = name.to_lowercase();
        let mut found = vec![];
        for (player_index, player) in self.players.iter().enumerate() {
            if let Some(player) = player {
                if player.player_name.to_lowercase().contains(&name) {
                    found.push((player_index, player.player_name.clone()));
                    if found.len() >= 5 {
                        break;
                    }
                }
            }
        }
        found
    }

    fn process_message(&mut self, bytes: Vec<u8>, player_index: usize) {
        let msg = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return,
        };

        if self.players[player_index].is_some() {
            if msg.starts_with("/") {
                let split: Vec<&str> = msg.splitn(2, " ").collect();
                let command = &split[0][1..];
                let arg = if split.len() < 2 { "" } else { &split[1] };
                self.process_command(command, arg, player_index);
            } else {
                if !self.is_muted {
                    match &self.players[player_index as usize] {
                        Some(player) => match player.is_muted {
                            HQMMuteStatus::NotMuted => {
                                self.add_user_chat_message(msg, player_index);
                            }
                            HQMMuteStatus::ShadowMuted => {
                                self.add_directed_user_chat_message(
                                    msg,
                                    player_index,
                                    player_index,
                                );
                            }
                            HQMMuteStatus::Muted => {}
                        },
                        _ => {
                            return;
                        }
                    }
                }
            }
        }
    }

    fn player_exit(&mut self, addr: SocketAddr) {
        let player_index = self.find_player_slot(addr);
        match player_index {
            Some(player_index) => {
                let player_name = {
                    let player = self.players[player_index].as_ref().unwrap();
                    player.player_name.clone()
                };
                self.remove_player(player_index);
                info!("{} ({}) exited server", player_name, player_index);
                let msg = format!("{} exited", player_name);
                self.add_server_chat_message(msg);

                if self.game.ranked_started {
                    let mut exist = false;
                    let mut leaved_seconds = 0;
                    let mut index = 0;
                    let mut found_index = 0;
                    for i in self.game.game_players.iter() {
                        match i {
                            RHQMGamePlayer {
                                player_i_r: _,
                                player_name_r,
                                player_points: _,
                                player_team: _,
                                goals: _,
                                assists: _,
                                leaved_seconds,
                            } => {
                                if player_name_r == &player_name {
                                    exist = true;
                                    found_index = index;
                                }
                            }
                        }
                        index += 1;
                    }

                    if exist {
                        leaved_seconds = self.game.game_players[found_index].leaved_seconds;
                    }

                    if exist {
                        let secs = leaved_seconds % 60;
                        let minutes = (leaved_seconds - secs) / 60;
                        let msg = format!("{} have {}m {}s to rejoin", player_name, minutes, secs);
                        self.add_server_chat_message(msg);
                    }
                }
            }
            None => {}
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_team(
        &mut self,
        player_index: usize,
        team: Option<HQMTeam>,
    ) -> Option<Option<(usize, HQMTeam)>> {
        match &mut self.players[player_index as usize] {
            Some(player) => {
                let res = set_team_internal(
                    player_index,
                    player,
                    &mut self.game.world,
                    &self.config,
                    team,
                );
                if let Some(object) = res {
                    let msg = HQMMessage::PlayerUpdate {
                        player_name: player.player_name.clone(),
                        object,
                        player_index,
                        in_server: true,
                    };
                    self.add_global_message(msg, true);
                }
                res
            }
            None => None,
        }
    }

    pub(crate) fn set_team_with_position(
        &mut self,
        player_index: usize,
        team: Option<HQMTeam>,
    ) -> Option<Option<(usize, HQMTeam)>> {
        match &mut self.players[player_index as usize] {
            Some(player) => {
                let res = set_team_internal_with_position(
                    player_index,
                    player,
                    &mut self.game.world,
                    &self.config,
                    team,
                    Point3::new(30.0 / 2.0, 1.5, 5.0),
                );
                if let Some(object) = res {
                    let msg = HQMMessage::PlayerUpdate {
                        player_name: player.player_name.clone(),
                        object,
                        player_index,
                        in_server: true,
                    };
                    self.add_global_message(msg, true);
                }
                res
            }
            None => None,
        }
    }

    pub(crate) fn set_team_with_position_by_point(
        &mut self,
        player_index: usize,
        team: Option<HQMTeam>,
        x: f32,
        y: f32,
        z: f32,
        rot_x: f32,
        rot_y: f32,
        rot_z: f32,
    ) -> Option<Option<(usize, HQMTeam)>> {
        match &mut self.players[player_index as usize] {
            Some(player) => {
                let res = set_team_internal_with_position_and_rotation(
                    player_index,
                    player,
                    &mut self.game.world,
                    &self.config,
                    team,
                    Point3::new(x, y, z),
                    rot_x,
                    rot_y,
                    rot_z,
                );
                if let Some(object) = res {
                    let msg = HQMMessage::PlayerUpdate {
                        player_name: player.player_name.clone(),
                        object,
                        player_index,
                        in_server: true,
                    };
                    self.add_global_message(msg, true);
                }
                res
            }
            None => None,
        }
    }

    fn add_player(&mut self, player_name: String, addr: SocketAddr) -> Option<usize> {
        let player_index = self.find_empty_player_slot();
        match player_index {
            Some(player_index) => {
                let update = HQMMessage::PlayerUpdate {
                    player_name: player_name.clone(),
                    object: None,
                    player_index,
                    in_server: true,
                };

                self.add_global_message(update, true);

                let mut messages = self.game.persistent_messages.clone();
                for welcome_msg in self.config.welcome.iter() {
                    messages.push(Rc::new(HQMMessage::Chat {
                        player_index: None,
                        message: welcome_msg.clone(),
                    }));
                }

                let new_player = HQMConnectedPlayer::new(player_index, player_name, addr, messages);

                self.players[player_index] = Some(new_player);

                Some(player_index)
            }
            _ => None,
        }
    }

    pub(crate) fn remove_player(&mut self, player_index: usize) {
        let mut admin_check: bool = false;

        match &self.players[player_index as usize] {
            Some(player) => {
                let update = HQMMessage::PlayerUpdate {
                    player_name: player.player_name.clone(),
                    object: None,
                    player_index,
                    in_server: false,
                };
                if let Some(object_index) = player.skater {
                    self.game.world.objects[object_index] = HQMGameObject::None;
                }

                if player.is_admin {
                    admin_check = true;
                }

                self.add_global_message(update, true);

                self.players[player_index as usize] = None;

                let mut logged_index = 0;
                let mut logged_selected = 999;
                for player in self.game.logged_players.iter() {
                    if player.player_i == player_index {
                        logged_selected = logged_index;
                    }
                    logged_index += 1;
                }

                if logged_selected != 999 {
                    if !self.game.ranked_started {
                        self.game.logged_players.remove(logged_selected);
                    }
                }
            }
            None => {}
        }

        if admin_check {
            let mut admin_found = false;

            for p in self.players.iter_mut() {
                if let Some(player) = p {
                    if player.is_admin {
                        admin_found = true;
                    }
                }
            }

            if !admin_found {
                self.allow_join = true;
            }
        }
    }

    fn add_user_chat_message(&mut self, message: String, sender_index: usize) {
        if let Some(player) = &self.players[sender_index] {
            info!("{} ({}): {}", &player.player_name, sender_index, &message);
            let chat = HQMMessage::Chat {
                player_index: Some(sender_index),
                message,
            };
            self.add_global_message(chat, false);
        }
    }

    pub(crate) fn add_server_chat_message(&mut self, message: String) {
        let chat = HQMMessage::Chat {
            player_index: None,
            message,
        };
        self.add_global_message(chat, false);
    }

    fn add_directed_user_chat_message2(
        &mut self,
        message: String,
        receiver_index: usize,
        sender_index: Option<usize>,
    ) {
        // This message will only be visible to a single player
        if let Some(player) = &mut self.players[receiver_index] {
            player.add_directed_user_chat_message2(message, sender_index);
        }
    }

    pub(crate) fn add_directed_user_chat_message(
        &mut self,
        message: String,
        receiver_index: usize,
        sender_index: usize,
    ) {
        self.add_directed_user_chat_message2(message, receiver_index, Some(sender_index));
    }

    pub(crate) fn add_directed_server_chat_message(
        &mut self,
        message: String,
        receiver_index: usize,
    ) {
        self.add_directed_user_chat_message2(message, receiver_index, None);
    }

    pub(crate) fn add_global_message(&mut self, message: HQMMessage, persistent: bool) {
        let rc = Rc::new(message);
        self.game.replay_messages.push(rc.clone());
        if persistent {
            self.game.persistent_messages.push(rc.clone());
        }
        for player in self.players.iter_mut() {
            match player {
                Some(player) => {
                    player.messages.push(rc.clone());
                }
                _ => (),
            }
        }
    }

    fn find_player_slot(&self, addr: SocketAddr) -> Option<usize> {
        return self.players.iter().position(|x| match x {
            Some(x) => x.addr == addr,
            None => false,
        });
    }

    fn find_empty_player_slot(&self) -> Option<usize> {
        return self.players.iter().position(|x| x.is_none());
    }

    fn send_directed_sign_up_messages(&mut self) {
        let mut indexes = vec![];
        for (player_index, player) in self.players.iter().enumerate() {
            if let Some(player) = player {
                let mut exist = false;
                for player_item in self.game.logged_players.iter() {
                    if player_item.player_name == player.player_name {
                        exist = true;
                    }
                }

                if !exist {
                    indexes.push(player_index)
                }
            }
        }

        for i in indexes.iter() {
            self.add_directed_server_chat_message(
                String::from("Sign up on https://rhqm.site"),
                i.to_owned(),
            );
            self.add_directed_server_chat_message(
                String::from("Type /l <password> or /login <password> to join ranked game"),
                i.to_owned(),
            );
        }
    }

    fn update_players_and_input(&mut self) {
        let mut red_player_count = 0usize;
        let mut blue_player_count = 0usize;
        for p in self.game.world.objects.iter() {
            if let HQMGameObject::Player(player) = p {
                if player.team == HQMTeam::Red {
                    red_player_count += 1;
                } else if player.team == HQMTeam::Blue {
                    blue_player_count += 1;
                }
            }
        }

        let mut messages = vec![];
        let mut chat_messages = vec![];
        let players = &mut self.players;
        let world = &mut self.game.world;
        for (player_index, player_option) in players.iter_mut().enumerate() {
            if let Some(player) = player_option {
                player.inactivity += 1;
                if player.inactivity > 500 {
                    if let Some(object_index) = player.skater {
                        world.objects[object_index] = HQMGameObject::None;
                    }
                    info!("{} ({}) timed out", player.player_name, player_index);
                    messages.push(HQMMessage::PlayerUpdate {
                        player_name: player.player_name.clone(),
                        object: None,
                        player_index,
                        in_server: false,
                    });
                    let chat_msg = format!("{} timed out", player.player_name);
                    chat_messages.push(chat_msg);

                    *player_option = None;

                    continue;
                }

                player.team_switch_timer = player.team_switch_timer.saturating_sub(1);
                let skater_object = player.skater.and_then(|x| match &mut world.objects[x] {
                    HQMGameObject::Player(player) => Some(player),
                    _ => None,
                });
                let change = match skater_object {
                    Some(skater_object) => {
                        if player.input.spectate() {
                            if self.game.ranked_started == false {
                                let team_player_count = match skater_object.team {
                                    HQMTeam::Red => &mut red_player_count,
                                    HQMTeam::Blue => &mut blue_player_count,
                                };

                                if !self.game.ranked_started {
                                    let res = set_team_internal(
                                        player_index,
                                        player,
                                        world,
                                        &self.config,
                                        None,
                                    );

                                    if res.is_some() {
                                        *team_player_count -= 1;
                                        player.team_switch_timer = 500;
                                    }
                                    res
                                } else {
                                    None
                                }
                            } else {
                                skater_object.input = player.input.clone();
                                None
                            }
                        } else {
                            skater_object.input = player.input.clone();
                            None
                        }
                    }
                    None => None,
                };
                if let Some(change) = change {
                    messages.push(HQMMessage::PlayerUpdate {
                        player_name: player.player_name.clone(),
                        object: change,
                        player_index,
                        in_server: true,
                    });
                }
            }
        }

        for message in messages {
            self.add_global_message(message, true);
        }
        for message in chat_messages {
            self.add_server_chat_message(message);
        }
    }

    async fn tick(&mut self, socket: &UdpSocket) {
        if self.player_count() != 0 {
            self.game.active = true;
            let packets = tokio::task::block_in_place(|| {
                self.update_players_and_input();
                let events = self.game.world.simulate_step();
                if self.config.mode == HQMServerMode::Match {
                    self.handle_events(events);
                    self.update_clock();
                    self.game.update_game_state();

                    if self.game.period > 3 && self.game.red_score == self.game.blue_score {
                        if self.game.time_break > 1700 {
                            self.game.shootout_randomized = false;

                            if self.game.shoutout_red_start {
                                if self.game.shootout_number >= 5 {
                                    let mut red_score = 0;
                                    let mut blue_score = 0;

                                    for i in self.game.shootout_red_score.iter() {
                                        if i == &String::from("+") {
                                            red_score += 1;
                                        }
                                    }

                                    for i in self.game.shootout_blue_score.iter() {
                                        if i == &String::from("+") {
                                            blue_score += 1;
                                        }
                                    }

                                    if red_score != blue_score {
                                        self.game.game_over = true;
                                    }
                                }
                            }
                        }
                        if self.game.time_break > 1500 && self.game.time_break < 1700 {
                            if self.game.shootout_randomized == false {
                                self.config.team_max = 1;
                                self.force_players_off_ice_by_system();

                                let mut red_stat = String::from("").to_owned();
                                let mut blue_stat = String::from("").to_owned();

                                let mut stat_index = 0;
                                for i in self.game.shootout_red_score.iter() {
                                    if stat_index < 5 || self.game.shootout_number == 5 {
                                        let mut pre = String::from("");
                                        if self.game.shootout_number == 5 && stat_index == 5 {
                                            pre = String::from(" I ");
                                        }
                                        if stat_index == self.game.shootout_number
                                            && self.game.shoutout_red_start
                                        {
                                            red_stat = format!(
                                                "{}{}{} ",
                                                red_stat,
                                                pre,
                                                String::from("N")
                                            );
                                        } else {
                                            red_stat = format!("{}{}{} ", red_stat, pre, i);
                                        }
                                    }
                                    stat_index += 1;
                                }

                                stat_index = 0;
                                for i in self.game.shootout_blue_score.iter() {
                                    if stat_index < 5 || self.game.shootout_number == 5 {
                                        let mut pre = String::from("");
                                        if self.game.shootout_number == 5 && stat_index == 5 {
                                            pre = String::from(" I ");
                                        }
                                        if stat_index == self.game.shootout_number
                                            && !self.game.shoutout_red_start
                                        {
                                            blue_stat = format!(
                                                "{}{}{} ",
                                                blue_stat,
                                                pre,
                                                String::from("N")
                                            );
                                        } else {
                                            blue_stat = format!("{}{}{} ", blue_stat, pre, i);
                                        }
                                    }
                                    stat_index += 1;
                                }

                                self.add_server_chat_message(format!("RED {}", red_stat));
                                self.add_server_chat_message(format!("BLU {}", blue_stat));

                                if self.game.shoutout_red_start {
                                    let mut found_index_red = 0;
                                    let mut found_index_blue = 0;

                                    let red_index = self.game.shootout_red % self.game.ranked_count;
                                    let blue_index =
                                        self.game.shootout_blue % self.game.ranked_count;

                                    let mut red_att = 0;
                                    let mut blue_gk = 1;

                                    let mut red_name = String::from("");
                                    let mut blue_name = String::from("");

                                    for i in self.game.game_players.iter() {
                                        if i.player_team == 0 {
                                            if found_index_red == red_index {
                                                red_att = i.player_i_r;
                                                red_name = i.player_name_r.to_string();
                                            }
                                            found_index_red += 1;
                                        } else {
                                            if found_index_blue == blue_index {
                                                blue_gk = i.player_i_r;
                                                blue_name = i.player_name_r.to_string();
                                            }
                                            found_index_blue += 1;
                                        }
                                    }

                                    self.add_server_chat_message(format!(
                                        "{} vs {}(GK)",
                                        red_name, blue_name,
                                    ));

                                    self.game.world.rink =
                                        HQMRink::new_red_shootout(30.0, 61.0, 8.5);

                                    self.set_team(red_att, Some(HQMTeam::Red));
                                    self.set_team(blue_gk, Some(HQMTeam::Blue));

                                    self.set_preferred_faceoff_position_by_system(red_att, "C");
                                    self.set_preferred_faceoff_position_by_system(blue_gk, "G");

                                    self.game.shoutout_red_start = false;

                                    if self.game.shootout_red == self.game.ranked_count / 2 - 1 {
                                        self.game.shootout_red = 0;
                                    } else {
                                        self.game.shootout_red += 1;
                                    }
                                } else {
                                    let mut found_index_red = 0;
                                    let mut found_index_blue = 0;

                                    let red_index = self.game.shootout_red % self.game.ranked_count;
                                    let blue_index =
                                        self.game.shootout_blue % self.game.ranked_count;

                                    let mut red_att = 0;
                                    let mut blue_gk = 1;

                                    let mut red_name = String::from("");
                                    let mut blue_name = String::from("");

                                    for i in self.game.game_players.iter() {
                                        if i.player_team == 0 {
                                            if found_index_red == red_index {
                                                red_att = i.player_i_r;
                                                red_name = i.player_name_r.to_string();
                                            }
                                            found_index_red += 1;
                                        } else {
                                            if found_index_blue == blue_index {
                                                blue_gk = i.player_i_r;
                                                blue_name = i.player_name_r.to_string();
                                            }
                                            found_index_blue += 1;
                                        }
                                    }

                                    self.add_server_chat_message(format!(
                                        "{} vs {}(GK)",
                                        blue_name, red_name,
                                    ));

                                    self.game.world.rink =
                                        HQMRink::new_blue_shootout(30.0, 61.0, 8.5);

                                    self.set_team(red_att, Some(HQMTeam::Red));
                                    self.set_team(blue_gk, Some(HQMTeam::Blue));

                                    self.set_preferred_faceoff_position_by_system(blue_gk, "C");
                                    self.set_preferred_faceoff_position_by_system(red_att, "G");

                                    self.game.shoutout_red_start = true;
                                    if self.game.shootout_blue == self.game.ranked_count / 2 - 1 {
                                        self.game.shootout_blue = 0;
                                    } else {
                                        self.game.shootout_blue += 1;
                                    }

                                    if self.game.shootout_number != 5 {
                                        self.game.shootout_number += 1;
                                    }
                                }

                                self.game.shootout_randomized = true;
                            }
                        }
                    }
                }

                get_packets(&self.game.world.objects)
            });

            let mut write_buf = vec![0u8; 4096];
            self.game
                .saved_ticks
                .truncate(self.game.saved_ticks.capacity() - 1);
            self.game.saved_ticks.push_front(HQMSavedTick {
                packets,
                time: Instant::now(),
            });

            self.game.packet = self.game.packet.wrapping_add(1);
            self.game.game_step = self.game.game_step.wrapping_add(1);

            send_updates(&self.game, &self.players, socket, &mut write_buf).await;
            if self.config.replays_enabled {
                write_replay(&mut self.game, &mut write_buf);
            }
        } else if self.game.active {
            info!("Game {} abandoned", self.game.game_id);
            self.new_game();
            self.allow_join = true;
        }
    }

    fn call_goal(&mut self, team: HQMTeam, puck: usize) {
        if self.game.period <= 3 {
            if team == HQMTeam::Red {
                self.game.red_score += 1;
            } else if team == HQMTeam::Blue {
                self.game.blue_score += 1;
            }
        }

        self.game.time_break = self.config.time_break * 100;
        self.game.is_intermission_goal = true;
        self.game.next_faceoff_spot = self.game.world.rink.center_faceoff_spot.clone();
        if self.game.period > 3 {
            self.game.time_break = self.config.time_intermission * 100;
            self.game.time = 0;

            if team == HQMTeam::Red {
                self.game.shootout_red_score[self.game.shootout_number] = String::from("+");
            } else if team == HQMTeam::Blue {
                self.game.shootout_blue_score[self.game.shootout_number] = String::from("+");
            }
        }

        if self.game.red_score + self.config.mercy_rule == self.game.blue_score {
            self.game.time_break = self.config.time_intermission * 100;
            self.game.game_over = true;
        }

        if self.game.red_score == self.game.blue_score + self.config.mercy_rule {
            self.game.time_break = self.config.time_intermission * 100;
            self.game.game_over = true;
        }

        let mut goal_scorer_index = None;
        let mut assist_index = None;

        if let HQMGameObject::Puck(this_puck) = &mut self.game.world.objects[puck] {
            for touch in this_puck.touches.iter() {
                if touch.team == team {
                    let player_index = touch.player_index;
                    if goal_scorer_index.is_none() {
                        goal_scorer_index = Some(player_index);

                        let index = self
                            .game
                            .game_players
                            .iter()
                            .position(|r| r.player_i_r == player_index)
                            .unwrap();

                        self.game.game_players[index].goals += 1;
                    } else if assist_index.is_none() && Some(player_index) != goal_scorer_index {
                        assist_index = Some(player_index);

                        let index = self
                            .game
                            .game_players
                            .iter()
                            .position(|r| r.player_i_r == player_index)
                            .unwrap();

                        self.game.game_players[index].assists += 1;
                        break;
                    }
                }
            }
        }

        let message = HQMMessage::Goal {
            team,
            goal_player_index: goal_scorer_index,
            assist_player_index: assist_index,
        };
        self.add_global_message(message, true);
    }

    fn call_offside(&mut self, team: HQMTeam, pass_origin: &Point3<f32>) {
        self.game.next_faceoff_spot = self
            .game
            .world
            .rink
            .get_offside_faceoff_spot(pass_origin, team);
        self.game.time_break = self.config.time_break * 100;
        self.game.offside_status = HQMOffsideStatus::Offside(team);
        self.add_server_chat_message(String::from("Offside"));
    }

    fn call_icing(&mut self, team: HQMTeam, pass_origin: &Point3<f32>) {
        self.game.next_faceoff_spot = self
            .game
            .world
            .rink
            .get_icing_faceoff_spot(pass_origin, team);
        self.game.time_break = self.config.time_break * 100;
        self.game.icing_status = HQMIcingStatus::Icing(team);
        self.add_server_chat_message(String::from("Icing"));
    }

    fn handle_events(&mut self, events: Vec<HQMSimulationEvent>) {
        if self.game.offside_status.is_offside()
            || self.game.icing_status.is_icing()
            || self.game.period == 0
            || self.game.time == 0
            || self.game.time_break > 0
            || self.game.paused
        {
            return;
        }
        for event in events {
            match event {
                HQMSimulationEvent::PuckEnteredNet { team, puck } => {
                    match &self.game.offside_status {
                        HQMOffsideStatus::Warning(offside_team, p, _) if *offside_team == team => {
                            let copy = p.clone();
                            self.call_offside(team, &copy);
                        }
                        HQMOffsideStatus::Offside(_) => {}
                        _ => {
                            self.call_goal(team, puck);
                        }
                    }
                }
                HQMSimulationEvent::PuckTouch { player, puck } => {
                    // Get connected player index from skater
                    if let HQMGameObject::Player(skater) = &self.game.world.objects[player] {
                        let this_connected_player_index = skater.connected_player_index;
                        let touching_team = skater.team;
                        let faceoff_position = skater.faceoff_position.clone();

                        if let HQMGameObject::Puck(puck) = &mut self.game.world.objects[puck] {
                            puck.add_touch(
                                this_connected_player_index,
                                touching_team,
                                self.game.time,
                            );

                            let other_team = match touching_team {
                                HQMTeam::Red => HQMTeam::Blue,
                                HQMTeam::Blue => HQMTeam::Red,
                            };

                            if let HQMOffsideStatus::Warning(team, p, i) = &self.game.offside_status
                            {
                                if *team == touching_team {
                                    let pass_origin = if this_connected_player_index == *i {
                                        puck.body.pos.clone()
                                    } else {
                                        p.clone()
                                    };
                                    self.call_offside(touching_team, &pass_origin);
                                }
                                continue;
                            }
                            if let HQMIcingStatus::Warning(team, p) = &self.game.icing_status {
                                if touching_team != *team {
                                    if faceoff_position == "G" {
                                        self.game.icing_status = HQMIcingStatus::No;
                                        self.add_server_chat_message(String::from(
                                            "Icing waved off",
                                        ));
                                    } else {
                                        let copy = p.clone();
                                        self.call_icing(other_team, &copy);
                                    }
                                } else {
                                    self.game.icing_status = HQMIcingStatus::No;
                                    self.add_server_chat_message(String::from("Icing waved off"));
                                }
                            } else if let HQMIcingStatus::NotTouched(_, _) = self.game.icing_status
                            {
                                self.game.icing_status = HQMIcingStatus::No;
                            }
                        }
                    }
                }
                HQMSimulationEvent::PuckEnteredOtherHalf { team, puck } => {
                    if let HQMGameObject::Puck(puck) = &self.game.world.objects[puck] {
                        if let Some(touch) = puck.touches.front() {
                            if team == touch.team && self.game.icing_status == HQMIcingStatus::No {
                                self.game.icing_status =
                                    HQMIcingStatus::NotTouched(team, touch.puck_pos.clone());
                            }
                        }
                    }
                }
                HQMSimulationEvent::PuckPassedGoalLine { team, puck: _ } => {
                    if let HQMIcingStatus::NotTouched(icing_team, p) = &self.game.icing_status {
                        if team == *icing_team {
                            match self.config.icing {
                                HQMIcingConfiguration::Touch => {
                                    self.game.icing_status =
                                        HQMIcingStatus::Warning(team, p.clone());
                                    self.add_server_chat_message(String::from("Icing warning"));
                                }
                                HQMIcingConfiguration::NoTouch => {
                                    let copy = p.clone();
                                    self.call_icing(team, &copy);
                                }
                                HQMIcingConfiguration::Off => {}
                            }
                        }
                    }
                }
                HQMSimulationEvent::PuckEnteredOffensiveZone { team, puck } => {
                    if self.game.offside_status == HQMOffsideStatus::InNeutralZone {
                        if let HQMGameObject::Puck(puck) = &self.game.world.objects[puck] {
                            if let Some(touch) = puck.touches.front() {
                                if team == touch.team
                                    && has_players_in_offensive_zone(&self.game.world, team)
                                {
                                    match self.config.offside {
                                        HQMOffsideConfiguration::Delayed => {
                                            self.game.offside_status = HQMOffsideStatus::Warning(
                                                team,
                                                touch.puck_pos.clone(),
                                                touch.player_index,
                                            );
                                            self.add_server_chat_message(String::from(
                                                "Offside warning",
                                            ));
                                        }
                                        HQMOffsideConfiguration::Immediate => {
                                            let copy = touch.puck_pos.clone();
                                            self.call_offside(team, &copy);
                                        }
                                        HQMOffsideConfiguration::Off => {
                                            self.game.offside_status =
                                                HQMOffsideStatus::InOffensiveZone(team);
                                        }
                                    }
                                } else {
                                    self.game.offside_status =
                                        HQMOffsideStatus::InOffensiveZone(team);
                                }
                            } else {
                                self.game.offside_status = HQMOffsideStatus::InOffensiveZone(team);
                            }
                        }
                    }
                }
                HQMSimulationEvent::PuckLeftOffensiveZone { team: _, puck: _ } => {
                    if let HQMOffsideStatus::Warning(_, _, _) = self.game.offside_status {
                        self.add_server_chat_message(String::from("Offside waved off"));
                    }
                    self.game.offside_status = HQMOffsideStatus::InNeutralZone;
                }
            }
        }
        if let HQMOffsideStatus::Warning(team, _, _) = self.game.offside_status {
            if !has_players_in_offensive_zone(&self.game.world, team) {
                self.game.offside_status = HQMOffsideStatus::InOffensiveZone(team);
                self.add_server_chat_message(String::from("Offside waved off"));
            }
        }
    }

    pub(crate) fn new_game(&mut self) {
        let old_game =
            std::mem::replace(&mut self.game, HQMGame::new(self.game_alloc, &self.config));
        self.game.logged_players = vec![];
        self.game.ranked_started = false;
        for i in old_game.logged_players_for_next.iter() {
            self.game.logged_players.push(i.clone());
        }
        self.game.logged_players_for_next = vec![];

        if self.config.replays_enabled && old_game.period != 0 {
            let time = old_game.start_time.format("%Y-%m-%dT%H%M%S").to_string();
            let file_name = format!("{}.{}.hrp", self.config.server_name, time);
            let replay_data = old_game.replay_data;

            let game_id = old_game.game_id;

            tokio::spawn(async move {
                if tokio::fs::create_dir_all("replays").await.is_err() {
                    return;
                };
                let path: PathBuf = ["replays", &file_name].iter().collect();

                let mut file_handle = match File::create(path).await {
                    Ok(file) => file,
                    Err(e) => {
                        println!("{:?}", e);
                        return;
                    }
                };

                let size = replay_data.len() as u32;

                let _x = file_handle.write_all(&0u32.to_le_bytes()).await;
                let _x = file_handle.write_all(&size.to_le_bytes()).await;
                let _x = file_handle.write_all(&replay_data).await;
                let _x = file_handle.sync_all().await;

                info!("Replay of game {} saved as {}", game_id, file_name);
            });
        }

        info!("New game {} started", self.game.game_id);
        self.game_alloc += 1;

        let puck_line_start =
            self.game.world.rink.width / 2.0 - 0.4 * ((self.config.warmup_pucks - 1) as f32);

        for i in 0..self.config.warmup_pucks {
            let pos = Point3::new(
                puck_line_start + 0.8 * (i as f32),
                1.5,
                self.game.world.rink.length / 2.0,
            );
            let rot = Matrix3::identity();
            self.game
                .world
                .create_puck_object(pos, rot, self.config.cylinder_puck_post_collision);
        }

        let mut messages = Vec::new();
        for (i, p) in self.players.iter_mut().enumerate() {
            if let Some(player) = p {
                player.skater = None;

                player.known_msgpos = 0;
                player.known_packet = u32::MAX;
                player.messages.clear();
                let update = HQMMessage::PlayerUpdate {
                    player_name: player.player_name.clone(),
                    object: None,
                    player_index: i,
                    in_server: true,
                };
                messages.push(update);
            }
        }
        for message in messages {
            self.add_global_message(message, true);
        }

        self.allow_ranked_join = true;
        self.game.time = self.config.time_warmup * 100;
    }

    fn get_faceoff_positions(
        players: &[Option<HQMConnectedPlayer>],
        objects: &[HQMGameObject],
        allowed_positions: &[String],
    ) -> HashMap<usize, (HQMTeam, String)> {
        let mut res = HashMap::new();

        let mut red_players = vec![];
        let mut blue_players = vec![];
        for (player_index, player) in players.iter().enumerate() {
            if let Some(player) = player {
                let team = player.skater.and_then(|i| match &objects[i] {
                    HQMGameObject::Player(skater) => Some(skater.team),
                    _ => None,
                });
                if team == Some(HQMTeam::Red) {
                    red_players.push((player_index, player.preferred_faceoff_position.as_ref()));
                } else if team == Some(HQMTeam::Blue) {
                    blue_players.push((player_index, player.preferred_faceoff_position.as_ref()));
                }
            }
        }

        fn setup_position(
            positions: &mut HashMap<usize, (HQMTeam, String)>,
            players: &[(usize, Option<&String>)],
            allowed_positions: &[String],
            team: HQMTeam,
        ) {
            let mut available_positions = Vec::from(allowed_positions);

            // First, we try to give each player its preferred position
            for (player_index, player_position) in players.iter() {
                if let Some(player_position) = player_position {
                    if let Some(x) = available_positions
                        .iter()
                        .position(|x| *x == **player_position)
                    {
                        let s = available_positions.remove(x);
                        positions.insert(*player_index, (team, s));
                    }
                }
            }
            let c = String::from("C");
            // Some players did not get their preferred positions because they didn't have one,
            // or because it was already taken
            for (player_index, player_position) in players.iter() {
                if !positions.contains_key(player_index) {
                    let s = if let Some(x) = available_positions.iter().position(|x| *x == c) {
                        // Someone needs to be C
                        let x = available_positions.remove(0);
                        (team, x)
                    } else if !available_positions.is_empty() {
                        // Give out the remaining positions
                        let x = available_positions.remove(0);
                        (team, x)
                    } else {
                        // Oh no, we're out of legal starting positions
                        if let Some(player_position) = player_position {
                            (team, (*player_position).clone())
                        } else {
                            (team, c.clone())
                        }
                    };
                    positions.insert(*player_index, s);
                }
            }
            // if available_positions.contains(&c) && !players.is_empty() {
            //     positions.insert(players[0].0, (team, c.clone()));
            // }
        }

        setup_position(&mut res, &red_players, allowed_positions, HQMTeam::Red);
        setup_position(&mut res, &blue_players, allowed_positions, HQMTeam::Blue);

        res
    }

    fn do_faceoff(&mut self) {
        let faceoff_spot = &self.game.next_faceoff_spot;

        let positions = Self::get_faceoff_positions(
            &self.players,
            &self.game.world.objects,
            &self.game.world.rink.allowed_positions,
        );

        let puck_pos = &faceoff_spot.center_position + &(1.5f32 * Vector3::y());

        self.game.world.objects = vec![HQMGameObject::None; 32];
        self.game.world.create_puck_object(
            puck_pos.clone(),
            Matrix3::identity(),
            self.config.cylinder_puck_post_collision,
        );

        let mut messages = Vec::new();

        fn setup(
            messages: &mut Vec<HQMMessage>,
            world: &mut HQMGameWorld,
            player: &mut HQMConnectedPlayer,
            player_index: usize,
            faceoff_position: String,
            pos: Point3<f32>,
            rot: Matrix3<f32>,
            team: HQMTeam,
        ) {
            let new_object_index = world.create_player_object(
                team,
                pos,
                rot,
                player.hand,
                player_index,
                faceoff_position,
                player.mass,
            );
            player.skater = new_object_index;

            let update = HQMMessage::PlayerUpdate {
                player_name: player.player_name.clone(),
                object: new_object_index.map(|x| (x, team)),
                player_index,

                in_server: true,
            };
            messages.push(update);
        }

        for (player_index, (team, faceoff_position)) in positions {
            if let Some(player) = &mut self.players[player_index] {
                let (player_position, player_rotation) = match team {
                    HQMTeam::Red => faceoff_spot.red_player_positions[&faceoff_position].clone(),
                    HQMTeam::Blue => faceoff_spot.blue_player_positions[&faceoff_position].clone(),
                };
                setup(
                    &mut messages,
                    &mut self.game.world,
                    player,
                    player_index,
                    faceoff_position,
                    player_position,
                    player_rotation.matrix().clone_owned(),
                    team,
                )
            }
        }

        let rink = &self.game.world.rink;
        self.game.icing_status = HQMIcingStatus::No;
        self.game.offside_status = if rink
            .red_lines_and_net
            .offensive_line
            .point_past_middle_of_line(&puck_pos)
        {
            HQMOffsideStatus::InOffensiveZone(HQMTeam::Red)
        } else if rink
            .blue_lines_and_net
            .offensive_line
            .point_past_middle_of_line(&puck_pos)
        {
            HQMOffsideStatus::InOffensiveZone(HQMTeam::Blue)
        } else {
            HQMOffsideStatus::InNeutralZone
        };

        for message in messages {
            self.add_global_message(message, true);
        }
    }

    fn update_clock(&mut self) {
        if !self.game.paused {
            if self.game.time_break > 0 {
                self.game.time_break -= 1;
                if self.game.time_break == 0 {
                    self.game.is_intermission_goal = false;
                    if self.game.game_over {
                        self.new_game();
                    } else {
                        if self.game.time == 0 {
                            self.game.time = self.config.time_period * 100;

                            if self.game.period > 3 {
                                self.game.time = 1500;
                            }
                        }
                        self.do_faceoff();
                    }
                }
            } else if self.game.time > 0 {
                self.game.time -= 1;

                if self.game.time % 100 == 0 {
                    let mut indexes = vec![];

                    let mut index = 0;

                    for i in self.game.game_players.iter() {
                        if i.leaved_seconds != 0 {
                            let mut ex = false;
                            for (player_index, player) in self.players.iter().enumerate() {
                                if let Some(player) = player {
                                    if player.player_name == i.player_name_r {
                                        ex = true;
                                    }
                                }
                            }

                            if !ex {
                                indexes.push(index);
                            }
                        }

                        index += 1;
                    }

                    for i in indexes.iter() {
                        self.game.game_players[i.to_owned()].leaved_seconds -= 1;
                        if self.game.game_players[i.to_owned()].leaved_seconds == 1 {
                            self.game.game_players[i.to_owned()].leaved_seconds = 0;
                            self.add_server_chat_message(format!(
                                "{} lose 30 points",
                                self.game.game_players[i.to_owned()].player_name_r
                            ));
                        }
                    }
                }
                if self.game.time == 0 {
                    if self.game.period != 4 {
                        if self.game.period != 0 || self.game.ranked_started {
                            self.game.period += 1;
                        }
                    }
                    if self.game.period > 3 && self.game.red_score != self.game.blue_score {
                        self.game.time_break = self.config.time_intermission * 100;
                        self.game.game_over = true;
                    } else {
                        self.game.time_break = self.config.time_intermission * 100;
                        self.game.next_faceoff_spot =
                            self.game.world.rink.center_faceoff_spot.clone();
                    }
                }
            }

            if self.game.period == 0 && !self.game.ranked_started {
                if self.game.logged_players.len() != 0 {
                    if self.game.time == 1 {
                        self.game.time = 4000;
                        self.game.wait_for_end = true;
                    }
                    if self.game.time == 0 {
                        if self.game.time_break == 700 {
                            self.get_next_mini_game();
                        }

                        if !self.game.last_mini_game_changed {
                            self.game.last_mini_game_changed = true;
                            self.add_server_chat_message(String::from(" "));
                            self.add_server_chat_message(String::from(
                                "Vote for next mini game /v # or /vote #",
                            ));
                            self.add_server_chat_message(String::from(
                                "1.Speed shots  2.Goalkeeper  3.Air goals",
                            ));
                            self.add_server_chat_message(String::from(
                                "4.Air puck  5.Scorer  6.Precision",
                            ));
                            self.add_server_chat_message(String::from("7.Long passes"));
                            self.game.time_break = 1300;
                            self.game.force_intermission = true;
                        }

                        if self.game.time_break == 1 {
                            self.game.time = 18000;
                            self.game.paused = false;
                            self.game.last_mini_game_changed = false;
                            self.game.force_intermission = false;
                        }
                    } else {
                        match self.game.last_mini_game {
                            0 => {
                                if self.game.mini_game_warmup > 0 {
                                    if self.game.mini_game_warmup == 499 {
                                        self.game.mini_game_time = 3000;
                                        self.new_world();
                                        self.force_players_off_ice_by_system();

                                        self.config.spawn_point = HQMSpawnPoint::Bench;
                                        self.game.next_game_player_index =
                                            self.get_random_logged_player();

                                        if self.game.next_game_player_index != 999 {
                                            self.set_team(
                                                self.game.next_game_player_index,
                                                Some(HQMTeam::Red),
                                            );

                                            for player in self.game.logged_players.iter() {
                                                if player.player_i
                                                    == self.game.next_game_player_index
                                                {
                                                    self.game.next_game_player =
                                                        player.player_name.to_owned();
                                                }
                                            }

                                            self.game.pucks_in_net = vec![];

                                            self.add_server_chat_message(format!(
                                                "Next try by {}",
                                                self.game.next_game_player
                                            ));
                                        }
                                    }
                                    if self.game.next_game_player_index != 999 {
                                        if self.game.mini_game_warmup % 100 == 0
                                            && self.game.mini_game_warmup < 400
                                        {
                                            self.add_directed_server_chat_message(
                                                format!("{}", self.game.mini_game_warmup / 100),
                                                self.game.next_game_player_index,
                                            );
                                        }
                                    }
                                    self.game.mini_game_warmup -= 1;
                                } else {
                                    if self.game.mini_game_time > 0 {
                                        if self.game.mini_game_time == 3000 {
                                            self.render_pucks(9);
                                        }

                                        let mut pucks = vec![];

                                        for object in &mut self.game.world.objects.iter() {
                                            if let HQMGameObject::Puck(puck) = object {
                                                pucks.push(puck.clone());
                                            }
                                        }

                                        for puck in pucks.iter() {
                                            let mut exist = false;
                                            for ind in self.game.pucks_in_net.iter() {
                                                if ind == &puck.index {
                                                    exist = true;
                                                }
                                            }

                                            if !exist {
                                                let result = self.check_puck_in_net(puck);
                                                if result == 1 {
                                                    self.game.world.objects[puck.index] =
                                                        HQMGameObject::None;

                                                    self.game.pucks_in_net.push(puck.index);

                                                    if self.game.pucks_in_net.len() > 6 {
                                                        self.add_server_chat_message(format!(
                                                            "Puck in net [{}/8] ({}.{})",
                                                            self.game.pucks_in_net.len(),
                                                            (3000 - self.game.mini_game_time) / 100,
                                                            (3000 - self.game.mini_game_time) % 100
                                                        ));
                                                    } else {
                                                        self.add_directed_server_chat_message(
                                                            format!(
                                                                "Puck in net [{}/8] ({}.{})",
                                                                self.game.pucks_in_net.len(),
                                                                (3000 - self.game.mini_game_time)
                                                                    / 100,
                                                                (3000 - self.game.mini_game_time)
                                                                    % 100
                                                            ),
                                                            self.game.next_game_player_index,
                                                        );
                                                    }

                                                    if self.game.pucks_in_net.len() == 8 {
                                                        let result = format!(
                                                            "{}.{}",
                                                            (3000 - self.game.mini_game_time) / 100,
                                                            (3000 - self.game.mini_game_time) % 100
                                                        );

                                                        Self::save_mini_game_result(
                                                            &self.game.next_game_player,
                                                            result,
                                                        );

                                                        self.add_server_chat_message(format!(
                                                            "Result saved"
                                                        ));

                                                        if self.game.wait_for_end {
                                                            self.game.time = 0;
                                                        }

                                                        self.game.mini_game_time = 300;
                                                    }
                                                }
                                            }
                                        }

                                        self.game.mini_game_time -= 1;
                                    } else {
                                        if self.game.wait_for_end {
                                            self.game.time = 0;
                                        }
                                        self.game.mini_game_warmup = 500;
                                    }
                                }
                            }
                            1 => {
                                if self.game.mini_game_warmup > 0 {
                                    if self.game.mini_game_warmup == 499 {
                                        self.game.mini_game_time = 30000;
                                        self.new_world();
                                        self.force_players_off_ice_by_system();

                                        self.config.spawn_point = HQMSpawnPoint::Center;
                                        self.game.next_game_player_index =
                                            self.get_random_logged_player();

                                        if self.game.next_game_player_index != 999 {
                                            self.set_team_with_position(
                                                self.game.next_game_player_index,
                                                Some(HQMTeam::Blue),
                                            );

                                            for player in self.game.logged_players.iter() {
                                                if player.player_i
                                                    == self.game.next_game_player_index
                                                {
                                                    self.game.next_game_player =
                                                        player.player_name.to_owned();
                                                }
                                            }

                                            self.game.gk_catches = 0;
                                            self.game.gk_last_height = 2;
                                            self.game.gk_last_vector = 2;
                                            self.game.gk_speed = 0.3;
                                            self.game.gk_puck_in_net = false;

                                            self.add_server_chat_message(format!(
                                                "Next try by {}",
                                                self.game.next_game_player
                                            ));
                                        }
                                    }
                                    if self.game.next_game_player_index != 999 {
                                        if self.game.mini_game_warmup % 100 == 0
                                            && self.game.mini_game_warmup < 400
                                        {
                                            self.add_directed_server_chat_message(
                                                format!("{}", self.game.mini_game_warmup / 100),
                                                self.game.next_game_player_index,
                                            );
                                        }
                                    }
                                    self.game.mini_game_warmup -= 1;
                                } else {
                                    if self.game.mini_game_time > 0 {
                                        if self.game.mini_game_time % 200 == 0
                                            && self.game.gk_puck_in_net == false
                                        {
                                            self.render_pucks(1);
                                            for object in self.game.world.objects.iter_mut() {
                                                if let HQMGameObject::Puck(puck) = object {
                                                    puck.body.pos = Point3::new(
                                                        self.game.world.rink.width / 2.0,
                                                        self.game.gk_heights
                                                            [self.game.gk_last_height],
                                                        self.game.world.rink.length / 2.0,
                                                    );

                                                    let mut x_vec = 0.0;

                                                    if self.game.gk_vectors
                                                        [self.game.gk_last_vector]
                                                        != 0.0
                                                    {
                                                        x_vec = self.game.gk_vectors
                                                            [self.game.gk_last_vector]
                                                            + (self.game.gk_speed - 0.3) / 100.0;
                                                    } else {
                                                        x_vec = self.game.gk_vectors
                                                            [self.game.gk_last_vector];
                                                    }

                                                    puck.body.linear_velocity = Vector3::new(
                                                        x_vec,
                                                        0.0,
                                                        -1.0 * self.game.gk_speed,
                                                    );

                                                    self.game.gk_last_vector =
                                                        rand::thread_rng().gen_range(0, 5);

                                                    if self.game.gk_last_vector == 5 {
                                                        self.game.gk_last_vector = 0;
                                                    }

                                                    self.game.gk_last_height += 1;

                                                    if self.game.gk_last_height == 5 {
                                                        self.game.gk_last_height = 0;
                                                    }
                                                }
                                            }

                                            if self.game.gk_catches != 0
                                                && self.game.gk_catches % 5 == 0
                                            {
                                                self.add_server_chat_message(format!(
                                                    "{} pucks caught",
                                                    self.game.gk_catches
                                                ));
                                            }

                                            self.game.gk_catches += 1;
                                            self.game.gk_speed += 0.02;
                                        }

                                        let mut pucks = vec![];

                                        for object in &mut self.game.world.objects.iter() {
                                            if let HQMGameObject::Puck(puck) = object {
                                                pucks.push(puck.clone());
                                            }
                                        }

                                        for puck in pucks.iter() {
                                            let result = self.check_puck_in_net(puck);
                                            if result == 1 {
                                                self.game.gk_puck_in_net = true;
                                                self.game.world.objects[puck.index] =
                                                    HQMGameObject::None;

                                                Self::save_gk_mini_game_result(
                                                    &self.game.next_game_player,
                                                    (self.game.gk_catches - 1).to_string(),
                                                );

                                                self.add_server_chat_message(format!(
                                                    "{} pucks caught, result saved",
                                                    (self.game.gk_catches - 1)
                                                ));

                                                if self.game.wait_for_end {
                                                    self.game.time = 0;
                                                }

                                                self.game.mini_game_time = 301;
                                            }
                                        }

                                        self.game.mini_game_time -= 1;
                                    } else {
                                        self.game.mini_game_warmup = 500;
                                    }
                                }
                            }
                            2 => {
                                if self.game.mini_game_warmup > 0 {
                                    if self.game.mini_game_warmup == 499 {
                                        self.game.mini_game_time = 30000;
                                        self.new_world();
                                        self.force_players_off_ice_by_system();
                                        self.game.world.gravity = 0.000500555;
                                        self.config.spawn_point = HQMSpawnPoint::Center;
                                        self.game.next_game_player_index =
                                            self.get_random_logged_player();

                                        if self.game.next_game_player_index != 999 {
                                            self.set_team_with_position(
                                                self.game.next_game_player_index,
                                                Some(HQMTeam::Blue),
                                            );

                                            for player in self.game.logged_players.iter() {
                                                if player.player_i
                                                    == self.game.next_game_player_index
                                                {
                                                    self.game.next_game_player =
                                                        player.player_name.to_owned();
                                                }
                                            }

                                            self.game.gk_catches = 0;
                                            self.game.gk_last_height = 3;
                                            self.game.gk_last_vector = 2;
                                            self.game.gk_speed = 0.3;
                                            self.game.gk_puck_in_net = true;

                                            self.add_server_chat_message(format!(
                                                "Next try by {}",
                                                self.game.next_game_player
                                            ));
                                        }
                                    }
                                    if self.game.next_game_player_index != 999 {
                                        if self.game.mini_game_warmup % 100 == 0
                                            && self.game.mini_game_warmup < 400
                                        {
                                            self.add_directed_server_chat_message(
                                                format!("{}", self.game.mini_game_warmup / 100),
                                                self.game.next_game_player_index,
                                            );
                                        }
                                    }
                                    self.game.mini_game_warmup -= 1;
                                } else {
                                    if self.game.mini_game_time > 0 {
                                        if self.game.mini_game_time % 400 == 0 {
                                            if self.game.gk_puck_in_net == true {
                                                self.game.gk_puck_in_net = false;
                                                self.render_pucks(1);
                                                for object in self.game.world.objects.iter_mut() {
                                                    if let HQMGameObject::Puck(puck) = object {
                                                        let mut dem = 1.6;
                                                        // if rand::thread_rng().gen_range(0, 2) == 0 {
                                                        //     dem = 1.5;
                                                        // } else {
                                                        //     dem = -1.5;
                                                        // }

                                                        puck.body.pos = Point3::new(
                                                            10.0,
                                                            4.0,
                                                            self.game.world.rink.length / 2.0,
                                                        );

                                                        let mut x_vec = 0.23 * self.game.gk_speed;

                                                        let speed = -0.8 * self.game.gk_speed;

                                                        puck.body.linear_velocity =
                                                            Vector3::new(x_vec, 0.0, speed);

                                                        self.game.gk_last_vector += 1;

                                                        if self.game.gk_last_vector == 5 {
                                                            self.game.gk_last_vector = 0;
                                                        }
                                                        self.game.world.gravity += 0.00001;
                                                    }
                                                }

                                                self.game.gk_catches += 1;
                                            } else {
                                                if self.game.gk_catches - 1 != 0 {
                                                    Self::save_catch_mini_game_result(
                                                        &self.game.next_game_player,
                                                        (self.game.gk_catches - 1).to_string(),
                                                    );

                                                    self.add_server_chat_message(format!(
                                                        "{} goals, result saved",
                                                        (self.game.gk_catches - 1)
                                                    ));
                                                }

                                                if self.game.wait_for_end {
                                                    self.game.time = 0;
                                                }

                                                self.game.mini_game_time = 301;
                                            }
                                        }

                                        let mut pucks = vec![];

                                        for object in &mut self.game.world.objects.iter() {
                                            if let HQMGameObject::Puck(puck) = object {
                                                pucks.push(puck.clone());
                                            }
                                        }

                                        for puck in pucks.iter() {
                                            let result = self.check_puck_in_net(puck);
                                            if result == 1 {
                                                self.game.gk_puck_in_net = true;
                                                if self.game.gk_catches > 1 {
                                                    self.add_server_chat_message(format!(
                                                        "{} goals",
                                                        self.game.gk_catches
                                                    ));
                                                }

                                                self.game.world.objects[puck.index] =
                                                    HQMGameObject::None;
                                            }
                                        }

                                        self.game.mini_game_time -= 1;
                                    } else {
                                        self.game.mini_game_warmup = 500;
                                    }
                                }
                            }
                            3 => {
                                if self.game.mini_game_warmup > 0 {
                                    if self.game.mini_game_warmup == 499 {
                                        self.game.mini_game_time = 30000;
                                        self.new_world();
                                        self.force_players_off_ice_by_system();
                                        self.game.world.gravity = 0.000130555;
                                        self.config.spawn_point = HQMSpawnPoint::Center;
                                        self.game.next_game_player_index =
                                            self.get_random_logged_player();

                                        if self.game.next_game_player_index != 999 {
                                            self.set_team(
                                                self.game.next_game_player_index,
                                                Some(HQMTeam::Blue),
                                            );

                                            for player in self.game.logged_players.iter() {
                                                if player.player_i
                                                    == self.game.next_game_player_index
                                                {
                                                    self.game.next_game_player =
                                                        player.player_name.to_owned();
                                                }
                                            }

                                            self.add_server_chat_message(format!(
                                                "Next try by {}",
                                                self.game.next_game_player
                                            ));
                                        }
                                    }
                                    if self.game.next_game_player_index != 999 {
                                        if self.game.mini_game_warmup % 100 == 0
                                            && self.game.mini_game_warmup < 400
                                        {
                                            self.add_directed_server_chat_message(
                                                format!("{}", self.game.mini_game_warmup / 100),
                                                self.game.next_game_player_index,
                                            );
                                        }
                                    }
                                    self.game.mini_game_warmup -= 1;
                                } else {
                                    if self.game.mini_game_time > 0 {
                                        if self.game.mini_game_time == 30000 {
                                            self.render_pucks(1);
                                            for object in self.game.world.objects.iter_mut() {
                                                if let HQMGameObject::Puck(puck) = object {
                                                    puck.body.pos = Point3::new(
                                                        self.game.world.rink.width / 2.0,
                                                        2.0,
                                                        self.game.world.rink.length / 2.0,
                                                    );
                                                }
                                            }
                                        }
                                        let mut pucks = vec![];

                                        for object in &mut self.game.world.objects.iter() {
                                            if let HQMGameObject::Puck(puck) = object {
                                                pucks.push(puck.clone());
                                            }
                                        }

                                        for puck in pucks.iter() {
                                            let result = self.check_puck_touched_ice(puck);
                                            if result == 1 {
                                                self.game.world.objects[puck.index] =
                                                    HQMGameObject::None;

                                                let result = format!(
                                                    "{}.{}",
                                                    (30000 - self.game.mini_game_time) / 100,
                                                    (30000 - self.game.mini_game_time) % 100
                                                );

                                                self.add_server_chat_message(format!(
                                                    "Puck was on air {}, result saved",
                                                    result.to_string()
                                                ));

                                                Self::save_air_mini_game_result(
                                                    &self.game.next_game_player,
                                                    result,
                                                );

                                                if self.game.wait_for_end {
                                                    self.game.time = 0;
                                                }

                                                self.game.mini_game_time = 300;
                                            }
                                        }

                                        if self.game.mini_game_time < 29500
                                            && self.game.mini_game_time % 600 == 0
                                        {
                                            for puck in pucks.iter() {
                                                let result = self.check_puck_stay(puck);
                                                if result == 1 {
                                                    self.game.world.objects[puck.index] =
                                                        HQMGameObject::None;
                                                    let result = format!(
                                                        "{}.{}",
                                                        (30000 - self.game.mini_game_time) / 100,
                                                        (30000 - self.game.mini_game_time) % 100
                                                    );
                                                    self.add_server_chat_message(format!(
                                                        "Puck was on air {}, result saved",
                                                        result.to_string()
                                                    ));
                                                    Self::save_air_mini_game_result(
                                                        &self.game.next_game_player,
                                                        result,
                                                    );
                                                    if self.game.wait_for_end {
                                                        self.game.time = 0;
                                                    }
                                                    self.game.mini_game_time = 300;
                                                }
                                            }
                                        }

                                        if self.game.world.gravity != 0.000680555 {
                                            self.game.world.gravity += 0.0000001;
                                        }

                                        if self.game.mini_game_time % 500 == 0 {
                                            let result = format!(
                                                "{}.{}",
                                                (30000 - self.game.mini_game_time) / 100,
                                                (30000 - self.game.mini_game_time) % 100
                                            );

                                            if (30000 - self.game.mini_game_time) / 100 != 0 {
                                                self.add_server_chat_message(format!(
                                                    "Puck on air {}",
                                                    result.to_string()
                                                ));
                                            }
                                        }

                                        self.game.mini_game_time -= 1;
                                    } else {
                                        self.game.mini_game_warmup = 500;
                                    }
                                }
                            }
                            4 => {
                                if self.game.mini_game_warmup > 0 {
                                    if self.game.mini_game_warmup == 499 {
                                        self.game.mini_game_time = 30000;
                                        self.new_world();
                                        self.force_players_off_ice_by_system();
                                        self.config.spawn_point = HQMSpawnPoint::Center;
                                        self.game.next_game_player_index =
                                            self.get_random_logged_player();

                                        if self.game.next_game_player_index != 999 {
                                            self.set_team_with_position_by_point(
                                                self.game.next_game_player_index,
                                                Some(HQMTeam::Blue),
                                                17.0,
                                                1.5,
                                                6.0,
                                                0.0,
                                                -3.0 * FRAC_PI_2,
                                                0.0,
                                            );

                                            for player in self.game.logged_players.iter() {
                                                if player.player_i
                                                    == self.game.next_game_player_index
                                                {
                                                    self.game.next_game_player =
                                                        player.player_name.to_owned();
                                                }
                                            }

                                            self.game.gk_catches = 0;
                                            self.game.gk_last_height = 3;
                                            self.game.gk_last_vector = 2;
                                            self.game.gk_speed = 0.3;
                                            self.game.gk_puck_in_net = true;

                                            self.add_server_chat_message(format!(
                                                "Next try by {}",
                                                self.game.next_game_player
                                            ));
                                        }
                                    }
                                    if self.game.next_game_player_index != 999 {
                                        if self.game.mini_game_warmup % 100 == 0
                                            && self.game.mini_game_warmup < 400
                                        {
                                            self.add_directed_server_chat_message(
                                                format!("{}", self.game.mini_game_warmup / 100),
                                                self.game.next_game_player_index,
                                            );
                                        }
                                    }
                                    self.game.mini_game_warmup -= 1;
                                } else {
                                    if self.game.mini_game_time > 0 {
                                        if self.game.mini_game_time % 400 == 0 {
                                            if self.game.gk_puck_in_net == true {
                                                self.game.gk_puck_in_net = false;
                                                self.render_pucks(1);
                                                for object in self.game.world.objects.iter_mut() {
                                                    if let HQMGameObject::Puck(puck) = object {
                                                        let mut dem = 0;
                                                        if rand::thread_rng().gen_range(0, 2) == 0 {
                                                            dem = rand::thread_rng()
                                                                .gen_range(0, 15)
                                                                / 10;
                                                        } else {
                                                            dem = -1
                                                                * rand::thread_rng()
                                                                    .gen_range(0, 15)
                                                                / 10;
                                                        }

                                                        puck.body.pos = Point3::new(
                                                            5.0,
                                                            1.0,
                                                            7.0 + (dem as f32),
                                                        );

                                                        let mut x_vec = 0.35 * self.game.gk_speed;

                                                        let speed = -0.8 * self.game.gk_speed;

                                                        puck.body.linear_velocity =
                                                            Vector3::new(x_vec, 0.0, 0.0);

                                                        self.game.gk_last_vector += 1;

                                                        if self.game.gk_last_vector == 5 {
                                                            self.game.gk_last_vector = 0;
                                                        }
                                                    }
                                                }

                                                self.game.gk_catches += 1;
                                                self.game.gk_speed += 0.02;
                                            } else {
                                                if self.game.gk_catches - 1 != 0 {
                                                    Self::save_scorer_mini_game_result(
                                                        &self.game.next_game_player,
                                                        (self.game.gk_catches - 1).to_string(),
                                                    );

                                                    self.add_server_chat_message(format!(
                                                        "{} goals, result saved",
                                                        (self.game.gk_catches - 1)
                                                    ));
                                                }

                                                if self.game.wait_for_end {
                                                    self.game.time = 0;
                                                }

                                                self.game.mini_game_time = 301;
                                            }
                                        }

                                        let mut pucks = vec![];

                                        for object in &mut self.game.world.objects.iter() {
                                            if let HQMGameObject::Puck(puck) = object {
                                                pucks.push(puck.clone());
                                            }
                                        }

                                        for puck in pucks.iter() {
                                            let result = self.check_puck_in_net(puck);
                                            if result == 1 {
                                                self.game.gk_puck_in_net = true;
                                                if self.game.gk_catches > 1 {
                                                    self.add_server_chat_message(format!(
                                                        "{} goals",
                                                        self.game.gk_catches
                                                    ));
                                                }

                                                self.game.world.objects[puck.index] =
                                                    HQMGameObject::None;
                                            }
                                        }

                                        self.game.mini_game_time -= 1;
                                    } else {
                                        self.game.mini_game_warmup = 500;
                                    }
                                }
                            }
                            5 => {
                                if self.game.mini_game_warmup > 0 {
                                    if self.game.mini_game_warmup == 499 {
                                        self.game.mini_game_time = 30000;
                                        self.new_world();
                                        self.force_players_off_ice_by_system();
                                        self.config.spawn_point = HQMSpawnPoint::Center;
                                        self.game.next_game_player_index =
                                            self.get_random_logged_player();

                                        if self.game.next_game_player_index != 999 {
                                            self.set_team_with_position_by_point(
                                                self.game.next_game_player_index,
                                                Some(HQMTeam::Blue),
                                                30.0 / 2.0,
                                                1.5,
                                                25.0,
                                                0.0,
                                                0.0,
                                                0.0,
                                            );

                                            for player in self.game.logged_players.iter() {
                                                if player.player_i
                                                    == self.game.next_game_player_index
                                                {
                                                    self.game.next_game_player =
                                                        player.player_name.to_owned();
                                                }
                                            }

                                            self.game.lastx =
                                                rand::thread_rng().gen_range(5, 25) as f32;
                                            self.game.lasty = 0.2;

                                            self.game.gk_catches = 0;
                                            self.game.gk_last_height = 3;
                                            self.game.gk_last_vector = 2;
                                            self.game.gk_speed = 0.3;
                                            self.game.gk_puck_in_net = true;

                                            self.add_server_chat_message(format!(
                                                "Next try by {}",
                                                self.game.next_game_player
                                            ));
                                        }
                                    }
                                    if self.game.next_game_player_index != 999 {
                                        if self.game.mini_game_warmup % 100 == 0
                                            && self.game.mini_game_warmup < 400
                                        {
                                            self.add_directed_server_chat_message(
                                                format!("{}", self.game.mini_game_warmup / 100),
                                                self.game.next_game_player_index,
                                            );
                                        }
                                    }
                                    self.game.mini_game_warmup -= 1;
                                } else {
                                    if self.game.mini_game_time > 0 {
                                        self.render_circle();
                                        if self.game.mini_game_time % 500 == 0 {
                                            if self.game.gk_puck_in_net == true {
                                                self.game.gk_puck_in_net = false;

                                                if self.game.gk_catches >= 10 {
                                                    self.render_pucks(9);
                                                } else {
                                                    self.render_pucks(26);
                                                }
                                                self.game.sent = true;
                                                for object in self.game.world.objects.iter_mut() {
                                                    if let HQMGameObject::Puck(puck) = object {
                                                        puck.body.pos.x = 30.0 / 2.0;
                                                        puck.body.pos.y = 1.5;
                                                        puck.body.pos.z = 22.0;
                                                        puck.body.angular_velocity.x = 0.0;
                                                        puck.body.angular_velocity.y = 0.0;
                                                        puck.body.angular_velocity.z = 0.0;
                                                        puck.body.linear_velocity.x = 0.0;
                                                        puck.body.linear_velocity.y = 0.0;
                                                        puck.body.linear_velocity.z = 0.0;
                                                        break;
                                                    }
                                                }

                                                if self.game.lasty < 4.0 {
                                                    self.game.lasty += 0.3;
                                                }
                                                self.game.lastx =
                                                    rand::thread_rng().gen_range(5, 25) as f32;

                                                self.game.gk_catches += 1;
                                                self.game.sent = false;
                                            } else {
                                                if self.game.gk_catches - 1 != 0 {
                                                    Self::save_precision_mini_game_result(
                                                        &self.game.next_game_player,
                                                        (self.game.gk_catches - 1).to_string(),
                                                    );

                                                    self.add_server_chat_message(format!(
                                                        "{} hits, result saved",
                                                        (self.game.gk_catches - 1)
                                                    ));
                                                }

                                                if self.game.wait_for_end {
                                                    self.game.time = 0;
                                                }

                                                self.game.mini_game_time = 301;
                                            }
                                        } else {
                                            let mut pucks = vec![];

                                            for object in &mut self.game.world.objects.iter() {
                                                if let HQMGameObject::Puck(puck) = object {
                                                    pucks.push(puck.clone());
                                                }
                                            }

                                            for puck in pucks.iter() {
                                                if !self.game.sent {
                                                    let result = self.check_puck_in_square(puck);
                                                    if result == 1 {
                                                        self.game.gk_puck_in_net = true;
                                                        self.add_server_chat_message(format!(
                                                            "{} hits",
                                                            self.game.gk_catches
                                                        ));

                                                        self.game.sent = true;
                                                    }
                                                }
                                                break;
                                            }
                                        }

                                        self.game.mini_game_time -= 1;
                                    } else {
                                        self.game.mini_game_warmup = 500;
                                    }
                                }
                            }
                            6 => {
                                if self.game.mini_game_warmup > 0 {
                                    if self.game.mini_game_warmup == 499 {
                                        self.game.mini_game_time = 30000;
                                        self.new_world();
                                        self.force_players_off_ice_by_system();
                                        self.config.spawn_point = HQMSpawnPoint::Center;
                                        self.game.next_game_player_index =
                                            self.get_random_logged_player();

                                        if self.game.next_game_player_index != 999 {
                                            self.set_team_with_position_by_point(
                                                self.game.next_game_player_index,
                                                Some(HQMTeam::Blue),
                                                30.0 / 2.0,
                                                1.5,
                                                20.0,
                                                0.0,
                                                PI,
                                                0.0,
                                            );

                                            for player in self.game.logged_players.iter() {
                                                if player.player_i
                                                    == self.game.next_game_player_index
                                                {
                                                    self.game.next_game_player =
                                                        player.player_name.to_owned();
                                                }
                                            }

                                            self.game.lastx =
                                                rand::thread_rng().gen_range(5, 25) as f32;
                                            self.game.lasty = 0.0;
                                            self.game.lastz = 35.0;

                                            self.game.gk_catches = 0;
                                            self.game.gk_last_height = 3;
                                            self.game.gk_last_vector = 2;
                                            self.game.gk_speed = 0.3;
                                            self.game.gk_puck_in_net = true;

                                            self.add_server_chat_message(format!(
                                                "Next try by {}",
                                                self.game.next_game_player
                                            ));
                                        }
                                    }
                                    if self.game.next_game_player_index != 999 {
                                        if self.game.mini_game_warmup % 100 == 0
                                            && self.game.mini_game_warmup < 400
                                        {
                                            self.add_directed_server_chat_message(
                                                format!("{}", self.game.mini_game_warmup / 100),
                                                self.game.next_game_player_index,
                                            );
                                        }
                                    }
                                    self.game.mini_game_warmup -= 1;
                                } else {
                                    if self.game.mini_game_time > 0 {
                                        self.render_pass_target();
                                        if self.game.mini_game_time % 500 == 0 {
                                            if self.game.gk_puck_in_net == true {
                                                self.game.gk_puck_in_net = false;

                                                if self.game.gk_catches >= 10 {
                                                    self.render_pucks(15);
                                                } else {
                                                    self.render_pucks(21);
                                                }
                                                self.game.sent = true;
                                                for object in self.game.world.objects.iter_mut() {
                                                    if let HQMGameObject::Puck(puck) = object {
                                                        puck.body.pos.x = 30.0 / 2.0;
                                                        puck.body.pos.y = 1.5;
                                                        puck.body.pos.z = 22.0;
                                                        puck.body.angular_velocity.x = 0.0;
                                                        puck.body.angular_velocity.y = 0.0;
                                                        puck.body.angular_velocity.z = 0.0;
                                                        puck.body.linear_velocity.x = 0.0;
                                                        puck.body.linear_velocity.y = 0.0;
                                                        puck.body.linear_velocity.z = 0.0;
                                                        break;
                                                    }
                                                }

                                                if self.game.lastz < 50.0 {
                                                    self.game.lastz += 2.0;
                                                }
                                                self.game.lastx =
                                                    rand::thread_rng().gen_range(5, 25) as f32;

                                                self.game.gk_catches += 1;
                                                self.game.sent = false;
                                            } else {
                                                if self.game.gk_catches - 1 != 0 {
                                                    Self::save_passes_mini_game_result(
                                                        &self.game.next_game_player,
                                                        (self.game.gk_catches - 1).to_string(),
                                                    );

                                                    self.add_server_chat_message(format!(
                                                        "{} passes, result saved",
                                                        (self.game.gk_catches - 1)
                                                    ));
                                                }

                                                if self.game.wait_for_end {
                                                    self.game.time = 0;
                                                }

                                                self.game.mini_game_time = 301;
                                            }
                                        } else {
                                            let mut pucks = vec![];

                                            for object in &mut self.game.world.objects.iter() {
                                                if let HQMGameObject::Puck(puck) = object {
                                                    pucks.push(puck.clone());
                                                }
                                            }

                                            for puck in pucks.iter() {
                                                if !self.game.sent {
                                                    let result =
                                                        self.check_puck_passed_in_square(puck);
                                                    if result == 1 {
                                                        self.game.gk_puck_in_net = true;
                                                        self.add_server_chat_message(format!(
                                                            "{} passes",
                                                            self.game.gk_catches
                                                        ));

                                                        self.game.sent = true;
                                                    }
                                                }
                                                break;
                                            }
                                        }

                                        self.game.mini_game_time -= 1;
                                    } else {
                                        self.game.mini_game_warmup = 500;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    if self.game.time == 18000 {
                        self.init_mini_game();
                    }
                }
            }
        }
    }

    pub fn check_puck_in_square(&mut self, puck: &HQMPuck) -> usize {
        let mut result = 0;

        if self.game.gk_catches >= 10 {
            if puck.body.pos.x > self.game.lastx - 0.5 && puck.body.pos.y < self.game.lastx + 1.5 {
                if puck.body.pos.y > self.game.lasty - 0.5
                    && puck.body.pos.y < self.game.lasty + 1.5
                {
                    if puck.body.pos.z < 10.3 && puck.body.pos.z > 9.7 {
                        result = 1;
                    }
                }
            }
        } else {
            if puck.body.pos.x > self.game.lastx - 0.5 && puck.body.pos.y < self.game.lastx + 2.5 {
                if puck.body.pos.y > self.game.lasty - 0.5
                    && puck.body.pos.y < self.game.lasty + 2.5
                {
                    if puck.body.pos.z < 10.3 && puck.body.pos.z > 9.7 {
                        result = 1;
                    }
                }
            }
        }

        return result;
    }

    pub fn check_puck_passed_in_square(&mut self, puck: &HQMPuck) -> usize {
        let mut result = 0;

        if self.game.gk_catches >= 10 {
            if puck.body.pos.x > self.game.lastx - 0.5 && puck.body.pos.y < self.game.lastx + 1.5 {
                if puck.body.pos.y <= 2.0 {
                    if puck.body.pos.z > self.game.lastz - 0.5
                        && puck.body.pos.z < self.game.lastz + 0.5
                    {
                        result = 1;
                    }
                }
            }
        } else {
            if puck.body.pos.x > self.game.lastx - 0.5 && puck.body.pos.y < self.game.lastx + 2.5 {
                if puck.body.pos.y <= 3.0 {
                    if puck.body.pos.z > self.game.lastz - 0.5
                        && puck.body.pos.z < self.game.lastz + 0.5
                    {
                        result = 1;
                    }
                }
            }
        }

        return result;
    }

    pub fn render_circle(&mut self) {
        let mut first = true;
        let mut i = 0;

        let mut z = 10.0;
        let mut xoffset = self.game.lastx;
        let mut yoffset = self.game.lasty;
        let mut circle_points = vec![
            Vector3::new(0.0, 0.0, z),
            Vector3::new(0.0, 1.0, z),
            Vector3::new(0.0, 2.0, z),
            Vector3::new(0.0, 3.0, z),
            Vector3::new(1.0, 0.0, z),
            Vector3::new(2.0, 0.0, z),
            Vector3::new(3.0, 0.0, z),
            Vector3::new(3.0, 1.0, z),
            Vector3::new(3.0, 2.0, z),
            Vector3::new(3.0, 3.0, z),
            Vector3::new(1.0, 3.0, z),
            Vector3::new(2.0, 3.0, z),
            Vector3::new(0.0, 0.5, z), //
            Vector3::new(0.0, 1.5, z), //
            Vector3::new(0.0, 2.5, z), //
            Vector3::new(3.0, 0.5, z), //
            Vector3::new(3.0, 1.5, z), //
            Vector3::new(3.0, 2.5, z), //
            Vector3::new(0.5, 0.0, z), //
            Vector3::new(1.5, 0.0, z), //
            Vector3::new(2.5, 0.0, z), //
            Vector3::new(0.5, 3.0, z), //
            Vector3::new(1.5, 3.0, z), //
            Vector3::new(2.5, 3.0, z), //
        ];

        if self.game.gk_catches >= 10 {
            circle_points = vec![
                Vector3::new(0.0, 0.0, z),
                Vector3::new(0.0, 1.0, z),
                Vector3::new(0.0, 2.0, z),
                Vector3::new(1.0, 0.0, z),
                Vector3::new(2.0, 0.0, z),
                Vector3::new(2.0, 1.0, z),
                Vector3::new(2.0, 2.0, z),
                Vector3::new(1.0, 2.0, z),
                Vector3::new(0.0, 0.5, z), //
                Vector3::new(0.0, 1.5, z), //
                Vector3::new(2.0, 0.5, z), //
                Vector3::new(2.0, 1.5, z), //
                Vector3::new(0.5, 0.0, z), //
                Vector3::new(1.5, 0.0, z), //
                Vector3::new(0.5, 2.0, z), //
                Vector3::new(1.5, 2.0, z), //
            ];
        }

        for object in self.game.world.objects.iter_mut() {
            if let HQMGameObject::Puck(puck) = object {
                if first {
                    first = false;
                } else {
                    if circle_points.len() > i {
                        let circle = circle_points[i];
                        puck.body.pos.x = circle.x + xoffset;
                        puck.body.pos.y = circle.y + yoffset;
                        puck.body.pos.z = circle.z;
                    } else {
                        puck.body.pos.x = 0.0;
                        puck.body.pos.y = 0.0;
                        puck.body.pos.z = 0.0;
                    }
                    i += 1;
                }
            }
        }
    }

    pub fn render_pass_target(&mut self) {
        let mut first = true;
        let mut i = 0;

        let z = self.game.lastz;
        let xoffset = self.game.lastx;
        let yoffset = 0.0;
        let mut circle_points = vec![
            Vector3::new(0.0, 0.0, z),
            Vector3::new(0.0, 1.0, z),
            Vector3::new(0.0, 2.0, z),
            Vector3::new(0.0, 3.0, z),
            Vector3::new(3.0, 0.0, z),
            Vector3::new(3.0, 1.0, z),
            Vector3::new(3.0, 2.0, z),
            Vector3::new(3.0, 3.0, z),
            Vector3::new(1.0, 3.0, z),
            Vector3::new(2.0, 3.0, z),
            Vector3::new(0.0, 0.5, z), //
            Vector3::new(0.0, 1.5, z), //
            Vector3::new(0.0, 2.5, z), //
            Vector3::new(3.0, 0.5, z), //
            Vector3::new(3.0, 1.5, z), //
            Vector3::new(3.0, 2.5, z), //
            Vector3::new(0.5, 3.0, z), //
            Vector3::new(1.5, 3.0, z), //
            Vector3::new(2.5, 3.0, z), //
        ];

        if self.game.gk_catches >= 10 {
            circle_points = vec![
                Vector3::new(0.0, 0.0, z),
                Vector3::new(0.0, 1.0, z),
                Vector3::new(0.0, 2.0, z),
                Vector3::new(2.0, 0.0, z),
                Vector3::new(2.0, 1.0, z),
                Vector3::new(2.0, 2.0, z),
                Vector3::new(1.0, 2.0, z),
                Vector3::new(0.0, 0.5, z), //
                Vector3::new(0.0, 1.5, z), //
                Vector3::new(2.0, 0.5, z), //
                Vector3::new(2.0, 1.5, z), //
                Vector3::new(0.5, 2.0, z), //
                Vector3::new(1.5, 2.0, z), //
            ];
        }

        for object in self.game.world.objects.iter_mut() {
            if let HQMGameObject::Puck(puck) = object {
                if first {
                    first = false;
                } else {
                    if circle_points.len() > i {
                        let circle = circle_points[i];
                        puck.body.pos.x = circle.x + xoffset;
                        puck.body.pos.y = circle.y;
                        puck.body.pos.z = circle.z;
                    } else {
                        puck.body.pos.x = 0.0;
                        puck.body.pos.y = 0.0;
                        puck.body.pos.z = 0.0;
                    }
                    i += 1;
                }
            }
        }
    }

    pub fn check_puck_in_net(&mut self, puck: &HQMPuck) -> usize {
        let mut result = 0;
        for (team, net) in vec![
            (HQMTeam::Red, &self.game.world.rink.red_lines_and_net.net),
            (HQMTeam::Blue, &self.game.world.rink.blue_lines_and_net.net),
        ] {
            if (&net.left_post - &puck.body.pos).dot(&net.normal) >= 0.0 {
                if (&net.left_post - &puck.body.prev_pos).dot(&net.normal) < 0.0 {
                    if (&net.left_post - &puck.body.pos).dot(&net.left_post_inside) < 0.0
                        && (&net.right_post - &puck.body.pos).dot(&net.right_post_inside) < 0.0
                        && puck.body.pos.y < 1.0
                    {
                        result = 1;
                    } else {
                        result = 2;
                    }
                }
            }
        }

        return result;
    }

    pub fn check_puck_touched_ice(&mut self, puck: &HQMPuck) -> usize {
        let mut result = 0;
        if puck.body.pos.y <= 0.1 {
            result = 1;
        }

        return result;
    }

    pub fn check_puck_stay(&mut self, puck: &HQMPuck) -> usize {
        let mut result = 0;
        if self.game.last_puck_point < puck.body.pos.y + 0.05
            && self.game.last_puck_point > puck.body.pos.y - 0.05
        {
            result = 1;
        }

        self.game.last_puck_point = puck.body.pos.y;

        return result;
    }

    pub async fn run(&mut self) -> std::io::Result<()> {
        // Start new game
        self.new_game();

        // Set up timers
        let mut tick_timer = tokio::time::interval(Duration::from_millis(10));

        let addr = SocketAddr::from(([0, 0, 0, 0], self.config.port));

        let socket = Arc::new(tokio::net::UdpSocket::bind(&addr).await?);
        info!(
            "Server listening at address {:?}",
            socket.local_addr().unwrap()
        );

        if self.config.public {
            let socket = socket.clone();
            tokio::spawn(async move {
                loop {
                    let master_server = get_master_server().await.ok();
                    if let Some(addr) = master_server {
                        for _ in 0..60 {
                            let msg = b"Hock\x20";
                            let res = socket.send_to(msg, addr).await;
                            if res.is_err() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    } else {
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    }
                }
            });
        }
        let (msg_sender, mut msg_receiver) = tokio::sync::mpsc::channel(256);
        {
            let socket = socket.clone();

            tokio::spawn(async move {
                loop {
                    let mut buf = BytesMut::new();
                    buf.resize(512, 0u8);

                    match socket.recv_from(&mut buf).await {
                        Ok((size, addr)) => {
                            buf.truncate(size);
                            let _ = msg_sender
                                .send(HQMServerReceivedData::GameClientPacket {
                                    addr,
                                    data: buf.freeze(),
                                })
                                .await;
                        }
                        Err(_) => {}
                    }
                }
            });
        };

        loop {
            tokio::select! {
                _ = tick_timer.tick() => {
                    self.tick(& socket).await;
                }
                x = msg_receiver.recv() => {
                    if let Some (HQMServerReceivedData::GameClientPacket {
                        addr,
                        data: msg
                    }) = x {
                        self.handle_message(addr, & socket, & msg).await;
                    }
                }
            }
        }
    }

    pub fn new(config: HQMServerConfiguration) -> Self {
        let mut player_vec = Vec::with_capacity(64);
        for _ in 0..64 {
            player_vec.push(None);
        }

        HQMServer {
            players: player_vec,
            ban_list: HashSet::new(),
            allow_join: true,
            game: HQMGame::new(1, &config),
            game_alloc: 1,
            is_muted: false,
            config,
            last_sec: 3,
            allow_ranked_join: true,
        }
    }
}

fn has_players_in_offensive_zone(world: &HQMGameWorld, team: HQMTeam) -> bool {
    let line = match team {
        HQMTeam::Red => &world.rink.red_lines_and_net.offensive_line,
        HQMTeam::Blue => &world.rink.blue_lines_and_net.offensive_line,
    };

    for object in world.objects.iter() {
        if let HQMGameObject::Player(skater) = object {
            if skater.team == team {
                let feet_pos =
                    &skater.body.pos - (&skater.body.rot * Vector3::y().scale(skater.height));
                let dot = (&feet_pos - &line.point).dot(&line.normal);
                let leading_edge = -(line.width / 2.0);
                if dot < leading_edge {
                    // Player is offside
                    return true;
                }
            }
        }
    }

    false
}

fn write_message(writer: &mut HQMMessageWriter, message: &HQMMessage) {
    match message {
        HQMMessage::Chat {
            player_index,
            message,
        } => {
            writer.write_bits(6, 2);
            writer.write_bits(
                6,
                match *player_index {
                    Some(x) => x as u32,
                    None => u32::MAX,
                },
            );
            let message_bytes = message.as_bytes();
            let size = min(63, message_bytes.len());
            writer.write_bits(6, size as u32);

            for i in 0..size {
                writer.write_bits(7, message_bytes[i] as u32);
            }
        }
        HQMMessage::Goal {
            team,
            goal_player_index,
            assist_player_index,
        } => {
            writer.write_bits(6, 1);
            writer.write_bits(2, team.get_num());
            writer.write_bits(
                6,
                match *goal_player_index {
                    Some(x) => x as u32,
                    None => u32::MAX,
                },
            );
            writer.write_bits(
                6,
                match *assist_player_index {
                    Some(x) => x as u32,
                    None => u32::MAX,
                },
            );
        }
        HQMMessage::PlayerUpdate {
            player_name,
            object,
            player_index,
            in_server,
        } => {
            writer.write_bits(6, 0);
            writer.write_bits(6, *player_index as u32);
            writer.write_bits(1, if *in_server { 1 } else { 0 });
            let (object_index, team_num) = match object {
                Some((i, team)) => (*i as u32, team.get_num()),
                None => (u32::MAX, u32::MAX),
            };
            writer.write_bits(2, team_num);
            writer.write_bits(6, object_index);

            let name_bytes = player_name.as_bytes();
            for i in 0usize..31 {
                let v = if i < name_bytes.len() {
                    name_bytes[i]
                } else {
                    0
                };
                writer.write_bits(7, v as u32);
            }
        }
    };
}

fn write_objects(
    writer: &mut HQMMessageWriter,
    game: &HQMGame,
    packets: &VecDeque<HQMSavedTick>,
    known_packet: u32,
) {
    let current_packets = &packets[0].packets;

    let old_packets = {
        let diff = if known_packet == u32::MAX {
            None
        } else {
            game.packet.checked_sub(known_packet)
        };
        if let Some(diff) = diff {
            let index = diff as usize;
            if index < packets.len() && index < 192 && index > 0 {
                Some(&packets[index].packets)
            } else {
                None
            }
        } else {
            None
        }
    };

    writer.write_u32_aligned(game.packet);
    writer.write_u32_aligned(known_packet);

    for i in 0..32 {
        let current_packet = &current_packets[i];
        let old_packet = old_packets.map(|x| &x[i]);
        match current_packet {
            HQMObjectPacket::Puck(puck) => {
                let old_puck = old_packet.and_then(|x| match x {
                    HQMObjectPacket::Puck(old_puck) => Some(old_puck),
                    _ => None,
                });
                writer.write_bits(1, 1);
                writer.write_bits(2, 1); // Puck type
                writer.write_pos(17, puck.pos.0, old_puck.map(|puck| puck.pos.0));
                writer.write_pos(17, puck.pos.1, old_puck.map(|puck| puck.pos.1));
                writer.write_pos(17, puck.pos.2, old_puck.map(|puck| puck.pos.2));
                writer.write_pos(31, puck.rot.0, old_puck.map(|puck| puck.rot.0));
                writer.write_pos(31, puck.rot.1, old_puck.map(|puck| puck.rot.1));
            }
            HQMObjectPacket::Skater(skater) => {
                let old_skater = old_packet.and_then(|x| match x {
                    HQMObjectPacket::Skater(old_skater) => Some(old_skater),
                    _ => None,
                });
                writer.write_bits(1, 1);
                writer.write_bits(2, 0); // Skater type
                writer.write_pos(17, skater.pos.0, old_skater.map(|skater| skater.pos.0));
                writer.write_pos(17, skater.pos.1, old_skater.map(|skater| skater.pos.1));
                writer.write_pos(17, skater.pos.2, old_skater.map(|skater| skater.pos.2));
                writer.write_pos(31, skater.rot.0, old_skater.map(|skater| skater.rot.0));
                writer.write_pos(31, skater.rot.1, old_skater.map(|skater| skater.rot.1));
                writer.write_pos(
                    13,
                    skater.stick_pos.0,
                    old_skater.map(|skater| skater.stick_pos.0),
                );
                writer.write_pos(
                    13,
                    skater.stick_pos.1,
                    old_skater.map(|skater| skater.stick_pos.1),
                );
                writer.write_pos(
                    13,
                    skater.stick_pos.2,
                    old_skater.map(|skater| skater.stick_pos.2),
                );
                writer.write_pos(
                    25,
                    skater.stick_rot.0,
                    old_skater.map(|skater| skater.stick_rot.0),
                );
                writer.write_pos(
                    25,
                    skater.stick_rot.1,
                    old_skater.map(|skater| skater.stick_rot.1),
                );
                writer.write_pos(
                    16,
                    skater.head_rot,
                    old_skater.map(|skater| skater.head_rot),
                );
                writer.write_pos(
                    16,
                    skater.body_rot,
                    old_skater.map(|skater| skater.body_rot),
                );
            }
            HQMObjectPacket::None => {
                writer.write_bits(1, 0);
            }
        }
    }
}

fn write_replay(game: &mut HQMGame, write_buf: &mut [u8]) {
    let mut writer = HQMMessageWriter::new(write_buf);

    writer.write_byte_aligned(5);
    writer.write_bits(
        1,
        match game.game_over {
            true => 1,
            false => 0,
        },
    );
    writer.write_bits(8, game.red_score);
    writer.write_bits(8, game.blue_score);
    writer.write_bits(16, game.time);

    writer.write_bits(
        16,
        if game.is_intermission_goal {
            game.time_break
        } else {
            0
        },
    );
    writer.write_bits(8, game.period);

    let packets = &game.saved_ticks;

    write_objects(&mut writer, game, packets, game.replay_last_packet);
    game.replay_last_packet = game.packet;

    let remaining_messages = game.replay_messages.len() - game.replay_msg_pos;

    writer.write_bits(16, remaining_messages as u32);
    writer.write_bits(16, game.replay_msg_pos as u32);

    for message in &game.replay_messages[game.replay_msg_pos..game.replay_messages.len()] {
        write_message(&mut writer, Rc::as_ref(message));
    }
    game.replay_msg_pos = game.replay_messages.len();

    let pos = writer.get_pos();

    let slice = &write_buf[0..pos + 1];

    game.replay_data.extend_from_slice(slice);
}

async fn send_updates(
    game: &HQMGame,
    players: &[Option<HQMConnectedPlayer>],
    socket: &UdpSocket,
    write_buf: &mut [u8],
) {
    let packets = &game.saved_ticks;

    let rules_state = if let HQMOffsideStatus::Offside(_) = game.offside_status {
        HQMRulesState::Offside
    } else if let HQMIcingStatus::Icing(_) = game.icing_status {
        HQMRulesState::Icing
    } else {
        let icing_warning = game.icing_status.is_warning();
        let offside_warning = game.offside_status.is_warning();
        HQMRulesState::Regular {
            offside_warning,
            icing_warning,
        }
    };

    for player in players.iter() {
        if let Some(player) = player {
            let mut writer = HQMMessageWriter::new(write_buf);

            if player.game_id != game.game_id {
                writer.write_bytes_aligned(GAME_HEADER);
                writer.write_byte_aligned(6);
                writer.write_u32_aligned(game.game_id);
            } else {
                writer.write_bytes_aligned(GAME_HEADER);
                writer.write_byte_aligned(5);
                writer.write_u32_aligned(game.game_id);
                writer.write_u32_aligned(game.game_step);
                writer.write_bits(
                    1,
                    match game.game_over {
                        true => 1,
                        false => 0,
                    },
                );
                writer.write_bits(8, game.red_score);
                writer.write_bits(8, game.blue_score);
                writer.write_bits(16, game.time);

                writer.write_bits(
                    16,
                    if game.is_intermission_goal {
                        game.time_break
                    } else {
                        0
                    },
                );
                writer.write_bits(8, game.period);
                writer.write_bits(8, player.view_player_index as u32);

                // if using a non-cryptic version, send ping
                if player.client_version > 0 {
                    writer.write_u32_aligned(player.deltatime);
                }

                // if baba's second version or above, send rules
                if player.client_version > 1 {
                    let num = match rules_state {
                        HQMRulesState::Regular {
                            offside_warning,
                            icing_warning,
                        } => {
                            let mut res = 0;
                            if offside_warning {
                                res |= 1;
                            }
                            if icing_warning {
                                res |= 2;
                            }
                            res
                        }
                        HQMRulesState::Offside => 4,
                        HQMRulesState::Icing => 8,
                    };
                    writer.write_u32_aligned(num);
                }

                write_objects(&mut writer, game, packets, player.known_packet);

                let remaining_messages = min(player.messages.len() - player.known_msgpos, 15);

                writer.write_bits(4, remaining_messages as u32);
                writer.write_bits(16, player.known_msgpos as u32);

                for message in
                    &player.messages[player.known_msgpos..player.known_msgpos + remaining_messages]
                {
                    write_message(&mut writer, Rc::as_ref(message));
                }
            }
            let bytes_written = writer.get_bytes_written();

            let slice = &write_buf[0..bytes_written];
            let _ = socket.send_to(slice, player.addr).await;
        }
    }
}

fn set_team_internal(
    player_index: usize,
    player: &mut HQMConnectedPlayer,
    world: &mut HQMGameWorld,
    config: &HQMServerConfiguration,
    team: Option<HQMTeam>,
) -> Option<Option<(usize, HQMTeam)>> {
    let current_skater =
        player
            .skater
            .and_then(|skater_index| match &mut world.objects[skater_index] {
                HQMGameObject::Player(skater) => Some((skater_index, skater)),
                _ => None,
            });
    match current_skater {
        Some((skater_index, current_skater)) => {
            match team {
                Some(team) => {
                    if current_skater.team != team {
                        current_skater.team = team;
                        info!(
                            "{} ({}) has switched to team {:?}",
                            player.player_name, player_index, team
                        );
                        Some(Some((skater_index, team)))
                    } else {
                        None
                    }
                }
                None => {
                    player.team_switch_timer = 500; // 500 ticks, 5 seconds
                    info!("{} ({}) is spectating", player.player_name, player_index);
                    world.objects[skater_index] = HQMGameObject::None;
                    player.skater = None;
                    Some(None)
                }
            }
        }
        None => match team {
            Some(team) => {
                let (pos, rot) = match config.spawn_point {
                    HQMSpawnPoint::Center => {
                        let (z, rot) = match team {
                            HQMTeam::Red => ((world.rink.length / 2.0) + 3.0, 0.0),
                            HQMTeam::Blue => ((world.rink.length / 2.0) - 3.0, PI),
                        };
                        let pos = Point3::new(world.rink.width / 2.0, 2.0, z);
                        let rot = Rotation3::from_euler_angles(0.0, rot, 0.0);
                        (pos, rot)
                    }
                    HQMSpawnPoint::Bench => {
                        let z = match team {
                            HQMTeam::Red => (world.rink.length / 2.0) + 4.0,
                            HQMTeam::Blue => (world.rink.length / 2.0) - 4.0,
                        };
                        let pos = Point3::new(0.5, 2.0, z);
                        let rot = Rotation3::from_euler_angles(0.0, 3.0 * FRAC_PI_2, 0.0);
                        (pos, rot)
                    }
                };

                if let Some(i) = world.create_player_object(
                    team,
                    pos,
                    rot.matrix().clone_owned(),
                    player.hand,
                    player_index,
                    "".to_string(),
                    player.mass,
                ) {
                    player.skater = Some(i);
                    player.view_player_index = player_index;
                    info!(
                        "{} ({}) has joined team {:?}",
                        player.player_name, player_index, team
                    );
                    Some(Some((i, team)))
                } else {
                    None
                }
            }
            None => None,
        },
    }
}

fn set_team_internal_with_position(
    player_index: usize,
    player: &mut HQMConnectedPlayer,
    world: &mut HQMGameWorld,
    config: &HQMServerConfiguration,
    team: Option<HQMTeam>,
    position: Point3<f32>,
) -> Option<Option<(usize, HQMTeam)>> {
    let current_skater =
        player
            .skater
            .and_then(|skater_index| match &mut world.objects[skater_index] {
                HQMGameObject::Player(skater) => Some((skater_index, skater)),
                _ => None,
            });
    match current_skater {
        Some((skater_index, current_skater)) => {
            match team {
                Some(team) => {
                    if current_skater.team != team {
                        current_skater.team = team;
                        info!(
                            "{} ({}) has switched to team {:?}",
                            player.player_name, player_index, team
                        );
                        Some(Some((skater_index, team)))
                    } else {
                        None
                    }
                }
                None => {
                    player.team_switch_timer = 500; // 500 ticks, 5 seconds
                    info!("{} ({}) is spectating", player.player_name, player_index);
                    world.objects[skater_index] = HQMGameObject::None;
                    player.skater = None;
                    Some(None)
                }
            }
        }
        None => match team {
            Some(team) => {
                let (pos, rot) = match config.spawn_point {
                    HQMSpawnPoint::Center => {
                        let (z, rot) = match team {
                            HQMTeam::Red => ((world.rink.length / 2.0) + 3.0, 0.0),
                            HQMTeam::Blue => ((world.rink.length / 2.0) - 3.0, PI),
                        };
                        let pos = Point3::new(world.rink.width / 2.0, 2.0, z);
                        let rot = Rotation3::from_euler_angles(0.0, rot, 0.0);
                        (pos, rot)
                    }
                    HQMSpawnPoint::Bench => {
                        let z = match team {
                            HQMTeam::Red => (world.rink.length / 2.0) + 4.0,
                            HQMTeam::Blue => (world.rink.length / 2.0) - 4.0,
                        };
                        let pos = Point3::new(0.5, 2.0, z);
                        let rot = Rotation3::from_euler_angles(0.0, 3.0 * FRAC_PI_2, 0.0);
                        (pos, rot)
                    }
                };

                if let Some(i) = world.create_player_object(
                    team,
                    position,
                    rot.matrix().clone_owned(),
                    player.hand,
                    player_index,
                    "".to_string(),
                    player.mass,
                ) {
                    player.skater = Some(i);
                    player.view_player_index = player_index;
                    info!(
                        "{} ({}) has joined team {:?}",
                        player.player_name, player_index, team
                    );
                    Some(Some((i, team)))
                } else {
                    None
                }
            }
            None => None,
        },
    }
}

fn set_team_internal_with_position_and_rotation(
    player_index: usize,
    player: &mut HQMConnectedPlayer,
    world: &mut HQMGameWorld,
    config: &HQMServerConfiguration,
    team: Option<HQMTeam>,
    position: Point3<f32>,
    rot_x: f32,
    rot_y: f32,
    rot_z: f32,
) -> Option<Option<(usize, HQMTeam)>> {
    let current_skater =
        player
            .skater
            .and_then(|skater_index| match &mut world.objects[skater_index] {
                HQMGameObject::Player(skater) => Some((skater_index, skater)),
                _ => None,
            });
    match current_skater {
        Some((skater_index, current_skater)) => {
            match team {
                Some(team) => {
                    if current_skater.team != team {
                        current_skater.team = team;
                        info!(
                            "{} ({}) has switched to team {:?}",
                            player.player_name, player_index, team
                        );
                        Some(Some((skater_index, team)))
                    } else {
                        None
                    }
                }
                None => {
                    player.team_switch_timer = 500; // 500 ticks, 5 seconds
                    info!("{} ({}) is spectating", player.player_name, player_index);
                    world.objects[skater_index] = HQMGameObject::None;
                    player.skater = None;
                    Some(None)
                }
            }
        }
        None => match team {
            Some(team) => {
                let rot = Rotation3::from_euler_angles(rot_x, rot_y, rot_z);
                if let Some(i) = world.create_player_object(
                    team,
                    position,
                    rot.matrix().clone_owned(),
                    player.hand,
                    player_index,
                    "".to_string(),
                    player.mass,
                ) {
                    player.skater = Some(i);
                    player.view_player_index = player_index;
                    info!(
                        "{} ({}) has joined team {:?}",
                        player.player_name, player_index, team
                    );
                    Some(Some((i, team)))
                } else {
                    None
                }
            }
            None => None,
        },
    }
}

fn get_packets(objects: &[HQMGameObject]) -> Vec<HQMObjectPacket> {
    let mut packets: Vec<HQMObjectPacket> = Vec::with_capacity(32);
    for i in 0usize..32 {
        let packet = match &objects[i] {
            HQMGameObject::Puck(puck) => HQMObjectPacket::Puck(puck.get_packet()),
            HQMGameObject::Player(player) => HQMObjectPacket::Skater(player.get_packet()),
            HQMGameObject::None => HQMObjectPacket::None,
        };
        packets.push(packet);
    }
    packets
}

fn get_player_name(bytes: Vec<u8>) -> Option<String> {
    let first_null = bytes.iter().position(|x| *x == 0);

    let bytes = match first_null {
        Some(x) => &bytes[0..x],
        None => &bytes[..],
    }
    .to_vec();
    return match String::from_utf8(bytes) {
        Ok(s) => {
            let s = s.trim();
            let s = if s.is_empty() { "Noname" } else { s };
            Some(String::from(s))
        }
        Err(_) => None,
    };
}

async fn get_master_server() -> Result<SocketAddr, Box<dyn Error>> {
    let s = reqwest::get("http://www.crypticsea.com/anewzero/serverinfo.php")
        .await?
        .text()
        .await?;

    let split = s.split_ascii_whitespace().collect::<Vec<&str>>();

    let addr = split.get(1).unwrap_or(&"").parse::<IpAddr>()?;
    let port = split.get(2).unwrap_or(&"").parse::<u16>()?;
    Ok(SocketAddr::new(addr, port))
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) enum HQMMuteStatus {
    NotMuted,
    ShadowMuted,
    Muted,
}

pub(crate) struct HQMConnectedPlayer {
    pub(crate) player_name: String,
    pub(crate) addr: SocketAddr,
    client_version: u8,
    pub(crate) preferred_faceoff_position: Option<String>,
    pub(crate) skater: Option<usize>,
    game_id: u32,
    input: HQMPlayerInput,
    known_packet: u32,
    known_msgpos: usize,
    chat_rep: Option<u8>,
    messages: Vec<Rc<HQMMessage>>,
    inactivity: u32,
    pub(crate) is_admin: bool,
    pub(crate) is_muted: HQMMuteStatus,
    pub(crate) team_switch_timer: u32,
    hand: HQMSkaterHand,
    pub(crate) mass: f32,
    deltatime: u32,
    last_ping: VecDeque<f32>,
    view_player_index: usize,
}

impl HQMConnectedPlayer {
    pub fn new(
        player_index: usize,
        player_name: String,
        addr: SocketAddr,
        global_messages: Vec<Rc<HQMMessage>>,
    ) -> Self {
        HQMConnectedPlayer {
            player_name,
            addr,
            client_version: 0,
            preferred_faceoff_position: None,
            skater: None,
            game_id: u32::MAX,
            known_packet: u32::MAX,
            known_msgpos: 0,
            chat_rep: None,
            messages: global_messages,
            input: HQMPlayerInput::default(),
            inactivity: 0,
            is_admin: false,
            is_muted: HQMMuteStatus::NotMuted,
            hand: HQMSkaterHand::Right,
            team_switch_timer: 0,
            // store latest deltime client sends you to respond with it
            deltatime: 0,
            last_ping: VecDeque::new(),
            view_player_index: player_index,
            mass: 1.0,
        }
    }

    fn add_directed_user_chat_message2(&mut self, message: String, sender_index: Option<usize>) {
        // This message will only be visible to a single player
        let chat = HQMMessage::Chat {
            player_index: sender_index,
            message,
        };
        self.messages.push(Rc::new(chat));
    }

    #[allow(dead_code)]
    pub(crate) fn add_directed_user_chat_message(&mut self, message: String, sender_index: usize) {
        self.add_directed_user_chat_message2(message, Some(sender_index));
    }

    #[allow(dead_code)]
    pub(crate) fn add_directed_server_chat_message(&mut self, message: String) {
        self.add_directed_user_chat_message2(message, None);
    }
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMIcingConfiguration {
    Off,
    Touch,
    NoTouch,
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMOffsideConfiguration {
    Off,
    Delayed,
    Immediate,
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMSpawnPoint {
    Center,
    Bench,
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMServerMode {
    Match,
    PermanentWarmup,
}

pub(crate) struct HQMServerConfiguration {
    pub(crate) server_name: String,
    pub(crate) port: u16,
    pub(crate) public: bool,
    pub(crate) player_max: usize,
    pub(crate) team_max: usize,
    pub(crate) force_team_size_parity: bool,
    pub(crate) welcome: Vec<String>,
    pub(crate) mode: HQMServerMode,

    pub(crate) password: String,

    pub(crate) time_period: u32,
    pub(crate) time_warmup: u32,
    pub(crate) time_break: u32,
    pub(crate) time_intermission: u32,
    pub(crate) offside: HQMOffsideConfiguration,
    pub(crate) icing: HQMIcingConfiguration,
    pub(crate) warmup_pucks: usize,
    pub(crate) mercy_rule: u32,
    pub(crate) limit_jump_speed: bool,

    pub(crate) cheats_enabled: bool,

    pub(crate) replays_enabled: bool,

    pub(crate) spawn_point: HQMSpawnPoint,
    pub(crate) cylinder_puck_post_collision: bool,
}
