// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

use std::{path::Path, process::Command};

pub fn isolated_install(repo: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lattice"));
    command
        .current_dir(repo)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env(
            "LATTICE_MODEL_DIR",
            lattice::embed::ensure_model_present()
                .expect("cache MiniLM before running offline tests"),
        );
    command
}
