extern crate crypto;

use crate::hqm_admin_commands::crypto::digest::Digest;
use crate::hqm_game::{
    HQMGameObject, HQMGameState, HQMGameWorld, HQMMessage, HQMRink, HQMTeam, RHQMGamePlayer,
    RHQMPlayer,
};
use crate::hqm_server::{
    HQMIcingConfiguration, HQMMuteStatus, HQMOffsideConfiguration, HQMServer, HQMServerMode,
    HQMSpawnPoint,
};
use crypto::md5::Md5;
use nalgebra::{Matrix3, Point3};
use postgres::{Connection, SslMode};
use rand::seq::SliceRandom;
use rand::Rng;
use std::net::SocketAddr;
use tracing::info;

impl HQMServer {
    fn admin_deny_message(&mut self, player_index: usize) {
        let msg = format!("Please log in before using that command");
        self.add_directed_server_chat_message(msg, player_index);
    }

    pub(crate) fn set_allow_join(&mut self, player_index: usize, allowed: bool) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.allow_join = allowed;

                if allowed {
                    info!("{} ({}) enabled joins", player.player_name, player_index);
                    let msg = format!("Joins enabled by {}", player.player_name);
                    self.add_server_chat_message(msg);
                } else {
                    info!("{} ({}) disabled joins", player.player_name, player_index);
                    let msg = format!("Joins disabled by {}", player.player_name);
                    self.add_server_chat_message(msg);
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn mute_player(&mut self, admin_player_index: usize, mute_player_index: usize) {
        if let Some(admin_player) = &self.players[admin_player_index] {
            if admin_player.is_admin {
                let admin_player_name = admin_player.player_name.clone();

                if mute_player_index < self.players.len() {
                    if let Some(mute_player) = &mut self.players[mute_player_index] {
                        mute_player.is_muted = HQMMuteStatus::Muted;
                        info!(
                            "{} ({}) muted {} ({})",
                            admin_player_name,
                            admin_player_index,
                            mute_player.player_name,
                            mute_player_index
                        );
                        let msg =
                            format!("{} muted by {}", mute_player.player_name, admin_player_name);
                        self.add_server_chat_message(msg);
                    }
                }
            } else {
                self.admin_deny_message(admin_player_index);
            }
        }
    }

    pub(crate) fn unmute_player(&mut self, admin_player_index: usize, mute_player_index: usize) {
        if let Some(admin_player) = &self.players[admin_player_index] {
            if admin_player.is_admin {
                let admin_player_name = admin_player.player_name.clone();

                if mute_player_index < self.players.len() {
                    if let Some(mute_player) = &mut self.players[mute_player_index] {
                        let old_status = mute_player.is_muted;
                        mute_player.is_muted = HQMMuteStatus::NotMuted;
                        info!(
                            "{} ({}) unmuted {} ({})",
                            admin_player_name,
                            admin_player_index,
                            mute_player.player_name,
                            mute_player_index
                        );
                        let msg = format!(
                            "{} unmuted by {}",
                            mute_player.player_name, admin_player_name
                        );
                        if old_status == HQMMuteStatus::Muted {
                            self.add_server_chat_message(msg);
                        } else {
                            self.add_directed_server_chat_message(msg, admin_player_index);
                        }
                    }
                }
            } else {
                self.admin_deny_message(admin_player_index);
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn shadowmute_player(
        &mut self,
        admin_player_index: usize,
        mute_player_index: usize,
    ) {
        if let Some(admin_player) = &self.players[admin_player_index] {
            if admin_player.is_admin {
                let admin_player_name = admin_player.player_name.clone();

                if mute_player_index < self.players.len() {
                    if let Some(mute_player) = &mut self.players[mute_player_index] {
                        let old_status = mute_player.is_muted;
                        mute_player.is_muted = HQMMuteStatus::ShadowMuted;
                        info!(
                            "{} ({}) shadowmuted {} ({})",
                            admin_player_name,
                            admin_player_index,
                            mute_player.player_name,
                            mute_player_index
                        );
                        let msg = format!(
                            "{} shadowmuted by {}",
                            mute_player.player_name, admin_player_name
                        );
                        if old_status == HQMMuteStatus::Muted {
                            // Fake "unmuting" message
                            let msg = format!(
                                "{} unmuted by {}",
                                mute_player.player_name, admin_player_name
                            );
                            self.add_directed_server_chat_message(msg, mute_player_index);
                        }
                        self.add_directed_server_chat_message(msg, admin_player_index);
                    }
                }
            } else {
                self.admin_deny_message(admin_player_index);
            }
        }
    }

    pub(crate) fn mute_chat(&mut self, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.is_muted = true;

                let msg = format!("Chat muted by {}", player.player_name);
                info!("{} ({}) muted chat", player.player_name, player_index);
                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn unmute_chat(&mut self, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.is_muted = false;

                let msg = format!("Chat unmuted by {}", player.player_name);
                info!("{} ({}) unmuted chat", player.player_name, player_index);

                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn force_players_off_ice_by_system(&mut self) {
        let mut i = 0;
        while i < self.players.len() {
            self.force_player_off_ice(i);
            i = i + 1;
        }
    }

    pub(crate) fn force_player_off_ice(&mut self, force_player_index: usize) {
        if force_player_index < self.players.len() {
            if let Some(force_player) = &mut self.players[force_player_index] {
                force_player.team_switch_timer = 500; // 500 ticks, 5 seconds
                if let Some(i) = force_player.skater {
                    self.game.world.objects[i] = HQMGameObject::None;
                    force_player.skater = None;
                    let force_player_name = force_player.player_name.clone();

                    self.add_global_message(
                        HQMMessage::PlayerUpdate {
                            player_name: force_player_name,
                            object: None,
                            player_index: force_player_index as usize,
                            in_server: true,
                        },
                        true,
                    );
                }
            }
        }
    }

    pub(crate) fn set_preferred_faceoff_position(
        &mut self,
        player_index: usize,
        input_position: &str,
    ) {
        let input_position = input_position.to_uppercase();
        if self
            .game
            .world
            .rink
            .allowed_positions
            .contains(&input_position)
        {
            if let Some(player) = &mut self.players[player_index] {
                info!(
                    "{} ({}) set position {}",
                    player.player_name, player_index, input_position
                );
                let msg = format!("{} position {}", player.player_name, input_position);

                player.preferred_faceoff_position = Some(input_position);
                self.add_server_chat_message(msg);
            }
        }
    }

    pub(crate) fn set_preferred_faceoff_position_by_system(
        &mut self,
        player_index: usize,
        input_position: &str,
    ) {
        let input_position = input_position.to_uppercase();
        if self
            .game
            .world
            .rink
            .allowed_positions
            .contains(&input_position)
        {
            if let Some(player) = &mut self.players[player_index] {
                info!(
                    "{} ({}) set position {}",
                    player.player_name, player_index, input_position
                );
                let msg = format!("{} position {}", player.player_name, input_position);

                player.preferred_faceoff_position = Some(input_position);
            }
        }
    }

    pub(crate) fn admin_login(&mut self, player_index: usize, password: &str) {
        if let Some(player) = &mut self.players[player_index] {
            if self.config.password == password {
                player.is_admin = true;
                info!("{} ({}) is now admin", player.player_name, player_index);
                let msg = format!("{} admin", player.player_name);
                self.add_server_chat_message(msg);
            } else {
                info!(
                    "{} ({}) tried to become admin, entered wrong password",
                    player.player_name, player_index
                );
                let msg = format!("Incorrect password");
                self.add_directed_server_chat_message(msg, player_index);
            }
        }
    }

    pub(crate) fn test() {
        let mut hasher = Md5::new();
        let pass_str = String::from("TestingBigPassword1");

        let bytes = pass_str.as_bytes();
        hasher.input(bytes);
        let mut output = [0; 16];
        hasher.result(&mut output);
        let mut result: String = "".to_string();
        for i in output.to_vec().iter() {
            let t = format!("{:X}", i);
            result = format!("{}{}", result, t);
        }
    }

    pub(crate) fn kick_all_matching(
        &mut self,
        admin_player_index: usize,
        kick_player_name: &str,
        ban_player: bool,
    ) {
        if let Some(player) = &self.players[admin_player_index] {
            if player.is_admin {
                let admin_player_name = player.player_name.clone();

                // 0 full string | 1 begins with | 2 ends with | 3 contains
                let match_mode = if kick_player_name.starts_with("%") {
                    if kick_player_name.ends_with("%") {
                        3 // %contains%
                    } else {
                        2 // %ends with
                    }
                } else if kick_player_name.ends_with("%") {
                    1 // begins with%
                } else {
                    0
                };

                // Because we allow matching using wildcards, we use vectors for multiple instances found
                let mut kick_player_list: Vec<(usize, String, SocketAddr)> = Vec::new();

                for (player_index, p) in self.players.iter_mut().enumerate() {
                    if let Some(player) = p {
                        match match_mode {
                            0 => {
                                // full string
                                if player.player_name == kick_player_name {
                                    kick_player_list.push((
                                        player_index,
                                        player.player_name.clone(),
                                        player.addr,
                                    ));
                                }
                            }
                            1 => {
                                // begins with%
                                let match_string: String = kick_player_name
                                    .chars()
                                    .take(kick_player_name.len() - 1)
                                    .collect();

                                if player.player_name.starts_with(&match_string)
                                    || player.player_name == kick_player_name
                                {
                                    kick_player_list.push((
                                        player_index,
                                        player.player_name.clone(),
                                        player.addr,
                                    ));
                                }
                            }
                            2 => {
                                // %ends with
                                let match_string: String = kick_player_name
                                    .chars()
                                    .skip(1)
                                    .take(kick_player_name.len() - 1)
                                    .collect();

                                if player.player_name.ends_with(&match_string)
                                    || player.player_name == kick_player_name
                                {
                                    kick_player_list.push((
                                        player_index,
                                        player.player_name.clone(),
                                        player.addr,
                                    ));
                                }
                            }
                            3 => {
                                // %contains%
                                let match_string: String = kick_player_name
                                    .chars()
                                    .skip(1)
                                    .take(kick_player_name.len() - 2)
                                    .collect();

                                if player.player_name.contains(&match_string)
                                    || player.player_name == kick_player_name
                                {
                                    kick_player_list.push((
                                        player_index,
                                        player.player_name.clone(),
                                        player.addr,
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !kick_player_list.is_empty() {
                    for (player_index, player_name, player_addr) in kick_player_list {
                        if player_index != admin_player_index {
                            self.remove_player(player_index);

                            if ban_player {
                                self.ban_list.insert(player_addr.ip());

                                info!(
                                    "{} ({}) banned {} ({})",
                                    admin_player_name,
                                    admin_player_index,
                                    player_name,
                                    player_index
                                );
                                let msg =
                                    format!("{} banned by {}", player_name, admin_player_name);
                                self.add_server_chat_message(msg);
                            } else {
                                info!(
                                    "{} ({}) kicked {} ({})",
                                    admin_player_name,
                                    admin_player_index,
                                    player_name,
                                    player_index
                                );
                                let msg =
                                    format!("{} kicked by {}", player_name, admin_player_name);
                                self.add_server_chat_message(msg);
                            }
                        } else {
                            if ban_player {
                                let msg = format!("You cannot ban yourself");
                                self.add_directed_server_chat_message(msg, admin_player_index);
                            } else {
                                let msg = format!("You cannot kick yourself");
                                self.add_directed_server_chat_message(msg, admin_player_index);
                            }
                        }
                    }
                } else {
                    match match_mode {
                        0 => {
                            // full string
                            let msg = format!("No player names match {}", kick_player_name);
                            self.add_directed_server_chat_message(msg, admin_player_index);
                        }
                        1 => {
                            // begins with%
                            let msg = format!("No player names begin with {}", kick_player_name);
                            self.add_directed_server_chat_message(msg, admin_player_index);
                        }
                        2 => {
                            // %ends with
                            let msg = format!("No player names end with {}", kick_player_name);
                            self.add_directed_server_chat_message(msg, admin_player_index);
                        }
                        3 => {
                            // %contains%
                            let msg = format!("No player names contain {}", kick_player_name);
                            self.add_directed_server_chat_message(msg, admin_player_index);
                        }
                        _ => {}
                    }
                }
            } else {
                self.admin_deny_message(admin_player_index);
                return;
            }
        }
    }

    pub(crate) fn kick_player(
        &mut self,
        admin_player_index: usize,
        kick_player_index: usize,
        ban_player: bool,
    ) {
        if let Some(player) = &self.players[admin_player_index] {
            if player.is_admin {
                let admin_player_name = player.player_name.clone();

                if kick_player_index != admin_player_index {
                    if kick_player_index < self.players.len() {
                        if let Some(kick_player) = &mut self.players[kick_player_index as usize] {
                            let kick_player_name = kick_player.player_name.clone();
                            let kick_ip = kick_player.addr.ip().clone();
                            self.remove_player(kick_player_index);

                            if ban_player {
                                self.ban_list.insert(kick_ip);

                                info!(
                                    "{} ({}) banned {} ({})",
                                    admin_player_name,
                                    admin_player_index,
                                    kick_player_name,
                                    kick_player_name
                                );
                                let msg =
                                    format!("{} banned by {}", kick_player_name, admin_player_name);
                                self.add_server_chat_message(msg);
                            } else {
                                info!(
                                    "{} ({}) kicked {} ({})",
                                    admin_player_name,
                                    admin_player_index,
                                    kick_player_name,
                                    kick_player_name
                                );
                                let msg =
                                    format!("{} kicked by {}", kick_player_name, admin_player_name);
                                self.add_server_chat_message(msg);
                            }
                        }
                    }
                } else {
                    if ban_player {
                        let msg = format!("You cannot ban yourself");
                        self.add_directed_server_chat_message(msg, admin_player_index);
                    } else {
                        let msg = format!("You cannot kick yourself");
                        self.add_directed_server_chat_message(msg, admin_player_index);
                    }
                }
            } else {
                self.admin_deny_message(admin_player_index);
                return;
            }
        }
    }

    pub(crate) fn clear_bans(&mut self, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.ban_list.clear();
                info!("{} ({}) cleared bans", player.player_name, player_index);

                let msg = format!("Bans cleared by {}", player.player_name);
                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_clock(
        &mut self,
        input_minutes: u32,
        input_seconds: u32,
        player_index: usize,
    ) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.game.time = (input_minutes * 60 * 100) + (input_seconds * 100);

                info!(
                    "Clock set to {}:{} by {} ({})",
                    input_minutes, input_seconds, player.player_name, player_index
                );
                let msg = format!("Clock set by {}", player.player_name);
                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_score(&mut self, input_team: HQMTeam, input_score: u32, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                match input_team {
                    HQMTeam::Red => {
                        self.game.red_score = input_score;

                        info!(
                            "{} ({}) changed red score to {}",
                            player.player_name, player_index, input_score
                        );
                        let msg = format!("Red score changed by {}", player.player_name);
                        self.add_server_chat_message(msg);
                    }
                    HQMTeam::Blue => {
                        self.game.blue_score = input_score;

                        info!(
                            "{} ({}) changed blue score to {}",
                            player.player_name, player_index, input_score
                        );
                        let msg = format!("Blue score changed by {}", player.player_name);
                        self.add_server_chat_message(msg);
                    }
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_period(&mut self, input_period: u32, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.game.period = input_period;

                info!(
                    "{} ({}) set period to {}",
                    player.player_name, player_index, input_period
                );
                let msg = format!("Period set by {}", player.player_name);
                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_mercy(&mut self, mercy: u32, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.config.mercy_rule = mercy;

                info!(
                    "{} ({}) set mercy to {}",
                    player.player_name, player_index, mercy
                );
                let msg = format!("Mercy rule set by {} to {}", player.player_name, mercy);
                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn faceoff(&mut self, player_index: usize) {
        if self.config.mode == HQMServerMode::Match && self.game.state != HQMGameState::GameOver {
            if let Some(player) = &self.players[player_index] {
                if player.is_admin {
                    self.game.time_break = 5 * 100;
                    self.game.paused = false; // Unpause if it's paused as well

                    let msg = format!("Faceoff initiated by {}", player.player_name);
                    info!(
                        "{} ({}) initiated faceoff",
                        player.player_name, player_index
                    );
                    self.add_server_chat_message(msg);
                } else {
                    self.admin_deny_message(player_index);
                }
            }
        }
    }

    pub(crate) fn reset_game(&mut self, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                info!("{} ({}) reset game", player.player_name, player_index);
                let msg = format!("Game reset by {}", player.player_name);

                self.new_game();

                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn start_game(&mut self, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                if self.config.mode == HQMServerMode::Match
                    && self.game.state == HQMGameState::Warmup
                {
                    info!("{} ({}) started game", player.player_name, player_index);
                    let msg = format!("Game started by {}", player.player_name);

                    self.game.time = 1;

                    self.add_server_chat_message(msg);
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn pause(&mut self, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.game.paused = true;
                info!("{} ({}) paused game", player.player_name, player_index);
                let msg = format!("Game paused by {}", player.player_name);
                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn unpause(&mut self, player_index: usize) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                self.game.paused = false;
                info!("{} ({}) resumed game", player.player_name, player_index);
                let msg = format!("Game resumed by {}", player.player_name);

                self.add_server_chat_message(msg);
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_icing_rule(&mut self, player_index: usize, rule: &str) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                match rule {
                    "on" | "touch" => {
                        self.config.icing = HQMIcingConfiguration::Touch;
                        info!(
                            "{} ({}) enabled touch icing",
                            player.player_name, player_index
                        );
                        let msg = format!("Touch icing enabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    "notouch" => {
                        self.config.icing = HQMIcingConfiguration::NoTouch;
                        info!(
                            "{} ({}) enabled no-touch icing",
                            player.player_name, player_index
                        );
                        let msg = format!("No-touch icing enabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    "off" => {
                        self.config.icing = HQMIcingConfiguration::Off;
                        info!("{} ({}) disabled icing", player.player_name, player_index);
                        let msg = format!("Icing disabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    _ => {}
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_offside_rule(&mut self, player_index: usize, rule: &str) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                match rule {
                    "on" | "delayed" => {
                        self.config.offside = HQMOffsideConfiguration::Delayed;
                        info!("{} ({}) enabled offside", player.player_name, player_index);
                        let msg = format!("Offside enabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    "imm" | "immediate" => {
                        self.config.offside = HQMOffsideConfiguration::Immediate;
                        info!(
                            "{} ({}) enabled immediate offside",
                            player.player_name, player_index
                        );
                        let msg = format!("Immediate offside enabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    "off" => {
                        self.config.offside = HQMOffsideConfiguration::Off;
                        info!("{} ({}) disabled offside", player.player_name, player_index);
                        let msg = format!("Offside disabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    _ => {}
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_team_size(&mut self, player_index: usize, size: &str) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                if let Ok(new_num) = size.parse::<usize>() {
                    if new_num > 0 && new_num <= 15 {
                        self.config.team_max = new_num;

                        info!(
                            "{} ({}) set team size to {}",
                            player.player_name, player_index, new_num
                        );
                        let msg = format!("Team size set to {} by {}", new_num, player.player_name);

                        self.add_server_chat_message(msg);
                    }
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_replay(&mut self, player_index: usize, rule: &str) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                match rule {
                    "on" => {
                        self.config.replays_enabled = true;
                        if self.game.replay_data.len() < 64 * 1024 * 1024 {
                            self.game
                                .replay_data
                                .reserve((64 * 1024 * 1024) - self.game.replay_data.len())
                        }

                        info!("{} ({}) enabled replays", player.player_name, player_index);
                        let msg = format!("Replays enabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    "off" => {
                        self.config.replays_enabled = false;

                        info!("{} ({}) disabled replays", player.player_name, player_index);
                        let msg = format!("Replays disabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    _ => {}
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn set_team_parity(&mut self, player_index: usize, rule: &str) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                match rule {
                    "on" => {
                        self.config.force_team_size_parity = true;

                        info!(
                            "{} ({}) enabled team size parity",
                            player.player_name, player_index
                        );
                        let msg = format!("Team size parity enabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    "off" => {
                        self.config.force_team_size_parity = false;

                        info!(
                            "{} ({}) disabled team size parity",
                            player.player_name, player_index
                        );
                        let msg = format!("Team size parity disabled by {}", player.player_name);

                        self.add_server_chat_message(msg);
                    }
                    _ => {}
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    fn cheat_gravity(&mut self, split: &[&str]) {
        if split.len() >= 2 {
            let gravity = split[1].parse::<f32>();
            if let Ok(gravity) = gravity {
                self.game.world.gravity = gravity / 10000.0;
            }
        }
    }

    fn cheat_mass(&mut self, split: &[&str]) {
        if split.len() >= 3 {
            let player = split[1]
                .parse::<usize>()
                .ok()
                .and_then(|x| self.players.get_mut(x).and_then(|x| x.as_mut()));
            let mass = split[2].parse::<f32>();
            if let Some(player) = player {
                if let Ok(mass) = mass {
                    player.mass = mass;
                    if let Some(skater_obj_index) = player.skater {
                        if let HQMGameObject::Player(skater) =
                            &mut self.game.world.objects[skater_obj_index]
                        {
                            for collision_ball in skater.collision_balls.iter_mut() {
                                collision_ball.mass = mass;
                            }
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn cheat(&mut self, player_index: usize, arg: &str) {
        if let Some(player) = &self.players[player_index] {
            if player.is_admin {
                let split: Vec<&str> = arg.split_whitespace().collect();
                if let Some(&command) = split.get(0) {
                    match command {
                        "mass" => {
                            self.cheat_mass(&split);
                        }
                        "gravity" => {
                            self.cheat_gravity(&split);
                        }
                        _ => {}
                    }
                }
            } else {
                self.admin_deny_message(player_index);
            }
        }
    }

    pub(crate) fn user_logged_in(&mut self, user: &str, next: bool) {
        if next == false {
            let msg = format!(
                "{} logged in [{}]",
                user,
                self.game.logged_players.len().to_string()
            );

            self.add_server_chat_message(msg);
            if self.game.logged_players.len().to_string() == self.game.ranked_count.to_string() {
                self.game.ranked_started = true;
                self.game.time = 2000;
                self.game.paused = false;
                self.game.world.gravity = 0.000680555;
                let sum = self.randomize_players();
                self.force_players_off_ice_by_system();
                self.set_teams_by_server(sum);
            } else {
                if self.game.logged_players.len() == 1 {
                    self.game.time = 0;
                    self.game.paused = false;
                }
            }
        } else {
            let msg = format!(
                "{} logged in for next game [{}]",
                user,
                self.game.logged_players_for_next.len().to_string()
            );

            self.add_server_chat_message(msg);
        }
    }

    pub(crate) fn set_teams_by_server(&mut self, sum: usize) {
        let mut sum_red = 0;
        let mut sum_blue = 0;
        let half_sum = sum / 2;
        let mut red_team: Vec<usize> = vec![];
        let mut blue_team: Vec<usize> = vec![];

        let mut red_count = 0;
        let mut blue_count = 0;

        self.game
            .game_players
            .sort_by(|a, b| b.player_points.cmp(&a.player_points));

        for i in self.game.game_players.iter() {
            match i {
                RHQMGamePlayer {
                    player_i_r,
                    player_name_r: _,
                    player_points,
                    player_team: _,
                    goals: _,
                    assists: _,
                    leaved_seconds: _,
                } => {
                    if red_count == self.game.ranked_count / 2 {
                        blue_team.push(player_i_r.to_owned());
                        sum_blue = sum_blue + player_points;
                        blue_count += 1;
                    } else if blue_count == self.game.ranked_count / 2 {
                        red_team.push(player_i_r.to_owned());
                        sum_red = sum_red + player_points;
                        red_count += 1;
                    } else if sum_red <= sum_blue || sum_blue >= half_sum {
                        red_team.push(player_i_r.to_owned());
                        sum_red = sum_red + player_points;
                        red_count += 1;
                    } else {
                        blue_team.push(player_i_r.to_owned());
                        sum_blue = sum_blue + player_points;
                        blue_count += 1;
                    }
                }
            }
        }

        for i in red_team.iter() {
            let index = self
                .game
                .game_players
                .iter()
                .position(|r| r.player_i_r == i.to_owned())
                .unwrap();
            self.game.game_players[index].player_team = 0;

            self.set_team(i.to_owned(), Some(HQMTeam::Red));
        }

        for i in blue_team.iter() {
            let index = self
                .game
                .game_players
                .iter()
                .position(|r| r.player_i_r == i.to_owned())
                .unwrap();
            self.game.game_players[index].player_team = 1;
            self.set_team(i.to_owned(), Some(HQMTeam::Blue));
        }

        let msg2 = format!("{} {}", sum_red, sum_blue);
        self.add_server_chat_message(msg2);
    }

    pub(crate) fn randomize_players(&mut self) -> usize {
        self.add_server_chat_message(String::from("Ranked game starting"));
        let mut sum = 0;

        for player in self.game.logged_players.iter() {
            let points = Self::get_player_points(player.player_name.to_string());
            let player_item = RHQMGamePlayer {
                player_i_r: player.player_i,
                player_name_r: player.player_name.to_string(),
                player_points: points,
                player_team: 0,
                goals: 0,
                assists: 0,
                leaved_seconds: 120,
            };

            sum = sum + points;

            self.game.game_players.push(player_item);
        }

        return sum;
    }

    pub fn get_player_points(login: String) -> usize {
        let conn = Self::get_connection();
        let mut score: i64 = 0;
        let str_sql = format!(
            "select COALESCE(sum(\"Score\"),0) from public.\"GameStats\" where \"GameId\" in (select \"Id\" from public.\"Stats\" where \"Season\"=(select max(\"Season\") from public.\"Stats\"))
            and \"Player\" = (select \"Id\" from public.\"Users\" where \"Login\"='{}')",
            login
        );
        let str_t = &str_sql;
        let stmt = conn.prepare(str_t).unwrap();
        for row in stmt.query(&[]).unwrap() {
            score = row.get(0);
        }

        return score as usize;
    }

    pub fn save_mini_game_result(name: &String, result: String) {
        let conn = Self::get_connection();

        let str_sql = format!(
            "insert into public.\"MiniGamesStats\" values((select CASE WHEN max(\"Id\") IS NULL THEN 1 ELSE max(\"Id\")+1 END from public.\"MiniGamesStats\"),(select \"Id\" from public.\"Users\" where \"Login\"='{}'),NOW(), {})",
            name,
            result
        );

        conn.execute(&str_sql, &[]).unwrap();
    }

    pub fn save_air_mini_game_result(name: &String, result: String) {
        let conn = Self::get_connection();

        let str_sql = format!(
            "insert into public.\"AirMiniGamesStats\" values((select CASE WHEN max(\"Id\") IS NULL THEN 1 ELSE max(\"Id\")+1 END from public.\"AirMiniGamesStats\"),(select \"Id\" from public.\"Users\" where \"Login\"='{}'),NOW(), {})",
            name,
            result
        );

        conn.execute(&str_sql, &[]).unwrap();
    }

    pub fn save_gk_mini_game_result(name: &String, result: String) {
        let conn = Self::get_connection();

        let str_sql = format!(
            "insert into public.\"GkMiniGameStats\" values((select CASE WHEN max(\"Id\") IS NULL THEN 1 ELSE max(\"Id\")+1 END from public.\"GkMiniGameStats\"),(select \"Id\" from public.\"Users\" where \"Login\"='{}'),NOW(), {})",
            name,
            result
        );

        conn.execute(&str_sql, &[]).unwrap();
    }

    pub fn save_catch_mini_game_result(name: &String, result: String) {
        let conn = Self::get_connection();

        let str_sql = format!(
            "insert into public.\"CatchMiniGameStats\" values((select CASE WHEN max(\"Id\") IS NULL THEN 1 ELSE max(\"Id\")+1 END from public.\"CatchMiniGameStats\"),(select \"Id\" from public.\"Users\" where \"Login\"='{}'),NOW(), {})",
            name,
            result
        );

        conn.execute(&str_sql, &[]).unwrap();
    }

    pub fn save_scorer_mini_game_result(name: &String, result: String) {
        let conn = Self::get_connection();

        let str_sql = format!(
            "insert into public.\"ScorerMiniGame\" values((select CASE WHEN max(\"Id\") IS NULL THEN 1 ELSE max(\"Id\")+1 END from public.\"ScorerMiniGame\"),(select \"Id\" from public.\"Users\" where \"Login\"='{}'),NOW(), {})",
            name,
            result
        );

        conn.execute(&str_sql, &[]).unwrap();
    }

    pub fn save_precision_mini_game_result(name: &String, result: String) {
        let conn = Self::get_connection();

        let str_sql = format!(
            "insert into public.\"PrecisionMiniGame\" values((select CASE WHEN max(\"Id\") IS NULL THEN 1 ELSE max(\"Id\")+1 END from public.\"PrecisionMiniGame\"),(select \"Id\" from public.\"Users\" where \"Login\"='{}'),NOW(), {})",
            name,
            result
        );

        conn.execute(&str_sql, &[]).unwrap();
    }

    pub fn save_passes_mini_game_result(name: &String, result: String) {
        let conn = Self::get_connection();

        let str_sql = format!(
            "insert into public.\"PassesMiniGame\" values((select CASE WHEN max(\"Id\") IS NULL THEN 1 ELSE max(\"Id\")+1 END from public.\"PassesMiniGame\"),(select \"Id\" from public.\"Users\" where \"Login\"='{}'),NOW(), {})",
            name,
            result
        );

        conn.execute(&str_sql, &[]).unwrap();
    }

    pub(crate) fn afk(&mut self, player_index: usize) {
        let mut exist = false;
        let mut index = 0;
        let mut found_index = 0;
        for player in self.game.logged_players.iter_mut() {
            if player.player_i == player_index {
                exist = true;
                found_index = index;
            }
            index += 1;
        }

        if exist {
            self.game.logged_players[found_index].afk = true;
            self.add_directed_server_chat_message(
                String::from("You are AFK, type /here if you want to play mini-games"),
                player_index,
            );
        } else {
            self.add_directed_server_chat_message(
                String::from("You are not logged in"),
                player_index,
            );
        }
    }

    pub(crate) fn here(&mut self, player_index: usize) {
        let mut exist = false;
        let mut index = 0;
        let mut found_index = 0;
        for player in self.game.logged_players.iter_mut() {
            if player.player_i == player_index {
                exist = true;
                found_index = index;
            }
            index += 1;
        }

        if exist {
            self.game.logged_players[found_index].afk = false;
            self.add_directed_server_chat_message(String::from("You are not AFK"), player_index);
        } else {
            self.add_directed_server_chat_message(
                String::from("You are not logged in"),
                player_index,
            );
        }
    }

    pub fn get_mini_game_best_result() -> String {
        let conn = Self::get_connection();

        let str_sql = format!(
            "SELECT CONCAT(u.\"Login\",' (', m.\"Value\", ')') FROM public.\"MiniGamesStats\" m, public.\"Users\" u where m.\"Player\" = u.\"Id\" order by m.\"Value\"limit 1"
        );

        let mut player = String::from("");

        let str_t = &str_sql;
        let stmt = conn.prepare(str_t).unwrap();
        for row in stmt.query(&[]).unwrap() {
            player = row.get(0);
        }

        return player;
    }

    pub fn get_gk_mini_game_best_result() -> String {
        let conn = Self::get_connection();

        let str_sql = format!(
            "SELECT CONCAT(u.\"Login\",' (', m.\"Value\", ')') FROM public.\"GkMiniGameStats\" m, public.\"Users\" u where m.\"Player\" = u.\"Id\" order by m.\"Value\" desc limit 1"
        );

        let mut player = String::from("");

        let str_t = &str_sql;
        let stmt = conn.prepare(str_t).unwrap();
        for row in stmt.query(&[]).unwrap() {
            player = row.get(0);
        }

        return player;
    }

    pub fn get_catch_mini_game_best_result() -> String {
        let conn = Self::get_connection();

        let str_sql = format!(
            "SELECT CONCAT(u.\"Login\",' (', m.\"Value\", ')') FROM public.\"CatchMiniGameStats\" m, public.\"Users\" u where m.\"Player\" = u.\"Id\" order by m.\"Value\" desc limit 1"
        );

        let mut player = String::from("");

        let str_t = &str_sql;
        let stmt = conn.prepare(str_t).unwrap();
        for row in stmt.query(&[]).unwrap() {
            player = row.get(0);
        }

        return player;
    }

    pub fn get_air_mini_game_best_result() -> String {
        let conn = Self::get_connection();

        let str_sql = format!(
            "SELECT CONCAT(u.\"Login\",' (', m.\"Value\", ')') FROM public.\"AirMiniGamesStats\" m, public.\"Users\" u where m.\"Player\" = u.\"Id\" order by m.\"Value\" desc limit 1"
        );

        let mut player = String::from("");

        let str_t = &str_sql;
        let stmt = conn.prepare(str_t).unwrap();
        for row in stmt.query(&[]).unwrap() {
            player = row.get(0);
        }

        return player;
    }

    pub fn get_scorer_mini_game_best_result() -> String {
        let conn = Self::get_connection();

        let str_sql = format!(
            "SELECT CONCAT(u.\"Login\",' (', m.\"Value\", ')') FROM public.\"ScorerMiniGame\" m, public.\"Users\" u where m.\"Player\" = u.\"Id\" order by m.\"Value\" desc limit 1"
        );

        let mut player = String::from("");

        let str_t = &str_sql;
        let stmt = conn.prepare(str_t).unwrap();
        for row in stmt.query(&[]).unwrap() {
            player = row.get(0);
        }

        return player;
    }

    pub fn get_precision_mini_game_best_result() -> String {
        let conn = Self::get_connection();

        let str_sql = format!(
            "SELECT CONCAT(u.\"Login\",' (', m.\"Value\", ')') FROM public.\"PrecisionMiniGame\" m, public.\"Users\" u where m.\"Player\" = u.\"Id\" order by m.\"Value\" desc limit 1"
        );

        let mut player = String::from("");

        let str_t = &str_sql;
        let stmt = conn.prepare(str_t).unwrap();
        for row in stmt.query(&[]).unwrap() {
            player = row.get(0);
        }

        return player;
    }

    pub fn get_passes_mini_game_best_result() -> String {
        let conn = Self::get_connection();

        let str_sql = format!(
            "SELECT CONCAT(u.\"Login\",' (', m.\"Value\", ')') FROM public.\"PassesMiniGame\" m, public.\"Users\" u where m.\"Player\" = u.\"Id\" order by m.\"Value\" desc limit 1"
        );

        let mut player = String::from("");

        let str_t = &str_sql;
        let stmt = conn.prepare(str_t).unwrap();
        for row in stmt.query(&[]).unwrap() {
            player = row.get(0);
        }

        return player;
    }

    pub(crate) fn vote(&mut self, player_index: usize, game: usize) {
        let mut logged = false;
        if let Some(player) = &self.players[player_index] {
            for player_item in self.game.logged_players.iter() {
                if player.player_name == player_item.player_name {
                    logged = true;
                }
            }
        }

        if logged {
            if let Some(player) = &self.players[player_index] {
                let mut count = 0;

                for vote in self.game.voted1.iter() {
                    if vote == &player_index {
                        count += 1;
                    }
                }

                for vote in self.game.voted2.iter() {
                    if vote == &player_index {
                        count += 1;
                    }
                }

                for vote in self.game.voted3.iter() {
                    if vote == &player_index {
                        count += 1;
                    }
                }

                for vote in self.game.voted4.iter() {
                    if vote == &player_index {
                        count += 1;
                    }
                }

                for vote in self.game.voted5.iter() {
                    if vote == &player_index {
                        count += 1;
                    }
                }

                for vote in self.game.voted6.iter() {
                    if vote == &player_index {
                        count += 1;
                    }
                }

                for vote in self.game.voted7.iter() {
                    if vote == &player_index {
                        count += 1;
                    }
                }

                if count == 0 {
                    let mut voted_game = String::from("");
                    match game {
                        1 => {
                            self.game.voted1.push(player_index);
                            voted_game = String::from("Speed shots");
                        }
                        2 => {
                            self.game.voted2.push(player_index);
                            voted_game = String::from("Goalkeeper");
                        }
                        3 => {
                            self.game.voted3.push(player_index);
                            voted_game = String::from("Air goals");
                        }
                        4 => {
                            self.game.voted4.push(player_index);
                            voted_game = String::from("Air puck");
                        }
                        5 => {
                            self.game.voted5.push(player_index);
                            voted_game = String::from("Scorer");
                        }
                        6 => {
                            self.game.voted6.push(player_index);
                            voted_game = String::from("Precision");
                        }
                        7 => {
                            self.game.voted7.push(player_index);
                            voted_game = String::from("Passes");
                        }
                        _ => {}
                    }
                    self.add_server_chat_message(format!(
                        "{} voted for {}",
                        player.player_name, voted_game
                    ));
                } else {
                    self.add_directed_server_chat_message(
                        String::from("You can vote only one time"),
                        player_index,
                    );
                }
            }
        } else {
            self.add_directed_server_chat_message(String::from("Log in to vote"), player_index);
        }
    }

    pub(crate) fn login(&mut self, player_index: usize, password_user: &str) {
        let mut logged = false;
        if let Some(player) = &self.players[player_index] {
            for player_item in self.game.logged_players.iter() {
                if player.player_name == player_item.player_name {
                    logged = true;
                }
            }
        }

        if let Some(player) = &self.players[player_index] {
            for player_item in self.game.logged_players_for_next.iter() {
                if player.player_name == player_item.player_name {
                    logged = true;
                }
            }
        }

        if logged == false {
            if let Some(player) = &self.players[player_index] {
                let conn = Self::get_connection();

                let mut hasher = Md5::new();
                let pass_str = password_user.to_string();

                let bytes = pass_str.as_bytes();
                hasher.input(bytes);
                let mut output = [0; 16];
                hasher.result(&mut output);
                let mut result: String = "".to_string();
                for i in output.to_vec().iter() {
                    let t = format!("{:02X}", i);
                    result = format!("{}{}", result, t);
                }

                let str_sql = format!(
                    "SELECT count (*) FROM public.\"Users\" where \"Login\"='{}' and \"Password\"='{}'",
                    player.player_name, result
                );
                let str_t = &str_sql;
                let stmt = conn.prepare(str_t).unwrap();
                let mut next = false;

                let mut count: i64 = 0;

                for row in stmt.query(&[]).unwrap() {
                    count = row.get(0);
                }

                if count > 0 {
                    let player_item = RHQMPlayer {
                        player_i: player_index,
                        player_name: player.player_name.to_string(),
                        afk: false,
                    };

                    let str_sql_banned = format!("select checkban('{}')", player.player_name);

                    info!("{}", str_sql_banned);

                    let stmt_banned = conn.prepare(&str_sql_banned).unwrap();
                    let mut banned_count: i32 = 0;
                    for row in stmt_banned.query(&[]).unwrap() {
                        banned_count = row.get(0);
                    }

                    if banned_count > 0 {
                        self.add_directed_server_chat_message(
                            String::from("You are banned"),
                            player_index,
                        );
                    } else {
                        let name = player.player_name.to_string();

                        let mut toomuch = false;

                        if (self.game.logged_players.len()) < self.game.ranked_count {
                            self.game.logged_players.push(player_item);
                        } else {
                            next = true;

                            if self.game.logged_players_for_next.len() < self.game.ranked_count - 1
                            {
                                self.game.logged_players_for_next.push(player_item);
                            } else {
                                toomuch = true;
                                self.add_directed_server_chat_message(
                                    String::from("Too many logged in for next game players"),
                                    player_index,
                                );
                            }
                        }

                        if !toomuch {
                            self.user_logged_in(&name.to_owned(), next);
                        }
                    }
                } else {
                    self.add_directed_server_chat_message(
                        String::from("Wrong password"),
                        player_index,
                    );
                }
            }
        } else {
            self.add_directed_server_chat_message(
                String::from("You are already logged in"),
                player_index,
            );
        }
    }

    pub(crate) fn render_pucks(&mut self, puck_count: usize) {
        let puck_line_start = self.game.world.rink.width / 2.0 - 0.4 * ((10 - 1) as f32);

        self.game.world.puck_slots = puck_count;
        for i in 0..puck_count {
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
    }

    pub(crate) fn get_random_logged_player(&mut self) -> usize {
        let mut players: Vec<usize> = vec![];
        for player in self.game.logged_players.iter() {
            if !player.afk {
                players.push(player.player_i);
            }
        }

        let mut non_prev = false;
        let mut index = 0;

        let mut found_index = 999;

        while non_prev == false {
            let first: Vec<_> = players
                .choose_multiple(&mut rand::thread_rng(), 1)
                .collect();

            if first.len() != 0 {
                found_index = first[0].to_owned();

                if found_index != self.game.last_random_index {
                    non_prev = true;
                }
            }
            index += 1;
            if index == 4 {
                non_prev = true;
            }
        }

        self.game.last_random_index = found_index;

        return found_index;
    }

    pub(crate) fn new_world(&mut self) {
        let mut object_vec = Vec::with_capacity(32);
        for _ in 0..32 {
            object_vec.push(HQMGameObject::None);
        }
        let rink = HQMRink::new(30.0, 61.0, 8.5);
        self.game.world = HQMGameWorld {
            objects: object_vec,
            puck_slots: 1,
            rink,
            gravity: 0.000680555,
            limit_jump_speed: false,
        };

        self.config.spawn_point = HQMSpawnPoint::Center;
    }

    pub(crate) fn init_mini_game(&mut self) {
        self.force_players_off_ice_by_system();
        self.new_world();
        self.config.spawn_point = HQMSpawnPoint::Center;
        self.game.world.gravity = 0.000680555;
        self.game.wait_for_end = false;

        self.game.voted1 = vec![];
        self.game.voted2 = vec![];
        self.game.voted3 = vec![];
        self.game.voted4 = vec![];
        self.game.voted5 = vec![];
        self.game.voted6 = vec![];
        match self.game.last_mini_game {
            0 => {}
            1 => {}
            2 => {}
            3 => {}
            4 => {}
            5 => {}
            6 => {}
            _ => {}
        }
    }

    pub(crate) fn get_next_mini_game(&mut self) {
        let mut max_votes = 0;
        let mut max_votes_game = 0;

        if self.game.voted1.len() > max_votes {
            max_votes_game = 0;
            max_votes = self.game.voted1.len();
        }

        if self.game.voted2.len() > max_votes {
            max_votes_game = 1;
            max_votes = self.game.voted2.len();
        }

        if self.game.voted3.len() > max_votes {
            max_votes_game = 2;
            max_votes = self.game.voted3.len();
        }

        if self.game.voted4.len() > max_votes {
            max_votes_game = 3;
            max_votes = self.game.voted4.len();
        }

        if self.game.voted5.len() > max_votes {
            max_votes_game = 4;
            max_votes = self.game.voted5.len();
        }

        if self.game.voted6.len() > max_votes {
            max_votes_game = 5;
            max_votes = self.game.voted6.len();
        }

        if self.game.voted7.len() > max_votes {
            max_votes_game = 6;
            max_votes = self.game.voted7.len();
        }

        if max_votes == 0 {
            max_votes_game = rand::thread_rng().gen_range(0, 6);
        }

        self.game.last_mini_game = max_votes_game;

        let mut mini_game_name = String::from("");
        let mut mini_game_description = String::from("");
        match self.game.last_mini_game {
            0 => {
                self.game.mini_game_warmup = 500;
                let best = Self::get_mini_game_best_result();
                mini_game_name = format!("Speed shots ({})", best);
                mini_game_description = String::from("Score 8 goals in as less time as possible");
            }
            1 => {
                self.game.mini_game_warmup = 500;
                let best = Self::get_gk_mini_game_best_result();
                mini_game_name = format!("Goalkeeper ({})", best);
                mini_game_description = String::from("Save the most pucks");
            }
            2 => {
                self.game.mini_game_warmup = 500;
                let best = Self::get_catch_mini_game_best_result();
                mini_game_name = format!("Air goals ({})", best);
                mini_game_description = String::from("Score more air goals");
            }
            3 => {
                self.game.mini_game_warmup = 500;
                let best = Self::get_air_mini_game_best_result();
                mini_game_name = format!("Air puck ({})", best);
                mini_game_description =
                    String::from("Keep the puck in the air for as long as possible");
            }
            4 => {
                self.game.mini_game_warmup = 500;
                let best = Self::get_scorer_mini_game_best_result();
                mini_game_name = format!("Scorer ({})", best);
                mini_game_description = String::from("Score the most goals from the passes");
            }
            5 => {
                self.game.mini_game_warmup = 500;
                let best = Self::get_precision_mini_game_best_result();
                mini_game_name = format!("Precision ({})", best);
                mini_game_description = String::from("Shoot the pucks in the squares within 5s");
            }
            6 => {
                self.game.mini_game_warmup = 500;
                let best = Self::get_passes_mini_game_best_result();
                mini_game_name = format!("Passes ({})", best);
                mini_game_description = String::from("Pass the pucks in the squares within 5s");
            }
            // 1 => {
            //     self.game.mini_game_warmup = 500;

            //     if self.game.logged_players.len() >= 2 {
            //         mini_game_name = String::from("Shootouts");
            //         mini_game_description = String::from("Shootouts with random players");
            //     } else {
            //         self.game.last_mini_game += 1;
            //         self.get_next_mini_game();
            //     }
            // }
            // 2 => {
            //     if self.game.logged_players.len() >= 2 {
            //         mini_game_name = String::from("Touch counter");
            //         mini_game_description = String::from("Shoots");
            //     } else {
            //         self.game.last_mini_game += 1;
            //         self.get_next_mini_game();
            //     }
            // }
            _ => {
                self.game.last_mini_game = 0;
                self.get_next_mini_game();
            }
        }

        if mini_game_name.len() != 0 {
            self.add_server_chat_message(format!("Next mini-game: {}", mini_game_name));
            self.add_server_chat_message(format!("Description: {}", mini_game_description));
        }
    }

    pub(crate) fn save_data(&mut self) {
        let conn = Self::get_connection();

        let mut sum_red = 0;
        let mut sum_blue = 0;

        for i in self.game.game_players.iter() {
            if i.player_team == 0 {
                sum_red += i.player_points;
            } else {
                sum_blue += i.player_points;
            }
        }

        let avg_red = sum_red / (self.game.ranked_count / 2);
        let avg_blue = sum_blue / (self.game.ranked_count / 2);

        let max = 30;
        let min = 5;

        let mut max_points = 0;
        let mut max_name = String::from("");

        for i in self.game.game_players.iter() {
            match i {
                RHQMGamePlayer {
                    player_i_r: _,
                    player_name_r,
                    player_points,
                    player_team,
                    goals,
                    assists,
                    leaved_seconds,
                } => {
                    let mut win_div = 10;
                    let mut lose_div = 10;

                    if player_team == &0 {
                        let mut val =
                            isize::abs(player_points.to_owned() as isize - avg_red as isize);

                        if player_points.to_owned() as isize - max as isize > avg_red as isize {
                            val = max as isize;
                        }

                        if (player_points.to_owned() as isize + max as isize) < avg_red as isize {
                            val = max as isize;
                        }

                        win_div = max - val;
                        lose_div = val;
                    } else {
                        let mut val =
                            isize::abs(player_points.to_owned() as isize - avg_blue as isize);
                        if player_points.to_owned() as isize - max as isize > avg_blue as isize {
                            val = max as isize;
                        }

                        if (player_points.to_owned() as isize + max as isize) < avg_blue as isize {
                            val = min as isize;
                        }
                        win_div = max - val;
                        lose_div = val;
                    }

                    let mut points = 0;

                    if player_team == &0 {
                        if self.game.red_score > self.game.blue_score {
                            points = win_div as isize + self.game.red_score as isize
                                - self.game.blue_score as isize;
                        } else {
                            points = -1 as isize * lose_div as isize - self.game.blue_score as isize
                                + self.game.red_score as isize
                        }
                    } else {
                        if self.game.blue_score > self.game.red_score {
                            points = win_div as isize + self.game.blue_score as isize
                                - self.game.red_score as isize;
                        } else {
                            points = -1 as isize * lose_div as isize - self.game.red_score as isize
                                + self.game.blue_score as isize
                        }
                    }

                    if goals + assists >= max_points {
                        max_name = player_name_r.to_owned();
                        max_points = goals + assists;
                    }

                    let mut leaved = false;
                    if leaved_seconds == &0 {
                        leaved = true;
                        points = -30;
                    }

                    let str_sql_player = format!(
                        "insert into public.\"GameStats\" values((select max(\"Id\")+1 from public.\"GameStats\"), (select max(\"Id\")+1 from public.\"Stats\"), (select \"Id\" from public.\"Users\" where \"Login\"='{}'), {}, {}, {}, {}, {} )",
                        player_name_r,
                        player_team,
                        goals,
                        assists,
                        points,
                        leaved
                    );
                    conn.execute(&str_sql_player, &[]).unwrap();
                }
            }
        }

        let str_sql = format!(
            "insert into public.\"Stats\" values((select max(\"Id\")+1 from public.\"Stats\"), (select max(\"Season\") from public.\"Stats\"), {},{},NOW(), (select \"Id\" from public.\"Users\" where \"Login\"='{}'))",
            self.game.red_score,
            self.game.blue_score,
            max_name
        );
        conn.execute(&str_sql, &[]).unwrap();

        self.add_server_chat_message(format!(
            "{} {}",
            String::from("Ranked game ended I MVP: "),
            max_name
        ));
    }

    pub fn get_connection() -> postgres::Connection {
        let conn = Connection::connect(
            "postgresql://server:mjmfkiuj@212.193.53.255:5432/euranked",
            &SslMode::None,
        )
        .unwrap();

        return conn;
    }
}
