/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
use serde::{Deserialize, Serialize};
use jid::FullJid;
use secrecy::SecretString;

/// Uniquely identify an account inside Aparté
pub type Account = FullJid;

pub type Password = SecretString;

fn false_() -> bool {
    false
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ConnectionInfo {
    pub jid: String,
    pub server: Option<String>,
    pub port: Option<u16>,
    #[serde(default = "false_")]
    pub autoconnect: bool,
    #[serde(skip_serializing)]
    pub password: Option<Password>,
}