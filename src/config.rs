/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
 use serde::{Deserialize, Serialize};
 use std::collections::HashMap;
 
 use crate::account::ConnectionInfo;
 
 #[derive(Debug, Clone, Serialize, Deserialize, Default)]
 #[serde(default)]
 pub struct Config {
     pub accounts: HashMap<String, ConnectionInfo>,
 }
 